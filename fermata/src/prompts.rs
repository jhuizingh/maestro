//! What a session's pane is asking, and the tracker decisions that mirror its questions.
//!
//! The hooks put the open prompt on the session record and, for a question in a task worktree,
//! write one mirror file per question under `<store root>/prompts/`. `sync` brings the tracker
//! in line with them, outside the hook. See "Prompts" in DESIGN.md.
use crate::baton::{NewDecision, Tracker};
use crate::delivery;
use crate::log;
use crate::model::{Decision, DecisionStatus, JoinedTask, Prompt, Session};
use crate::store::{Kind, Store, now, parse_record};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::hash::{BuildHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Prompt and question texts are cut to this many characters.
const TEXT_MAX: usize = 2000;
/// A permission's tool input is cut to this many.
const ASK_MAX: usize = 500;
/// The end of a last message is cut to this many.
const TAIL_MAX: usize = 600;
/// An ended mirror the tracker keeps refusing is dropped after this long.
const KEEP_DAYS: i64 = 14;
/// Who poses and resolves mirrored decisions.
pub const BY: &str = "session";

pub fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// The end of a session's last message, where its question is.
pub fn tail(s: &str) -> String {
    let s = s.trim();
    let n = s.chars().count();
    if n <= TAIL_MAX {
        return s.to_string();
    }
    "…".to_string() + &s.chars().skip(n - TAIL_MAX).collect::<String>()
}

/// Whether any question is being mirrored.
pub fn any(store: &Store) -> bool {
    let dir = std::fs::read_dir(dir(store));
    dir.is_ok_and(|mut d| d.next().is_some())
}

/// One question of an `AskUserQuestion` call.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    pub text: String,
    pub options: Vec<String>,
    /// 1-based, when an option's label says "(Recommended)".
    pub rec: Option<usize>,
}

/// The questions in an `AskUserQuestion` input; anything unreadable is left out.
pub fn questions(input: &Value) -> Vec<Question> {
    let list = input["questions"].as_array().cloned().unwrap_or_default();
    list.iter()
        .filter_map(|q| {
            let text = cut(q["question"].as_str()?.trim(), TEXT_MAX);
            if text.is_empty() {
                return None;
            }
            let mut options = vec![];
            let mut rec = None;
            for o in q["options"].as_array().into_iter().flatten() {
                let label = o["label"].as_str().or(o.as_str()).unwrap_or("").trim();
                if label.is_empty() {
                    continue;
                }
                let desc = o["description"].as_str().unwrap_or("").trim();
                options.push(match desc {
                    "" => label.to_string(),
                    d => cut(&format!("{label} — {d}"), TEXT_MAX),
                });
                if rec.is_none() && label.to_lowercase().contains("(recommended)") {
                    rec = Some(options.len());
                }
            }
            Some(Question { text, options, rec })
        })
        .collect()
}

/// The prompt a dialog tool opens. `ids` are the decisions minted for its questions.
pub fn of_tool(tool: &str, input: &Value, ids: Vec<String>, agent: &str) -> Prompt {
    let base = Prompt {
        at: now(),
        agent: agent.into(),
        decisions: ids,
        ..Default::default()
    };
    if tool == "ExitPlanMode" {
        let plan = input["plan"].as_str().unwrap_or("").trim();
        let text = format!("Approve this plan?\n{}", cut(plan, TEXT_MAX));
        return Prompt {
            kind: "plan".into(),
            text: text.trim_end().into(),
            ..base
        };
    }
    let qs = questions(input);
    let (text, options) = match qs.as_slice() {
        [] => (format!("{tool} is open in the pane"), vec![]),
        [q] => (q.text.clone(), q.options.clone()),
        many => {
            let part = |q: &Question| {
                let opts: String = q.options.iter().map(|o| format!("\n  - {o}")).collect();
                q.text.clone() + &opts
            };
            let all: Vec<String> = many.iter().map(part).collect();
            (cut(&all.join("\n"), TEXT_MAX), vec![])
        }
    };
    Prompt {
        kind: "question".into(),
        text,
        options,
        ..base
    }
}

/// A permission request in short: the tool, and its command, path or input.
pub fn ask_text(tool: &str, input: &Value) -> String {
    let pick = ["command", "file_path", "url", "path"]
        .iter()
        .find_map(|k| input[*k].as_str());
    let detail = match pick {
        Some(s) => s.trim().to_string(),
        None if input.is_null() => String::new(),
        None => input.to_string(),
    };
    cut(&format!("{tool}: {detail}"), ASK_MAX)
}

/// The prompt a permission or elicitation notification announces.
pub fn of_notification(kind: &str, message: &str, ask: Option<&str>) -> Prompt {
    let text = match ask {
        Some(a) if !a.is_empty() => format!("{message}\n{a}"),
        _ => message.to_string(),
    };
    Prompt {
        kind: kind.into(),
        text: cut(text.trim(), TEXT_MAX),
        at: now(),
        ..Default::default()
    }
}

/// A fresh decision id in the tracker's form (`d` + hex).
pub fn mint_id() -> String {
    let r = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("d{:010x}", r & 0xff_ffff_ffff)
}

/// One mirrored question, kept until its decision is resolved.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Mirror {
    /// The decision's id, minted by the hook.
    pub id: String,
    pub task: String,
    pub session_id: String,
    pub cwd: String,
    /// The context `cwd` resolved to, for when the worktree is gone.
    pub context: String,
    pub question: String,
    pub options: Vec<String>,
    pub rec: Option<usize>,
    pub asked_at: String,
    /// The tracker has the decision.
    pub recorded: bool,
    /// The answer given in the pane, once `post-tool-use` saw it.
    pub answer: Option<String>,
    /// Why posing it was given up.
    pub gave_up: Option<String>,
}

fn dir(store: &Store) -> PathBuf {
    store.root.join("prompts")
}

fn path(store: &Store, id: &str) -> Result<PathBuf> {
    if !crate::store::valid_id(id) {
        bail!("bad decision id {id:?}");
    }
    Ok(dir(store).join(format!("{id}.json")))
}

fn write(store: &Store, m: &Mirror) -> Result<()> {
    let p = path(store, &m.id)?;
    std::fs::create_dir_all(dir(store))?;
    let tmp = p.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(m)? + "\n")?;
    std::fs::rename(&tmp, &p).context("write prompt mirror")?;
    Ok(())
}

fn read(store: &Store, id: &str) -> Option<Mirror> {
    let rec = parse_record(&std::fs::read_to_string(path(store, id).ok()?).ok()?)?;
    serde_json::from_value(Value::Object(rec)).ok()
}

/// Every mirror, oldest first.
pub fn all(store: &Store) -> Vec<Mirror> {
    let Ok(rd) = std::fs::read_dir(dir(store)) else {
        return vec![];
    };
    let mut v: Vec<Mirror> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| read(store, e.path().file_stem()?.to_str()?))
        .collect();
    v.sort_by(|a, b| a.asked_at.cmp(&b.asked_at).then(a.id.cmp(&b.id)));
    v
}

fn update(store: &Store, id: &str, f: impl FnOnce(&mut Mirror)) -> Result<()> {
    store.locked(|| {
        let Some(mut m) = read(store, id) else {
            return Ok(());
        };
        f(&mut m);
        write(store, &m)
    })?
}

fn remove(store: &Store, id: &str) {
    if let Ok(p) = path(store, id) {
        let _ = std::fs::remove_file(p);
    }
}

/// Write the mirrors for questions just asked; `ids` pair with `qs`. The session record must
/// already name the ids, or a sync running now would take the questions for closed ones.
pub fn pose(
    store: &Store,
    task: &str,
    session_id: &str,
    cwd: &str,
    qs: &[Question],
    ids: &[String],
) -> Result<()> {
    let t = now();
    store.locked(|| {
        for (q, id) in qs.iter().zip(ids) {
            write(
                store,
                &Mirror {
                    id: id.clone(),
                    task: task.into(),
                    session_id: session_id.into(),
                    cwd: cwd.into(),
                    question: q.text.clone(),
                    options: q.options.clone(),
                    rec: q.rec,
                    asked_at: t.clone(),
                    ..Default::default()
                },
            )?;
        }
        Ok(())
    })?
}

/// The answer to `question` in an `AskUserQuestion` result; `None` when it names none.
fn answer_to(response: &Value, question: &str) -> Option<String> {
    let answers = response["answers"].as_object()?;
    let (_, v) = answers
        .iter()
        .find(|(k, _)| cut(k.trim(), TEXT_MAX) == question)?;
    let text = match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            parts.join(", ")
        }
        _ => return None,
    };
    (!text.trim().is_empty()).then(|| text.trim().to_string())
}

/// The questions `ids` were answered in the pane: keep the answers for their outcomes, and stop
/// any reply still held for them. Call before the session record drops the prompt.
pub fn answer(store: &Store, ids: &[String], response: &Value) -> Result<()> {
    for id in ids {
        update(store, id, |m| {
            let a = answer_to(response, &m.question);
            m.answer = Some(a.unwrap_or_default());
        })?;
    }
    let ours = |d: &delivery::Delivery| {
        d.state == delivery::State::Held && d.decision.as_ref().is_some_and(|x| ids.contains(x))
    };
    for d in delivery::all(store).into_iter().filter(ours) {
        delivery::update(store, &d.id, |d| {
            let held = ours(d);
            if held {
                d.state = delivery::State::NoteOnly;
                d.detail = "answered in the session".into();
            }
            held
        })?;
    }
    Ok(())
}

/// Where a mirrored question stands, read from its session's record.
#[derive(Debug, PartialEq)]
enum Standing {
    Open,
    Moved,
    Ended,
    /// The record is there but cannot be read, so nothing is concluded.
    Unknown,
}

fn standing(store: &Store, m: &Mirror) -> Standing {
    let Some(rec) = store.read(Kind::Live, &m.session_id) else {
        return if store.live_ids().contains(&m.session_id) {
            Standing::Unknown
        } else {
            Standing::Ended
        };
    };
    if rec.get("status").and_then(Value::as_str) == Some("dead") {
        return Standing::Ended;
    }
    match open_ids(&rec).contains(&m.id) {
        true => Standing::Open,
        false => Standing::Moved,
    }
}

/// The decisions the record's open prompt was mirrored to.
pub fn open_ids(rec: &crate::store::Record) -> Vec<String> {
    let list = rec.get("prompt").map(|p| &p["decisions"]);
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn unsupported(e: &anyhow::Error) -> bool {
    e.to_string().starts_with("unsupported verb")
}

/// The verb or the thing it names does not exist, so trying again changes nothing.
fn hopeless(e: &anyhow::Error) -> bool {
    unsupported(e) || e.to_string().starts_with("not found")
}

type Trackers<'a> = &'a dyn Fn(&Mirror) -> Result<Arc<dyn Tracker>>;

/// Bring the tracker in line with every mirror. Returns one line per change made.
pub fn sync(store: &Store, trackers: Trackers) -> Result<Vec<String>> {
    let mut out = vec![];
    for m in all(store) {
        match sync_one(store, &m, trackers) {
            Ok(Some(l)) => out.push(l),
            Ok(None) => {}
            Err(e) => log::warn(format!("prompt decision {} on {}: {e:#}", m.id, m.task)),
        }
    }
    Ok(out)
}

fn sync_one(store: &Store, m: &Mirror, trackers: Trackers) -> Result<Option<String>> {
    let st = standing(store, m);
    // The hook keeps an answer before its record drops the prompt, so read again after.
    let m = &read(store, &m.id).unwrap_or_else(|| m.clone());
    match st {
        Standing::Unknown => Ok(None),
        Standing::Open if m.recorded || m.gave_up.is_some() => Ok(None),
        Standing::Open => {
            let posed = trackers(m)?.decide(
                &m.task,
                &NewDecision {
                    id: &m.id,
                    question: &m.question,
                    options: &m.options,
                    rec: m.rec,
                    by: BY,
                },
            );
            match posed {
                Ok(()) => {}
                // A try whose answer was lost: the id is ours, so the decision is too.
                Err(e) if e.to_string().contains("already exists") => {}
                Err(e) if hopeless(&e) => {
                    update(store, &m.id, |m| m.gave_up = Some(format!("{e:#}")))?;
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
            update(store, &m.id, |m| m.recorded = true)?;
            Ok(Some(format!("{}: posed {}", m.task, m.id)))
        }
        // It was never recorded. A try that did land but timed out is found below by asking.
        Standing::Moved | Standing::Ended if m.gave_up.is_some() => {
            remove(store, &m.id);
            Ok(None)
        }
        Standing::Moved | Standing::Ended => {
            let r = close(store, m, &st, trackers);
            if r.is_err() && expired(m) {
                log::warn(format!("prompt decision {} on {}: gave up", m.id, m.task));
                remove(store, &m.id);
            }
            r
        }
    }
}

fn expired(m: &Mirror) -> bool {
    crate::util::parse_ts(&m.asked_at)
        .is_none_or(|t| (chrono::Utc::now() - t).num_days() >= KEEP_DAYS)
}

/// A reply that may still reach its session: held, or typed in too recently to have been seen.
fn in_flight(d: &delivery::Delivery) -> bool {
    use delivery::State::*;
    let sent = d.sent_at.as_deref().and_then(crate::util::parse_ts);
    let fresh = sent.is_some_and(|t| (chrono::Utc::now() - t).num_minutes() < 10);
    d.received_at.is_none() && (d.state == Held || (matches!(d.state, Delivered | Queued) && fresh))
}

/// Resolve the decision of a question that is no longer open in its pane.
fn close(store: &Store, m: &Mirror, st: &Standing, trackers: Trackers) -> Result<Option<String>> {
    let trk = trackers(m)?;
    let mut outcome = match (&m.answer, st) {
        (Some(a), _) if a.is_empty() => "answered in the session".to_string(),
        (Some(a), _) => format!("answered in the session: {a}"),
        (None, Standing::Ended) => "the session ended before it was answered".into(),
        (None, _) => "the session moved past the prompt".into(),
    };
    // An answer given elsewhere that has not reached anyone must not be resolved away.
    if m.answer.is_none() || !m.recorded {
        match trk.decisions(&m.task) {
            Err(e) if hopeless(&e) => {}
            Err(e) => return Err(e),
            Ok(ds) => {
                let Some(d) = ds.iter().find(|d| d.id.as_deref() == Some(&m.id)) else {
                    remove(store, &m.id);
                    return Ok(None);
                };
                if m.answer.is_none() && d.status == DecisionStatus::Answered {
                    let mine: Vec<_> = delivery::all(store)
                        .into_iter()
                        .filter(|x| x.route == "session")
                        .filter(|x| x.decision.as_deref() == Some(&m.id))
                        .collect();
                    if mine.iter().any(in_flight) {
                        return Ok(None);
                    }
                    if !mine.iter().any(|x| x.received_at.is_some()) {
                        // Waiting on the orchestrator now, like any answered decision.
                        remove(store, &m.id);
                        return Ok(None);
                    }
                    outcome = "answered, and the session received it".into();
                }
            }
        }
    }
    match trk.resolve(&m.task, &m.id, &outcome, BY) {
        Ok(()) => {}
        Err(e) if hopeless(&e) => {
            remove(store, &m.id);
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    remove(store, &m.id);
    Ok(Some(format!("{}: resolved {} ({outcome})", m.task, m.id)))
}

/// `sync` against the real trackers, one run at a time. `wait` blocks for a running one (it may
/// have passed our mirror already); otherwise a running sync is left to it.
pub fn sync_real(store: &Store, wait: bool) -> Result<Vec<String>> {
    if all(store).is_empty() {
        return Ok(vec![]);
    }
    std::fs::create_dir_all(&store.root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(store.root.join(".prompts.lock"))?;
    if wait {
        lock.lock()?;
    } else if lock.try_lock().is_err() {
        return Ok(vec![]);
    }
    let baton = crate::baton::locate_baton().context("baton not found")?;
    sync(store, &|m| {
        // The context is the one baton resolves for the worktree the question was asked in.
        let ctx = match crate::baton::resolve_context_at(&baton, Path::new(&m.cwd)) {
            Err(_) if !m.context.is_empty() && !Path::new(&m.cwd).is_dir() => {
                crate::baton::resolve_context(&baton, &m.context)?
            }
            r => r?,
        };
        let v: Value = serde_json::from_str(&ctx).unwrap_or_default();
        let name = v["name"].as_str().unwrap_or("").to_string();
        if name != m.context {
            update(store, &m.id, |m| m.context = name)?;
        }
        Ok(Arc::new(crate::baton::ShellTracker::new(&baton, ctx)))
    })
}

/// What the session is asking, as far as its record says.
fn shown(s: &Session) -> Option<Prompt> {
    if s.status == "dead" {
        return None;
    }
    let dialog = delivery::open_prompt(s);
    if let Some(p) = s.prompt.as_ref().filter(|p| !p.text.trim().is_empty()) {
        return Some(p.clone());
    }
    let msg = s.status_message.clone().unwrap_or_default();
    if let Some(d) = dialog {
        let kind = match s.prompt_open.as_deref() {
            Some("AskUserQuestion") => "question",
            Some("ExitPlanMode") => "plan",
            Some("elicitation") => "elicitation",
            _ => "permission",
        };
        let text = match msg.trim() {
            "" => format!("{d} is open in the pane"),
            m => m.to_string(),
        };
        return Some(Prompt {
            kind: kind.into(),
            text,
            ..Default::default()
        });
    }
    if !s.needs_input() {
        return None;
    }
    let mut text = match msg.trim() {
        "" => "The session is waiting for your input".to_string(),
        m => m.to_string(),
    };
    if let Some(l) = s.last_message.as_deref().filter(|l| !l.trim().is_empty()) {
        text = format!("{text}\n{}", l.trim());
    }
    Some(Prompt {
        kind: INPUT.into(),
        text,
        at: s.idle_since.clone().unwrap_or_default(),
        ..Default::default()
    })
}

/// A session waiting at its input line: not a dialog, so it can be typed to.
pub const INPUT: &str = "input";

/// Put the session's open prompt among the task's decisions: mark the mirrored ones, and add a
/// row when nothing in the tracker stands for it. Safe to repeat; call when `session` changes.
pub fn apply(j: &mut JoinedTask) {
    j.decisions.retain(|d| !d.is_prompt_row());
    for d in &mut j.decisions {
        d.prompt = None;
    }
    let Some(p) = j.session.as_ref().and_then(shown) else {
        return;
    };
    let mut open = 0;
    for d in &mut j.decisions {
        if d.id.as_ref().is_some_and(|id| p.decisions.contains(id)) {
            d.prompt = Some(p.kind.clone());
            open += usize::from(d.is_open());
        }
    }
    let mirrored = !p.decisions.is_empty() && open == p.decisions.len();
    if mirrored || (p.kind == INPUT && j.open_decisions().next().is_some()) {
        return;
    }
    j.decisions.push(Decision {
        text: p.text,
        options: p.options,
        asked_at: Some(p.at).filter(|a| !a.is_empty()),
        prompt: Some(p.kind),
        ..Default::default()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::jt;
    use crate::store::tests::{put, scratch};
    use anyhow::anyhow;
    use serde_json::json;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Trk {
        calls: Mutex<Vec<String>>,
        /// The error each write fails with.
        fail: Mutex<Option<String>>,
        decisions: Mutex<Vec<Decision>>,
    }

    impl Trk {
        fn result(&self) -> Result<()> {
            match &*self.fail.lock().unwrap() {
                Some(e) => Err(anyhow!("{e}")),
                None => Ok(()),
            }
        }
        fn fail(&self, e: Option<&str>) {
            *self.fail.lock().unwrap() = e.map(String::from);
        }
        fn calls(&self) -> Vec<String> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
        fn set(&self, id: &str, status: DecisionStatus) {
            *self.decisions.lock().unwrap() = vec![Decision {
                id: Some(id.into()),
                status,
                ..Default::default()
            }];
        }
    }

    impl Tracker for Trk {
        fn list(&self, _: &str) -> Result<Vec<crate::model::Task>> {
            panic!("sync lists nothing")
        }
        fn get(&self, _: &str) -> Result<crate::model::Task> {
            panic!("sync gets nothing")
        }
        fn list_branches(&self, _: &str) -> Result<Vec<crate::model::Branch>> {
            panic!("sync lists no branches")
        }
        fn note(&self, _: &str, _: &str) -> Result<()> {
            panic!("sync writes no notes")
        }
        fn decisions(&self, _: &str) -> Result<Vec<Decision>> {
            Ok(self.decisions.lock().unwrap().clone())
        }
        fn decide(&self, id: &str, d: &NewDecision) -> Result<()> {
            let c = format!("decide {id} {} {} {:?}", d.id, d.question, d.rec);
            self.calls.lock().unwrap().push(c);
            self.result()
        }
        fn resolve(&self, id: &str, did: &str, outcome: &str, by: &str) -> Result<()> {
            let c = format!("resolve {id} {did} {outcome} ({by})");
            self.calls.lock().unwrap().push(c);
            self.result()
        }
    }

    const D: &str = "d0123456789";

    fn record(st: &Store, v: Value) {
        put(&st.root, Kind::Live, "s-1", v);
    }

    fn open(st: &Store) {
        record(st, json!({"session_id":"s-1","prompt":{"decisions":[D]}}));
    }

    fn moved(st: &Store) {
        record(st, json!({"session_id":"s-1","prompt":null}));
    }

    fn pose_one(st: &Store) {
        let q = Question {
            text: "Which?".into(),
            options: vec!["a".into(), "b".into()],
            rec: Some(2),
        };
        pose(st, "t-1", "s-1", "/wt", &[q], &[D.into()]).unwrap();
    }

    fn run(st: &Store, t: &Arc<Trk>) -> Vec<String> {
        sync(st, &|_| Ok(t.clone() as Arc<dyn Tracker>)).unwrap();
        t.calls()
    }

    #[test]
    fn sync_poses_once_and_resolves_when_moved_past() {
        let sc = scratch("prompts-sync");
        let (st, t) = (&sc.1, Arc::new(Trk::default()));
        open(st);
        pose_one(st);
        t.fail(Some("tracker decide: lock timeout"));
        assert_eq!(run(st, &t).len(), 1, "tried");
        t.fail(Some(&format!(
            "tracker: decide: decision {D} already exists"
        )));
        assert_eq!(run(st, &t), [format!("decide t-1 {D} Which? Some(2)")]);
        t.fail(None);
        assert!(run(st, &t).is_empty(), "recorded, still open");
        moved(st);
        t.set(D, DecisionStatus::Open);
        t.fail(Some("tracker: down"));
        assert_eq!(run(st, &t).len(), 1);
        assert_eq!(all(st).len(), 1, "retried later");
        t.fail(None);
        assert_eq!(
            run(st, &t),
            [format!(
                "resolve t-1 {D} the session moved past the prompt (session)"
            )]
        );
        assert!(all(st).is_empty());
    }

    #[test]
    fn sync_outcomes() {
        let sc = scratch("prompts-outcomes");
        let (st, t) = (&sc.1, Arc::new(Trk::default()));
        let recorded = || {
            open(st);
            pose_one(st);
            run(st, &t);
        };
        // Answered in the pane.
        recorded();
        answer(st, &[D.into()], &json!({"answers":{"Which?":["a","b"]}})).unwrap();
        moved(st);
        assert!(run(st, &t)[0].contains("answered in the session: a, b"));
        // The session ended, and a replaced (dead) one ended too.
        recorded();
        t.set(D, DecisionStatus::Open);
        st.remove(Kind::Live, "s-1");
        assert!(run(st, &t)[0].contains("session ended"));
        recorded();
        record(
            st,
            json!({"session_id":"s-1","status":"dead","prompt":{"decisions":[D]}}),
        );
        assert!(run(st, &t)[0].contains("session ended"));
        // Answered elsewhere: the orchestrator's to resolve.
        recorded();
        t.set(D, DecisionStatus::Answered);
        st.remove(Kind::Live, "s-1");
        assert!(run(st, &t).is_empty() && all(st).is_empty());
        // Unless the session got it, and not while it may still get it.
        recorded();
        moved(st);
        let mut d = delivery::Delivery::new("c", "t-1", "a", "a", "session");
        d.decision = Some(D.into());
        d.state = delivery::State::Held;
        delivery::write(st, &d).unwrap();
        assert!(run(st, &t).is_empty());
        assert_eq!(all(st).len(), 1);
        d.state = delivery::State::Delivered;
        d.received_at = Some(now());
        delivery::write(st, &d).unwrap();
        assert!(run(st, &t)[0].contains("the session received it"));
        // An unreadable record concludes nothing.
        recorded();
        std::fs::write(st.root.join("sessions/s-1.json"), "{oops").unwrap();
        assert!(run(st, &t).is_empty());
        assert_eq!(all(st).len(), 1);
    }

    #[test]
    fn sync_drops_what_was_never_recorded() {
        let sc = scratch("prompts-never");
        let (st, t) = (&sc.1, Arc::new(Trk::default()));
        open(st);
        pose_one(st);
        moved(st);
        assert!(run(st, &t).is_empty() && all(st).is_empty());
        // A backend without the verb is asked once.
        open(st);
        pose_one(st);
        t.fail(Some("unsupported verb: decide"));
        assert_eq!(run(st, &t).len(), 1);
        assert!(run(st, &t).is_empty());
        moved(st);
        assert!(run(st, &t).is_empty() && all(st).is_empty());
    }

    #[test]
    fn answering_in_the_pane_stops_a_held_reply() {
        let sc = scratch("prompts-held");
        let st = &sc.1;
        open(st);
        pose_one(st);
        let mut d = delivery::Delivery::new("c", "t-1", "a", "a", "session");
        d.decision = Some(D.into());
        d.state = delivery::State::Held;
        delivery::write(st, &d).unwrap();
        answer(st, &[D.into()], &json!("odd")).unwrap();
        assert_eq!(delivery::all(st)[0].state, delivery::State::NoteOnly);
        assert_eq!(all(st)[0].answer.as_deref(), Some(""));
    }

    #[test]
    fn reads_odd_input() {
        assert!(questions(&json!({})).is_empty());
        assert!(questions(&json!({"questions":[{"question":" "},{"x":1}]})).is_empty());
        let p = of_tool("AskUserQuestion", &Value::Null, vec![], "");
        assert!(p.text.contains("open in the pane"));
        let p = of_tool("ExitPlanMode", &json!({"plan":"1. x"}), vec![], "");
        assert_eq!(
            (p.kind.as_str(), p.text.as_str()),
            ("plan", "Approve this plan?\n1. x")
        );
        assert_eq!(tail(&"x".repeat(700)).chars().count(), TAIL_MAX + 1);
        let id = mint_id();
        assert!(id.len() == 11 && id[1..].bytes().all(|b| b.is_ascii_hexdigit()));
        // A prompt of a shape fermata does not know never loses the session.
        let s: Session =
            serde_json::from_value(json!({"session_id":"s","prompt":"?","last_message":4}))
                .unwrap();
        assert_eq!((s.prompt, s.last_message), (None, None));
    }

    fn session(v: Value) -> Option<Session> {
        Some(serde_json::from_value(v).unwrap())
    }

    #[test]
    fn apply_shows_the_open_prompt_once() {
        let mut j = jt("t-1");
        j.session = session(json!({"session_id":"s","status":"working",
            "prompt_open":"AskUserQuestion",
            "prompt":{"kind":"question","text":"Which?","options":["a"],"decisions":[D]}}));
        apply(&mut j);
        apply(&mut j);
        assert_eq!(j.decisions.len(), 1, "not in the tracker yet");
        assert!(j.decisions[0].pane_only() && j.decisions[0].is_open());
        j.decisions.push(Decision {
            id: Some(D.into()),
            ..Default::default()
        });
        apply(&mut j);
        assert_eq!(j.decisions.len(), 1, "the decision stands for it");
        assert_eq!(j.decisions[0].prompt.as_deref(), Some("question"));
        assert!(!j.decisions[0].pane_only());
        // Answered in fermata, but the dialog is still up.
        j.decisions[0].status = DecisionStatus::Answered;
        apply(&mut j);
        assert_eq!(j.decisions.len(), 2);
        j.session = session(json!({"session_id":"s","status":"working"}));
        apply(&mut j);
        assert_eq!(
            (j.decisions.len(), j.decisions[0].prompt.clone()),
            (1, None)
        );
    }

    #[test]
    fn apply_shows_waits() {
        let mut j = jt("t-1");
        j.session = session(json!({"session_id":"s","status":"needs_input",
            "status_message":"Claude is waiting","last_message":"Shall I?"}));
        apply(&mut j);
        assert_eq!(j.decisions[0].text, "Claude is waiting\nShall I?");
        assert!(!j.decisions[0].pane_only(), "can be typed to");
        j.decisions.push(Decision {
            id: Some("d1".into()),
            ..Default::default()
        });
        apply(&mut j);
        assert_eq!(j.decisions.len(), 1, "another decision covers the wait");
        // A record from before `prompt` was kept.
        j.session = session(json!({"session_id":"s","status":"needs_permission",
            "status_message":"may I"}));
        apply(&mut j);
        assert_eq!(j.decisions.len(), 2);
        assert_eq!(j.decisions[1].text, "may I");
        j.session.as_mut().unwrap().status = "dead".into();
        apply(&mut j);
        assert_eq!(j.decisions.len(), 1);
    }
}
