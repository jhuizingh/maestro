//! Tracker notes: split into entries, find the pending DECISION, parse options.
use crate::model::Decision;
use regex::Regex;
use std::sync::LazyLock;

static HEAD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_-]*(?: [A-Z0-9_-]+)*(?: \([^)]*\))?:").unwrap());
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2})?Z?)?").unwrap());
static REC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:rec|recommendation|recommend)\s*[:\-]\s*(.+)$").unwrap()
});
static LINE_OPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:\d+[.)]|[-*])\s+(.+)$").unwrap());
static INLINE_OPT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+)\)").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub kind: String,
    pub date: Option<String>,
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
            out.push(Note {
                kind,
                date,
                text: line.to_string(),
            });
        } else if let Some(last) = out.last_mut() {
            last.text.push('\n');
            last.text.push_str(line);
        } else if !line.trim().is_empty() {
            out.push(Note {
                kind: "NOTE".into(),
                date: None,
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
    }
}

/// The decision still waiting on the user: the latest DECISION, unless an ANSWER came after it.
/// `None` when answered; `Some(None)` when nothing was ever asked or answered.
pub fn pending_decision(notes: &str) -> Option<Option<Decision>> {
    match split_notes(notes)
        .into_iter()
        .rev()
        .find(|n| n.kind == "DECISION" || n.kind == "ANSWER")
    {
        Some(n) if n.kind == "ANSWER" => None,
        Some(n) => Some(Some(parse_decision(&n.text))),
        None => Some(None),
    }
}

pub fn answer_note(ts: &str, text: &str) -> String {
    format!("ANSWER (fermata, {ts}): {text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES: &str = "DECISION (2026-11-12): first? Rec: no.\nmore detail\nANSWER (fermata, 2026-11-12T10:00Z): yes\nDECISION (2026-11-12 sweep): Merge #4591 now? (1) merge and finish, (2) keep open, (3) wait. Rec: (1)";

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
        let d = pending_decision(NOTES).flatten().unwrap();
        assert!(d.text.starts_with("Merge #4591"));
        assert_eq!(d.options, vec!["merge and finish", "keep open", "wait"]);
        assert_eq!(d.recommendation.as_deref(), Some("(1)"));
    }

    #[test]
    fn piped_format() {
        let d = pending_decision(
            "DECISION (2026-11-12T19:40Z): Merge #4591 now? | 1) merge + finish | 2) keep open | 3) wait | Rec: 2",
        )
        .flatten()
        .unwrap();
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
        assert!(pending_decision("plain note").flatten().is_none());
    }

    #[test]
    fn answered_decision_is_not_pending() {
        let asked = "DECISION (2026-11-12): ship? Rec: yes";
        let d = pending_decision(asked).unwrap().unwrap();
        assert!(d.text.starts_with("ship?"));
        let answered =
            format!("{asked}\nANSWER (fermata, 2026-11-12T10:00Z): yes\nNOTE: unrelated");
        assert!(pending_decision(&answered).is_none());
        // A new question after the answer is pending again.
        let d = pending_decision(&format!("{answered}\nDECISION (2026-11-13): and now?"))
            .unwrap()
            .unwrap();
        assert!(d.text.starts_with("and now?"));
        assert_eq!(pending_decision("plain note"), Some(None));
        assert!(pending_decision("ANSWER (fermata, x): go").is_none());
    }

    #[test]
    fn untagged_first_line() {
        let n = split_notes("hello\nworld");
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].kind, "NOTE");
    }
}
