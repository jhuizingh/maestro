//! One task's conversation, cut out of every session that talked about it: turns, decisions,
//! workers, notes and unattributed activity in time order. See "Conversation" in DESIGN.md.
use crate::attribution::{reply_for, segments};
use crate::config::Context;
use crate::core::Loader;
use crate::history::{note_items, session_transcript, sort_key};
use crate::model::{Decision, JoinedTask, Session};
use crate::sessions::same_path;
use crate::transcript::{Entry, Kind, Link, cap, open_tail, parse_line};
use crate::turns::{self, Part, Run, Turn};
use crate::util::{one_line, parse_ts, truncate};
use serde_json::Value;
use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;

/// The most read from each worker or worktree-session transcript.
const TAIL: u64 = 4 * 1024 * 1024;

/// What a task is called in prompts, headings and writes.
pub fn names(t: &JoinedTask) -> Vec<&str> {
    std::iter::once(t.task.id.as_str())
        .chain(t.ticket_key.as_deref())
        .collect()
}

/// A `[fermata <task> <decision> <kind>]` message already shown.
type Shown = HashSet<(String, String)>;

/// An entry and the key it sorts by.
type Keyed = (String, Entry);

fn key(ts: &str) -> String {
    match parse_ts(ts) {
        Some(t) => t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        None => ts.to_string(),
    }
}

fn entry(kind: Kind, label: &str, ts: Option<String>, body: String) -> Entry {
    Entry {
        kind,
        ts,
        body,
        label: Some(label.into()),
        ..Default::default()
    }
}

fn link(mut e: Entry, path: &str) -> Entry {
    e.link = Some(Link {
        path: path.into(),
        ts: e.ts.clone(),
    });
    e
}

/// Key a source's entries in order; one without a time stays after the one before it.
fn keyed(entries: Vec<Entry>, from: &str) -> Vec<Keyed> {
    let mut last = key(from);
    entries
        .into_iter()
        .map(|e| {
            if let Some(t) = &e.ts {
                last = key(t);
            }
            (last.clone(), e)
        })
        .collect()
}

/// Runs of tool calls and results become one collapsed entry.
fn collapse(entries: Vec<Entry>, label: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = vec![];
    let mut group: Vec<Entry> = vec![];
    let flush = |group: &mut Vec<Entry>, out: &mut Vec<Entry>| {
        let Some(first) = group.first() else { return };
        let calls: Vec<&Entry> = group.iter().filter(|e| e.kind == Kind::Tool).collect();
        let n = calls.len();
        let last = calls.last().map(|e| e.head.as_str()).unwrap_or("result");
        let plural = if n == 1 { "" } else { "s" };
        let body: Vec<String> = group
            .iter()
            .map(|e| format!("{}\n{}", e.head, truncate(&e.body, 1000)))
            .collect();
        out.push(Entry {
            kind: Kind::Tool,
            ts: first.ts.clone(),
            head: format!("{n} tool call{plural} — {}", truncate(last, 100)),
            body: cap(&body.join("\n\n")),
            label: Some(format!("⚙ {label}")),
            link: first.link.clone(),
            folded: true,
        });
        group.clear();
    };
    for e in entries {
        if matches!(e.kind, Kind::Tool | Kind::ToolResult) {
            group.push(e);
        } else {
            flush(&mut group, &mut out);
            out.push(e);
        }
    }
    flush(&mut group, &mut out);
    out
}

/// A warning line: something that should be here could not be read.
fn gap(ts: Option<String>, path: &str, what: String) -> Entry {
    link(entry(Kind::Assistant, "⚠ incomplete", ts, what), path)
}

/// One turn as this task sees it; empty when it has nothing for the task.
fn turn_entries(run: &Run, names: &[&str], shown: &mut Shown) -> Vec<Entry> {
    let t = &run.turn;
    let prompted = t.prompted_for(names);
    let mut out = vec![];
    let mut open = vec![];
    for p in &run.parts {
        match p {
            Part::Prompt(ts, text) => {
                open.clear();
                for s in segments(text) {
                    if !s.task.as_deref().is_some_and(|x| names.contains(&x)) {
                        continue;
                    }
                    let label = match s.tag.as_str() {
                        "" => "▶ you".to_string(),
                        tag => format!("▶ you ({tag})"),
                    };
                    shown.insert((s.tag, s.text.clone()));
                    out.push(entry(Kind::User, &label, ts.clone(), s.text));
                }
            }
            Part::Reply(ts, text) => {
                if let Some(x) = reply_for(text, names, prompted, &mut open) {
                    out.push(entry(Kind::Assistant, "◀ orchestrator", ts.clone(), x));
                }
            }
            Part::Tool(e) if prompted => out.push(e.clone()),
            Part::Tool(_) => {}
        }
    }
    let out = out.into_iter().map(|e| link(e, &t.transcript)).collect();
    let mut out = collapse(out, "orchestrator");
    let what = t.unattributed(names);
    if !what.is_empty() {
        let e = entry(
            Kind::Assistant,
            &format!("? {}", t.unattributed_title()),
            Some(t.started.clone()),
            what.join(", "),
        );
        out.push(link(e, &t.transcript));
    }
    out
}

fn turns_part(turns: &[Turn], names: &[&str], shown: &mut Shown) -> Vec<Keyed> {
    let mut out = vec![];
    let mut seen = HashSet::new();
    // Recorded turns first: a running one's scan can cover them again.
    let (running, done): (Vec<&Turn>, Vec<&Turn>) = turns.iter().partition(|t| t.is_open());
    let about = |t: &Turn| t.attributed(names) || !t.unattributed(names).is_empty();
    let sessions: HashSet<&str> = turns
        .iter()
        .filter(|t| about(t))
        .map(|t| t.session.as_str())
        .collect();
    for t in done.into_iter().chain(running) {
        let at = Some(t.started.clone());
        if !t.is_open() && !t.attributed(names) {
            // A session that talks about this task may have done so in what is missing.
            if let Some(why) = &t.error
                && sessions.contains(t.session.as_str())
            {
                let what = format!("a turn that may be about this task: {why}");
                out.extend(keyed(
                    vec![gap(at.clone(), &t.transcript, what)],
                    &t.started,
                ));
            }
            if t.unattributed(names).is_empty() {
                continue;
            }
        }
        // A turn that only wrote here needs no reading back.
        if !t.is_open() && !t.attributed(names) {
            let run = Run {
                turn: t.clone(),
                parts: vec![],
            };
            out.extend(keyed(turn_entries(&run, names, shown), &t.started));
            seen.insert(t.prompt.clone());
            continue;
        }
        let runs = if t.prompt.is_empty() && !t.is_open() {
            Ok(vec![])
        } else {
            turns::read_parts(t)
        };
        if let Some(why) = &t.error {
            let what = format!("a turn about this task could not be read in full: {why}");
            out.extend(keyed(
                vec![gap(at.clone(), &t.transcript, what)],
                &t.started,
            ));
        }
        match runs {
            Ok(runs) => {
                for r in runs {
                    if seen.insert(r.turn.prompt.clone()) {
                        out.extend(keyed(turn_entries(&r, names, shown), &r.turn.started));
                    }
                }
            }
            Err(_) if t.is_open() && !t.prompted_for(names) => {}
            Err(e) => {
                let what = format!("a turn about this task could not be read: {e:#}");
                out.extend(keyed(vec![gap(at, &t.transcript, what)], &t.started));
            }
        }
    }
    out
}

fn decision_entries(d: &Decision, shown: &Shown) -> Vec<Entry> {
    // Without an id it is a note, and shows as one.
    let Some(id) = &d.id else { return vec![] };
    let mut q = d.text.clone();
    for (n, o) in d.options.iter().enumerate() {
        q += &format!("\n{}) {o}", n + 1);
    }
    if let Some(r) = &d.recommendation {
        q += &format!("\nRec: {r}");
    }
    let by = |who: &Option<String>, or: &str| who.clone().unwrap_or_else(|| or.into());
    let asker = by(&d.asked_by, "orchestrator");
    let mut out = vec![entry(
        Kind::Assistant,
        &format!("◆ decision {id} asked by {asker}"),
        d.asked_at.clone(),
        q,
    )];
    for r in &d.replies {
        // The same words, already shown as the message that carried them.
        if shown.contains(&(format!("{id} {}", r.kind), r.text.trim().to_string())) {
            continue;
        }
        let (kind, mark) = match r.kind.as_str() {
            "response" => (Kind::Assistant, "◀"),
            _ => (Kind::User, "▶"),
        };
        let who = by(&r.by, "unknown");
        let label = format!("{mark} decision {id} {} by {who}", r.kind);
        out.push(entry(kind, &label, r.at.clone(), r.text.clone()));
    }
    // A list read without replies still holds the latest of each.
    if d.replies.is_empty() {
        for (what, text, at) in [
            ("answer", &d.answer, d.answered_at.clone()),
            ("question", &d.pending_question, None),
        ] {
            if let Some(x) = text {
                let label = format!("▶ decision {id} {what}");
                out.push(entry(Kind::User, &label, at, x.clone()));
            }
        }
    }
    if d.resolved_at.is_some() || d.outcome.is_some() {
        out.push(entry(
            Kind::Assistant,
            &format!("◆ decision {id} resolved"),
            d.resolved_at.clone(),
            d.outcome.clone().unwrap_or_default(),
        ));
    }
    out
}

/// A whole transcript that belongs to the task: a worker's or a worktree session's. `brief`
/// folds the first user text, which is the worker's brief.
fn transcript_entries(path: &str, who: &str, brief: bool) -> Vec<Entry> {
    let (mut r, skipped) = match open_tail(Path::new(path), TAIL) {
        Ok(x) => x,
        Err(e) => {
            return vec![gap(
                None,
                path,
                format!("{who}: transcript unreadable: {e}"),
            )];
        }
    };
    let mut out: Vec<Entry> = vec![];
    let mut first = brief && skipped == 0;
    let mut buf = vec![];
    loop {
        buf.clear();
        match r.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                let ts = out.last().and_then(|e| e.ts.clone());
                out.push(gap(ts, path, format!("{who}: transcript unreadable: {e}")));
                break;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let injected = v["isMeta"] == true || v["isCompactSummary"] == true;
        for mut e in parse_line(&line) {
            match e.kind {
                Kind::User => {
                    let brief = std::mem::take(&mut first);
                    if brief || injected || crate::hook::injected(&e.body) {
                        let what = if brief { "brief" } else { "injected" };
                        let line = truncate(&one_line(&e.body), 100);
                        e.head = format!("{what}: {line}");
                        e.folded = true;
                    }
                    e.label = Some(format!("▶ → {who}"));
                }
                Kind::Assistant => e.label = Some(format!("◀ {who}")),
                Kind::Tool | Kind::ToolResult => {}
            }
            out.push(link(e, path));
        }
        // A message typed while it was working.
        let att = &v["attachment"];
        if v["type"] == "attachment"
            && att["type"] == "queued_command"
            && let Some(m) = att["prompt"].as_str()
            && !crate::hook::injected(m)
        {
            let ts = v["timestamp"].as_str().map(str::to_string);
            out.push(link(
                entry(Kind::User, &format!("▶ → {who}"), ts, cap(m)),
                path,
            ));
        }
        // A background worker's report is a tool call; it is what the worker said.
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            if b["name"] == "SubagentHandback"
                && let Some(m) = b["input"]["message"].as_str()
            {
                let ts = v["timestamp"].as_str().map(str::to_string);
                let e = entry(Kind::Assistant, &format!("◀ {who} report"), ts, cap(m));
                out.push(link(e, path));
            }
        }
    }
    let mut out = collapse(out, who);
    if skipped > 0 {
        let ts = out.first().and_then(|e| e.ts.clone());
        let what = format!(
            "{who}: only the last {} MB is shown; t opens the transcript",
            TAIL >> 20
        );
        out.insert(0, gap(ts, path, what));
    }
    out
}

/// Everything said about the task, oldest first. `decisions` is every one it has had, or why
/// they could not be read.
pub fn build(
    t: &JoinedTask,
    sessions: &[Session],
    turns: &[Turn],
    decisions: Result<Vec<Decision>, String>,
) -> Vec<Entry> {
    let names = names(t);
    let mut shown = Shown::new();
    let mut all = turns_part(turns, &names, &mut shown);
    let decisions = decisions.unwrap_or_else(|why| {
        let what = format!("resolved decisions and replies could not be read: {why}");
        let e = entry(Kind::Assistant, "⚠ incomplete", None, what);
        // Nothing dates it; first is where it cannot be taken for news.
        all.push((String::new(), e));
        t.decisions.clone()
    });
    // A prompt row is not a decision: the session's own turns hold that question.
    for d in decisions.iter().filter(|d| !d.is_prompt_row()) {
        let at = d.asked_at.clone().unwrap_or_default();
        all.extend(keyed(decision_entries(d, &shown), &at));
    }
    for w in &t.workers {
        let who = truncate(&w.description, 60);
        let from = w.modified.clone().unwrap_or_default();
        all.extend(keyed(transcript_entries(&w.transcript, &who, true), &from));
    }
    let wts = t.worktrees();
    let mut seen = HashSet::new();
    for s in sessions
        .iter()
        .filter(|s| wts.iter().any(|w| same_path(&s.worktree, w)))
    {
        // Closing while the view loads lists a session as live and as closed.
        if !seen.insert(&s.session_id) {
            continue;
        }
        let Some(p) = session_transcript(s) else {
            continue;
        };
        let who = format!("session {}", &s.session_id[..s.session_id.len().min(8)]);
        let from = s.session_start.clone().unwrap_or_default();
        let v = transcript_entries(&p.to_string_lossy(), &who, false);
        all.extend(keyed(v, &from));
    }
    let updated = t.task.updated_at.clone().unwrap_or_default();
    for i in note_items(t) {
        let k = key(&sort_key(&i, &updated));
        all.push((
            k,
            entry(Kind::Assistant, &format!("≡ {}", i.source), i.ts, i.body),
        ));
    }
    all.sort_by(|a, b| a.0.cmp(&b.0));
    all.into_iter().map(|(_, e)| e).collect()
}

/// `build` with the store's turns and the tracker's decisions. Slow: runs off the UI thread.
pub fn load(
    loader: &Loader,
    ctx: Option<&Context>,
    t: &JoinedTask,
    sessions: &[Session],
) -> Vec<Entry> {
    let decisions = match ctx {
        Some(c) => loader
            .tracker(c)
            .decisions_all(&t.task.id)
            // A backend without the verb holds no decision records to miss.
            .or_else(|e| match format!("{e:#}") {
                e if e.starts_with("unsupported verb") => Ok(t.decisions.clone()),
                e => Err(e),
            }),
        None => Err(format!("context {} is not loaded", t.context)),
    };
    let turns = turns::all(&crate::store::Store::open());
    build(t, sessions, &turns, decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::jt;
    use crate::model::{Reply, Worker};
    use crate::store::tests::scratch;
    use crate::turns::tests::Log;
    use serde_json::json;

    fn text(v: &[Entry]) -> String {
        v.iter()
            .map(|e| {
                let l = e.label.as_deref().unwrap_or("-");
                format!("[{l}] {}{}\n", e.head, if e.folded { "" } else { &e.body })
            })
            .collect()
    }

    #[test]
    fn each_task_sees_its_share_of_a_turn() {
        let t = scratch("conv-share");
        let mut l = Log::new(&t);
        let path = l.path();
        let st = &t.1;
        let p = "[fermata ctx-12] what is this PR doing?";
        turns::open(st, "s-1", &path, p).unwrap();
        l.prompt(p);
        l.bash("gh pr view");
        l.bash("gh pr diff");
        l.say("It renames a thing.\n### ctx-3\nstill waiting");
        turns::close(st, "s-1", &path).unwrap();
        turns::open(st, "s-1", &path, "sweep").unwrap();
        l.prompt("sweep");
        l.bash("$TRK note ctx-5 x");
        l.say("### ctx-3\ngreen\n### ctx-7\nred\n### ctx-9\nblue");
        turns::close(st, "s-1", &path).unwrap();
        let all = turns::all(st);
        let see = |id: &str| text(&build(&jt(id), &[], &all, Ok(vec![])));
        let a = see("ctx-12");
        assert!(a.contains("[▶ you] what is this PR doing?"), "{a}");
        assert!(a.contains("[⚙ orchestrator] 2 tool calls"), "{a}");
        assert!(a.contains("[◀ orchestrator] It renames a thing."), "{a}");
        assert!(!a.contains("waiting") && !a.contains("green"), "{a}");
        let b = see("ctx-3");
        assert_eq!(
            b,
            "[◀ orchestrator] ### ctx-3\nstill waiting\n[◀ orchestrator] ### ctx-3\ngreen\n"
        );
        assert_eq!(see("ctx-7"), "[◀ orchestrator] ### ctx-7\nred\n");
        let c = see("ctx-5");
        assert!(c.starts_with("[? unattributed activity (turn at "), "{c}");
        assert!(c.trim_end().ends_with("note"), "{c}");
        let v = build(&jt("ctx-5"), &[], &all, Ok(vec![]));
        assert_eq!(v[0].link.as_ref().unwrap().path, path);
        // A longer id is another task, and a ticket key is another name for this one.
        assert_eq!(see("ctx-1"), "");
        let mut k = jt("ctx-99");
        k.ticket_key = Some("ctx-7".into());
        assert_eq!(build(&k, &[], &all, Ok(vec![])).len(), 1);
    }

    #[test]
    fn a_running_turn_shows_without_stop() {
        let t = scratch("conv-run");
        let mut l = Log::new(&t);
        turns::open(&t.1, "s-1", &l.path(), "sweep").unwrap();
        l.prompt("sweep");
        l.bash("$TRK note ctx-5 x");
        l.say("### ctx-3\nso far");
        let all = turns::all(&t.1);
        let v = build(&jt("ctx-3"), &[], &all, Ok(vec![]));
        assert_eq!(v[0].body, "### ctx-3\nso far");
        assert!(text(&build(&jt("ctx-5"), &[], &all, Ok(vec![]))).contains("unattributed"));
    }

    #[test]
    fn unreadable_is_said() {
        let t = scratch("conv-gap");
        turns::open(&t.1, "s-1", "/nope/x.jsonl", "[fermata ctx-1] a").unwrap();
        turns::close(&t.1, "s-1", "/nope/x.jsonl").unwrap();
        let mut j = jt("ctx-1");
        j.decisions = vec![Decision {
            id: Some("d1".into()),
            text: "Merge?".into(),
            pending_question: Some("why?".into()),
            ..Default::default()
        }];
        j.decisions.push(Decision {
            text: "Allow Bash?".into(),
            prompt: Some("permission".into()),
            ..Default::default()
        });
        j.workers = vec![Worker {
            description: "ctx-1: w".into(),
            transcript: "/nope/w.jsonl".into(),
            finished: true,
            modified: None,
        }];
        let s = text(&build(&j, &[], &turns::all(&t.1), Err("boom".into())));
        assert!(
            s.contains("[⚠ incomplete] resolved decisions and replies could not be read: boom")
        );
        assert!(s.contains("could not be read in full"), "{s}");
        assert!(!s.contains("may be about this task"), "{s}");
        assert!(s.contains("ctx-1: w: transcript unreadable"), "{s}");
        assert!(s.contains("Merge?") && s.contains("question] why?"), "{s}");
        assert!(!s.contains("Allow Bash?"), "{s}");
        // A later unmarked turn of that session may have been about it; of another, not.
        turns::open(&t.1, "s-1", "/nope/x.jsonl", "sweep").unwrap();
        turns::close(&t.1, "s-1", "/nope/x.jsonl").unwrap();
        let of = |id| text(&build(&jt(id), &[], &turns::all(&t.1), Ok(vec![])));
        assert!(of("ctx-1").contains("may be about this task"));
        assert!(of("ctx-1").contains("could not be read in full"));
        assert!(!of("ctx-2").contains("may be about this task"));
    }

    #[test]
    fn a_session_listed_twice_shows_once() {
        let t = scratch("conv-twice");
        let mut l = Log::new(&t);
        l.prompt("go");
        l.say("only once");
        let mut j = jt("ctx-1");
        j.branches = vec![crate::model::Branch {
            worktree: "/wt/ctx-1".into(),
            ..Default::default()
        }];
        let s = Session {
            session_id: "s-1".into(),
            worktree: "/wt/ctx-1".into(),
            transcript_path: Some(l.path()),
            ..Default::default()
        };
        let x = text(&build(&j, &[s.clone(), s], &[], Ok(vec![])));
        assert_eq!(x.matches("only once").count(), 1, "{x}");
    }

    #[test]
    fn a_section_spans_tool_calls() {
        let t = scratch("conv-span");
        let mut l = Log::new(&t);
        let p = "[fermata ctx-1] a";
        turns::open(&t.1, "s-1", &l.path(), p).unwrap();
        l.prompt(p);
        l.say("### ctx-7\nlooking");
        l.bash("ls");
        l.say("seven is fixed");
        turns::close(&t.1, "s-1", &l.path()).unwrap();
        let of = |id| text(&build(&jt(id), &[], &turns::all(&t.1), Ok(vec![])));
        assert!(of("ctx-7").contains("seven is fixed"));
        assert!(!of("ctx-1").contains("seven is fixed"));
    }

    #[test]
    fn sources_merge_in_time_order() {
        let t = scratch("conv-merge");
        let mut l = Log::new(&t);
        let p = "[fermata ctx-1 d1 question] why?";
        turns::open(&t.1, "s-1", &l.path(), p).unwrap();
        l.prompt(p); // 14:01
        l.say("because"); // 14:02
        turns::close(&t.1, "s-1", &l.path()).unwrap();
        let w = t.0.join("w.jsonl");
        let at = |m: u32| format!("2026-11-23T14:{m:02}:30.000Z");
        let lines = [
            json!({"type":"user","timestamp":at(2),"message":{"content":"a long brief"}}),
            json!({"type":"assistant","timestamp":at(3),"message":{"content":[
                {"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}),
            json!({"type":"assistant","timestamp":at(5),"message":{"content":[
                {"type":"tool_use","name":"SubagentHandback","input":{"message":"all done"}}]}}),
        ];
        let text_of: Vec<String> = lines.iter().map(Value::to_string).collect();
        std::fs::write(&w, text_of.join("\n")).unwrap();
        let mut j = jt("ctx-1");
        j.task.notes = "NOTE (2026-11-23T14:04:00Z): noted".into();
        j.workers = vec![Worker {
            description: "ctx-1: w".into(),
            transcript: w.to_string_lossy().into_owned(),
            finished: true,
            modified: None,
        }];
        let reply = |kind: &str, text: &str, at: &str| Reply {
            kind: kind.into(),
            text: text.into(),
            at: Some(at.into()),
            by: Some("user".into()),
        };
        let d = Decision {
            id: Some("d1".into()),
            text: "Merge?".into(),
            asked_at: Some("2026-11-23T14:00:00Z".into()),
            replies: vec![
                reply("question", "why?", "2026-11-23T14:00:59Z"),
                reply("response", "because", "2026-11-23T14:06:00Z"),
            ],
            resolved_at: Some("2026-11-23T15:00:00Z".into()),
            outcome: Some("merged".into()),
            ..Default::default()
        };
        let s = text(&build(&j, &[], &turns::all(&t.1), Ok(vec![d])));
        let want = [
            "[◆ decision d1 asked by orchestrator] Merge?",
            "[▶ you (d1 question)] why?",
            "[◀ orchestrator] because",
            "[▶ → ctx-1: w] brief: a long brief",
            "[⚙ ctx-1: w] 2 tool calls",
            "[≡ note note]",
            "[◀ ctx-1: w report] all done",
            "[◀ decision d1 response by user] because",
            "[◆ decision d1 resolved] merged",
        ];
        let got: Vec<&str> = s.lines().collect();
        let mut at = 0;
        for w in want {
            match got[at..].iter().position(|g| g.starts_with(w)) {
                Some(i) => at += i + 1,
                None => panic!("missing or out of order: {w}\n{s}"),
            }
        }
        assert_eq!(s.matches("why?").count(), 1, "{s}");
    }
}
