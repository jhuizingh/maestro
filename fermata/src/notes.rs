//! Tracker notes: split into entries, find the pending DECISIONs, parse options.
use crate::model::{Decision, DecisionStatus, Task};
use regex::Regex;
use std::sync::LazyLock;

static HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_-]*(?: [A-Z0-9_-]+)*(?: \([^)]*\))?:").unwrap());
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2})?Z?)?").unwrap());
/// A hand-typed stamp with no date (`14:02Z`), used only when the head has no full date.
static TIME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{1,2}:\d{2}(?::\d{2})?Z?").unwrap());
static REC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:rec|recommendation|recommend)\s*[:\-]\s*(.+)$").unwrap()
});
static LINE_OPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:\d+[.)]|[-*])\s+(.+)$").unwrap());
static INLINE_OPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+)\)").unwrap());
/// What an ANSWER's head says it answers: nothing, a legacy decision's key, or its stamp.
static ANSWER_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\bre (none|n[0-9a-f]{10}",
        r"|\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2})?Z?)?",
        r"|\d{1,2}:\d{2}(?::\d{2})?Z?)[,)]"
    ))
    .unwrap()
});

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub kind: String,
    pub date: Option<String>,
    /// A time-only stamp (`14:02Z`) when the head has no date: when it was written is unknown.
    pub time: Option<String>,
    pub text: String,
}

/// Split the notes blob into entries. A line like `WORD (…):` starts a new entry.
pub fn split_notes(notes: &str) -> Vec<Note> {
    let mut out: Vec<Note> = vec![];
    for line in notes.lines() {
        if let Some(m) = HEAD.find(line) {
            let head = m.as_str();
            let kind = head.split([' ', '(', ':']).next().unwrap_or("").to_string();
            let date = DATE.find(head).map(|d| d.as_str().to_string());
            let time = date
                .is_none()
                .then(|| TIME.find(head).map(|t| t.as_str().to_string()))
                .flatten();
            out.push(Note {
                kind,
                date,
                time,
                text: line.to_string(),
            });
        } else if let Some(last) = out.last_mut() {
            last.text.push('\n');
            last.text.push_str(line);
        } else if !line.trim().is_empty() {
            out.push(Note {
                kind: "NOTE".into(),
                date: None,
                time: None,
                text: line.to_string(),
            });
        }
    }
    for n in &mut out {
        n.text = n.text.trim_end().to_string();
    }
    out
}

static PIPE_OPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)[.)]\s*(.+)$").unwrap());
static PIPE_REC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(?:rec|recommendation)\s*:\s*(.+)$").unwrap());

/// The orchestrator's format: `<question> | 1) <opt> | 2) <opt> | Rec: <n or text>`.
fn parse_piped(body: &str) -> Option<Decision> {
    let parts: Vec<&str> = body.split(" | ").map(str::trim).collect();
    if parts.len() < 2 || !parts[1..].iter().any(|p| PIPE_OPT.is_match(p)) {
        return None;
    }
    let mut d = Decision {
        text: parts[0].to_string(),
        ..Default::default()
    };
    for p in &parts[1..] {
        if let Some(c) = PIPE_OPT.captures(p) {
            d.options.push(c[2].trim().to_string());
        } else if let Some(c) = PIPE_REC.captures(p) {
            let r = c[1].trim().trim_end_matches(')').to_string();
            d.recommendation = Some(
                match r.parse::<usize>().ok().and_then(|n| {
                    d.options
                        .get(n.wrapping_sub(1))
                        .map(|o| format!("{n}) {o}"))
                }) {
                    Some(x) => x,
                    None => c[1].trim().to_string(),
                },
            );
        } else {
            d.text.push_str(" | ");
            d.text.push_str(p);
        }
    }
    Some(d)
}

/// The heads that say where a task stands; WAITING and FINDING are optional conventions.
const STANDING: &[&str] = &["DECISION", "WAITING", "FINDING"];

/// The newest DECISION, WAITING or FINDING note dated on or after `since`, as one line: what
/// an idle task is waiting on. Undated notes count; `since` is a date or timestamp.
pub fn standing(notes: &str, since: Option<&str>) -> Option<String> {
    let day = |d: &str| d.get(..10).unwrap_or(d).to_string();
    let n = split_notes(notes).into_iter().rev().find(|n| {
        STANDING.contains(&n.kind.as_str())
            && match (n.date.as_deref(), since) {
                (Some(d), Some(s)) => day(d) >= day(s),
                _ => true,
            }
    })?;
    let first = n.text.lines().next().unwrap_or_default();
    let body = HEAD.find(first).map_or(first, |m| &first[m.end()..]).trim();
    if body.is_empty() {
        return None;
    }
    Some(match n.kind.as_str() {
        "WAITING" if body.starts_with("on ") => format!("waiting {body}"),
        "WAITING" => format!("waiting on: {body}"),
        k => format!("{}: {body}", k.to_lowercase()),
    })
}

pub fn parse_decision(text: &str) -> Decision {
    if let Some(d) = HEAD
        .find(text)
        .and_then(|m| parse_piped(text[m.end()..].trim()))
    {
        return d;
    }
    let body = HEAD
        .find(text)
        .map(|m| text[m.end()..].trim())
        .unwrap_or(text)
        .to_string();
    let mut options: Vec<String> = text
        .lines()
        .skip(1)
        .filter_map(|l| LINE_OPT.captures(l).map(|c| c[1].trim().to_string()))
        .collect();
    if options.is_empty() {
        let cut = REC.find(&body).map(|m| m.start()).unwrap_or(body.len());
        let src = &body[..cut];
        let marks: Vec<_> = INLINE_OPT.find_iter(src).collect();
        if marks.len() >= 2 {
            for (i, m) in marks.iter().enumerate() {
                let end = marks.get(i + 1).map(|n| n.start()).unwrap_or(src.len());
                let mut opt = src[m.end()..end].trim().to_string();
                if i + 1 == marks.len()
                    && let Some(p) = opt.find(". ")
                {
                    opt.truncate(p);
                }
                options.push(opt.trim_end_matches([',', ';', ' ', '.']).to_string());
            }
        }
    }
    let recommendation = body
        .lines()
        .rev()
        .find_map(|l| REC.captures(l).map(|c| c[1].trim().to_string()));
    Decision {
        text: body,
        options,
        recommendation,
        ..Default::default()
    }
}

/// Which DECISION an ANSWER is for, from its head.
#[derive(Debug, PartialEq)]
enum Ref {
    /// `re none`: input that answers no decision.
    Nothing,
    /// `re <key>` or `re <stamp>`.
    Named(String),
    /// No `re`: the DECISION just before it.
    Unnamed,
}

fn answer_ref(n: &Note) -> Ref {
    let head = HEAD.find(&n.text).map_or("", |m| m.as_str());
    match ANSWER_REF.captures_iter(head).last() {
        Some(c) if &c[1] == "none" => Ref::Nothing,
        Some(c) => Ref::Named(c[1].to_string()),
        None => Ref::Unnamed,
    }
}

/// A legacy decision's key: FNV-1a of its body, whitespace aside. Written into ANSWER notes, so it
/// must never change; a question re-posted word for word keeps its key.
fn note_key(text: &str) -> String {
    let body = &text[HEAD.find(text).map_or(0, |m| m.end())..];
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in crate::util::one_line(body).bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("n{:010x}", h >> 24)
}

fn legacy_decision(n: &Note) -> Decision {
    Decision {
        note: Some(note_key(&n.text)),
        ..parse_decision(&n.text)
    }
}

/// Whether `r` (`19:11Z`, `2026-11-25T19:11Z`) is the stamp in a DECISION's head, or its time.
fn stamped(n: &Note, r: &str) -> bool {
    let Some(stamp) = n.date.as_deref().or(n.time.as_deref()) else {
        return false;
    };
    stamp == r
        || stamp
            .strip_suffix(r)
            .is_some_and(|p| p.ends_with(['T', ' ']))
}

/// Every legacy decision still waiting on the user, oldest first; see "Legacy decisions".
/// `None` when nothing was ever asked or answered.
pub fn pending_decisions(notes: &str) -> Option<Vec<Decision>> {
    let mut open: Vec<(Note, Decision)> = vec![];
    // The latest DECISION's key, until any ANSWER follows it.
    let mut just_asked: Option<String> = None;
    let mut any = false;
    for n in split_notes(notes) {
        match n.kind.as_str() {
            "DECISION" => {
                any = true;
                let d = legacy_decision(&n);
                open.retain(|(_, o)| o.note != d.note);
                just_asked = d.note.clone();
                open.push((n, d));
            }
            "ANSWER" => {
                any = true;
                match answer_ref(&n) {
                    Ref::Nothing => continue,
                    Ref::Named(r) => {
                        let hits: Vec<_> = open
                            .iter()
                            .filter(|(o, d)| d.note.as_deref() == Some(&*r) || stamped(o, &r))
                            .map(|(_, d)| d.note.clone())
                            .collect();
                        // A stamp two questions share names neither.
                        if let [k] = &hits[..] {
                            open.retain(|(_, d)| d.note != *k);
                        }
                    }
                    Ref::Unnamed => {
                        if just_asked.is_some() {
                            open.retain(|(_, d)| d.note != just_asked);
                        }
                    }
                }
                just_asked = None;
            }
            _ => {}
        }
    }
    any.then(|| open.into_iter().map(|(_, d)| d).collect())
}

/// Legacy `DECISION`s posted without the `needs-decision` label: shown, marked, only while they
/// are the last notes of all (any other note after them is taken to have moved on).
pub fn unlabelled_decisions(notes: &str) -> Vec<Decision> {
    let mut out: Vec<Decision> = vec![];
    for n in split_notes(notes) {
        if n.kind == "DECISION" {
            let d = legacy_decision(&n);
            out.retain(|o| o.note != d.note);
            out.push(Decision {
                label_missing: true,
                ..d
            });
        } else {
            out.clear();
        }
    }
    out
}

/// A `needs-decision` task's unresolved decisions: the tracker's, then the pending legacy
/// DECISION notes. A label with neither (or an unread tracker) still shows, as a placeholder.
pub fn task_decisions(task: &Task) -> Vec<Decision> {
    if !task.has_label("needs-decision") {
        return unlabelled_decisions(&task.notes);
    }
    let mut out: Vec<Decision> = task
        .decisions
        .iter()
        .flatten()
        .filter(|d| d.status != DecisionStatus::Resolved)
        .cloned()
        .collect();
    match pending_decisions(&task.notes) {
        Some(ds) => out.extend(ds),
        None if out.is_empty() => out.push(Decision {
            text: match task.decisions {
                Some(_) => "(labelled needs-decision; no open decision)".into(),
                None => "(labelled needs-decision; decisions could not be read)".into(),
            },
            ..Default::default()
        }),
        _ => {}
    }
    out
}

/// `outcome` is what routing did with it (`delivered to o:1.2`, `note only`, …); no `)`.
/// `re` is the key of the legacy decision it answers, if it answers one.
pub fn answer_note(ts: &str, outcome: &str, re: Option<&str>, text: &str) -> String {
    let re = re.unwrap_or("none");
    format!("ANSWER (fermata, {ts}, {outcome}, re {re}): {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES: &str = "DECISION (2026-11-12): first? Rec: no.\nmore detail\nANSWER (fermata, 2026-11-12T10:00Z): yes\nDECISION (2026-11-12 sweep): Merge #4591 now? (1) merge and finish, (2) keep open, (3) wait. Rec: (1)";

    #[test]
    fn standing_is_the_newest_since_the_session_began() {
        let n = "WAITING (2026-11-20T09:00Z): on the old thing.\n\
                 FINDING (2026-11-25T10:00Z): PR 7 at abc: green. Evidence follows\nsecond line\n\
                 NOTE (2026-11-25T11:00Z): not a standing note";
        let s = |since| standing(n, since);
        assert_eq!(
            s(Some("2026-11-25T09:00:00Z")).as_deref(),
            Some("finding: PR 7 at abc: green. Evidence follows")
        );
        assert_eq!(s(Some("2026-11-26T00:00:00Z")), None);
        let w = "WAITING (2026-11-25): revert commit X or hand the branch over";
        assert_eq!(
            standing(w, None).as_deref(),
            Some("waiting on: revert commit X or hand the branch over")
        );
        assert_eq!(
            standing("WAITING (14:02Z): on Sam's push to #9.", Some("2026-11-25")).as_deref(),
            Some("waiting on Sam's push to #9.")
        );
        assert_eq!(standing("just prose", None), None);
    }

    fn texts(notes: &str) -> Vec<String> {
        let ds = pending_decisions(notes).unwrap_or_default();
        ds.into_iter().map(|d| d.text).collect()
    }

    fn only(notes: &str) -> Decision {
        let mut ds = pending_decisions(notes).unwrap();
        assert_eq!(ds.len(), 1, "{ds:?}");
        ds.remove(0)
    }

    fn d_key(note: &str) -> Option<String> {
        only(note).note
    }

    #[test]
    fn splits() {
        let n = split_notes(NOTES);
        assert_eq!(n.len(), 3);
        assert_eq!(n[0].kind, "DECISION");
        assert!(n[0].text.ends_with("more detail"));
        assert_eq!(n[1].kind, "ANSWER");
        assert_eq!(n[1].date.as_deref(), Some("2026-11-12T10:00Z"));
    }

    #[test]
    fn latest_and_options() {
        let d = only(NOTES);
        assert!(d.text.starts_with("Merge #4591"));
        assert_eq!(d.options, vec!["merge and finish", "keep open", "wait"]);
        assert_eq!(d.recommendation.as_deref(), Some("(1)"));
    }

    #[test]
    fn piped_format() {
        let d = only(
            "DECISION (2026-11-12T19:40Z): Merge #4591 now? | 1) merge + finish | 2) keep open | 3) wait | Rec: 2",
        );
        assert_eq!(d.text, "Merge #4591 now?");
        assert_eq!(d.options, vec!["merge + finish", "keep open", "wait"]);
        assert_eq!(d.recommendation.as_deref(), Some("2) keep open"));
        let d = parse_decision("DECISION (x): Q? | 1) a | 2) b | Rec: b, because cheaper");
        assert_eq!(d.recommendation.as_deref(), Some("b, because cheaper"));
        // Pipes without numbered options fall back to the old parser.
        let d = parse_decision("DECISION (x): a | b. Rec: a");
        assert!(d.options.is_empty());
        assert_eq!(d.recommendation.as_deref(), Some("a"));
    }

    #[test]
    fn line_options() {
        let d = parse_decision("DECISION (x): pick\n1. alpha\n2) beta\n- gamma");
        assert_eq!(d.options, vec!["alpha", "beta", "gamma"]);
        assert!(pending_decisions("plain note").is_none());
    }

    #[test]
    fn answered_decision_is_not_pending() {
        let asked = "DECISION (2026-11-12): ship? Rec: yes";
        let d = only(asked);
        assert!(d.text.starts_with("ship?"));
        let answered =
            format!("{asked}\nANSWER (fermata, 2026-11-12T10:00Z): yes\nNOTE: unrelated");
        assert!(texts(&answered).is_empty());
        // A new question after the answer is pending again.
        let d = only(&format!("{answered}\nDECISION (2026-11-13): and now?"));
        assert!(d.text.starts_with("and now?"));
        assert!(
            pending_decisions("ANSWER (fermata, x): go")
                .unwrap()
                .is_empty()
        );
        // The head with a delivery outcome still parses as an ANSWER.
        let n = format!(
            "{asked}\n{}",
            answer_note(
                "2026-11-12T10:00Z",
                "held for a:1.1",
                d_key(asked).as_deref(),
                "y"
            )
        );
        assert!(texts(&n).is_empty());
    }

    #[test]
    fn time_only_stamp() {
        let n =
            split_notes("DECISION (14:02Z): go?\nNOTE (2026-11-12 09:00Z): x\nANSWER (sweep): y");
        assert_eq!(
            (n[0].date.as_deref(), n[0].time.as_deref()),
            (None, Some("14:02Z"))
        );
        assert_eq!(
            (n[1].date.as_deref(), n[1].time.as_deref()),
            (Some("2026-11-12 09:00Z"), None)
        );
        assert_eq!((n[2].date.as_deref(), n[2].time.as_deref()), (None, None));
    }

    #[test]
    fn unlabelled_trailing() {
        let ds =
            unlabelled_decisions("NOTE: a\nDECISION (14:02Z): ship? | 1) yes | 2) no | Rec: 1");
        assert!(ds.len() == 1 && ds[0].label_missing);
        assert_eq!(ds[0].options, vec!["yes", "no"]);
        // One question does not take another out.
        let two = unlabelled_decisions("DECISION (x): ship?\nDECISION (y): merge?");
        assert_eq!(two.len(), 2);
        assert!(unlabelled_decisions("DECISION (x): ship?\nNOTE: moved on").is_empty());
        assert!(unlabelled_decisions("DECISION (x): ship?\nANSWER (y): yes").is_empty());
        assert!(unlabelled_decisions("").is_empty());
    }

    /// The sequence that hid a question: Q1 and Q2 in flight, then Q2 and Q3 answered (each twice
    /// over: fermata's note and the orchestrator's).
    #[test]
    fn an_answer_never_hides_another_question() {
        let mut n = "DECISION (19:11Z): Q1 post it? | 1) yes | 2) no".to_string();
        let mut add = |line: &str| {
            n.push('\n');
            n.push_str(line);
            texts(&n)
        };
        let (q1, q2, q3) = ("Q1 post it?", "Q2 bump?", "Q3 ready?");
        // Posing the second hides nothing.
        assert_eq!(
            add("DECISION (19:18Z): Q2 bump? | 1) yes | 2) no"),
            [q1, q2]
        );
        assert_eq!(
            add("ANSWER (fermata, 19:23Z, queued for work:1.1): yes"),
            [q1]
        );
        // A second ANSWER to the same question does not reach back to the first one.
        assert_eq!(add("ANSWER (user, 19:23Z): yes"), [q1]);
        assert_eq!(
            add("DECISION (19:23Z): Q3 ready? | 1) yes | 2) no"),
            [q1, q3]
        );
        assert_eq!(
            add("ANSWER (fermata, 19:26Z, delivered to work:1.1): y"),
            [q1]
        );
        assert_eq!(add("ANSWER (fermata, 19:26Z): y"), [q1]);
        // Re-posted word for word: one row, not two; its answer settles both.
        assert_eq!(add("DECISION (19:29Z): Q1 post it? | 1) yes | 2) no"), [q1]);
        assert!(add("ANSWER (user, 19:30Z): yes").is_empty());
    }

    #[test]
    fn a_named_answer_closes_only_its_question() {
        let n = "DECISION (2026-11-25T19:11Z): one?\nDECISION (19:18Z): two?";
        let key = |i: usize| pending_decisions(n).unwrap()[i].note.clone();
        let after = |re: Option<&str>, more: &str| {
            texts(&format!(
                "{n}\n{}{more}",
                answer_note("t", "note only", re, "y")
            ))
        };
        assert_eq!(after(key(0).as_deref(), ""), ["two?"]);
        assert_eq!(after(key(1).as_deref(), ""), ["one?"]);
        // An unnamed copy of that answer does not close the other.
        assert_eq!(
            after(key(0).as_deref(), "\nANSWER (user, 19:30Z): y"),
            ["two?"]
        );
        // Input that answers nothing, or names a question that is gone, closes nothing.
        assert_eq!(after(None, "").len(), 2);
        assert_eq!(after(Some("n0000000000"), "").len(), 2);
        // Answered, then asked again in the same words: waiting again.
        let again = after(key(0).as_deref(), "\nDECISION (20:00Z): one?");
        assert_eq!(again, ["two?", "one?"]);
        // By hand, by the question's stamp.
        let by = |head: &str| texts(&format!("{n}\nANSWER ({head}): y"));
        assert_eq!(by("user, 19:30Z, re 19:11Z"), ["two?"]);
        assert_eq!(by("user, re 2026-11-25T19:11Z"), ["two?"]);
        assert_eq!(by("user, re 19:18Z"), ["one?"]);
        assert_eq!(by("user, re 9:18Z").len(), 2);
        // Anything else after `re` is prose.
        assert_eq!(by("user, re the pin"), ["one?"]);
        // Two questions with one stamp: neither.
        let same = "DECISION (19:11Z): a?\nDECISION (19:11Z): b?\nANSWER (user, re 19:11Z): y";
        assert_eq!(texts(same).len(), 2);
    }

    /// Keys are written into notes, so they outlive the binary.
    #[test]
    fn note_keys_are_pinned() {
        assert_eq!(d_key("DECISION (x): ship?").as_deref(), Some("n294a904913"));
        assert_eq!(
            d_key("DECISION (y):  ship?\n"),
            d_key("DECISION (x): ship?")
        );
        assert_ne!(d_key("DECISION (x): merge?"), d_key("DECISION (x): ship?"));
    }

    #[test]
    fn tracker_decisions_ignore_notes() {
        let seam = |id: &str, st| Decision {
            id: Some(id.into()),
            text: format!("q {id}"),
            status: st,
            ..Default::default()
        };
        let mut t = Task {
            labels: vec!["needs-decision".into()],
            notes: "ANSWER (fermata, 2026-11-12T10:00Z): created follow-ups".into(),
            decisions: Some(vec![
                seam("d1", DecisionStatus::Open),
                seam("d2", DecisionStatus::Answered),
                seam("d3", DecisionStatus::Resolved),
            ]),
            ..Default::default()
        };
        let keys = |t: &Task| {
            task_decisions(t)
                .iter()
                .map(|d| d.key().to_string())
                .collect::<Vec<_>>()
        };
        // An ANSWER-shaped note hides nothing; resolved ones are gone.
        assert_eq!(keys(&t), ["d1", "d2"]);
        // A pending legacy note shows beside them.
        t.notes
            .push_str("\nDECISION (2026-11-13): old style? Rec: yes");
        let legacy = d_key("DECISION (x): old style? Rec: yes").unwrap();
        assert_eq!(keys(&t), ["d1", "d2", legacy.as_str()]);
        // Unread decisions and no legacy note: the label alone still shows.
        t.notes.clear();
        t.decisions = None;
        assert!(task_decisions(&t)[0].text.contains("could not be read"));
        t.labels.clear();
        assert!(task_decisions(&t).is_empty());
    }

    #[test]
    fn untagged_first_line() {
        let n = split_notes("hello\nworld");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, "NOTE");
    }
}
