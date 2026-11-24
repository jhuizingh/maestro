//! Turn records: pointers to the orchestrator turns that are about a task, or wrote to one.
//! Written by the hooks, read by the Conversation view; see "Conversation" in DESIGN.md.
use crate::attribution::{ids_in, sections, segments, tracker_writes};
use crate::store::{Store, now, parse_record, valid_id};
use crate::transcript::{Entry, Kind, parse_line};
use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);
/// The most one scan reads.
const MAX_SCAN: u64 = 32 * 1024 * 1024;
/// Where a scan with no recorded starting point begins, from the end.
const TAIL: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Write {
    pub task: String,
    /// `note`, `state`, … or `SendMessage` / `Agent`.
    pub what: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub session: String,
    pub transcript: String,
    /// Byte range in the transcript.
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub end: u64,
    /// The prompt entry's uuid; empty while the turn is running.
    #[serde(default)]
    pub prompt: String,
    /// The last assistant entry's uuid.
    #[serde(default)]
    pub last: String,
    #[serde(default)]
    pub started: String,
    #[serde(default)]
    pub ended: Option<String>,
    /// Ids from `[fermata <id>]` segments.
    #[serde(default)]
    pub prompted: Vec<String>,
    /// Ids from `### <id>` headings in the reply.
    #[serde(default)]
    pub sections: Vec<String>,
    #[serde(default)]
    pub writes: Vec<Write>,
    /// Why the transcript could not be read in full.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn hit(ids: &[String], names: &[&str]) -> bool {
    ids.iter().any(|i| names.contains(&i.as_str()))
}

impl Turn {
    pub fn is_open(&self) -> bool {
        self.prompt.is_empty() && self.ended.is_none()
    }

    /// Whether it was prompted for the task known by `names`.
    pub fn prompted_for(&self, names: &[&str]) -> bool {
        hit(&self.prompted, names)
    }

    pub fn attributed(&self, names: &[&str]) -> bool {
        self.prompted_for(names) || hit(&self.sections, names)
    }

    /// What it did to the task, when it is not about it; empty when it is.
    pub fn unattributed(&self, names: &[&str]) -> Vec<&str> {
        if self.attributed(names) {
            return vec![];
        }
        let mut v: Vec<&str> = vec![];
        for w in &self.writes {
            if names.contains(&w.task.as_str()) && !v.contains(&w.what.as_str()) {
                v.push(&w.what);
            }
        }
        v
    }

    /// `unattributed activity (turn at 14:02)`, in local time.
    pub fn unattributed_title(&self) -> String {
        let at = crate::util::short_ts(&self.started);
        format!("unattributed activity (turn at {at})")
    }
}

/// One thing in a turn, in transcript order.
#[derive(Debug, Clone)]
pub enum Part {
    /// Text you typed: the prompt, or a message queued while the turn ran.
    Prompt(Option<String>, String),
    Reply(Option<String>, String),
    Tool(Entry),
}

/// One turn as scanned from a transcript.
#[derive(Debug, Clone, Default)]
pub struct Run {
    pub turn: Turn,
    pub parts: Vec<Part>,
}

struct Scan {
    runs: Vec<Run>,
    /// The scan stopped at its limit before the end of the file.
    cut: bool,
}

fn str_of<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// The text a user entry carries itself; `None` for a tool result.
fn user_text(v: &Value) -> Option<String> {
    match &v["message"]["content"] {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) if !a.is_empty() && a.iter().all(|b| b["type"] != "tool_result") => {
            let t: Vec<&str> = a.iter().filter_map(|b| b["text"].as_str()).collect();
            Some(t.join("\n"))
        }
        _ => None,
    }
}

/// Whether a person typed it, rather than Claude Code.
fn human(v: &Value, text: &str) -> bool {
    let origin = v["origin"]["kind"].as_str();
    origin.is_none_or(|k| k == "human") && !crate::hook::injected(text)
}

/// The prompt that starts a new turn, given the promptId of the turn so far.
fn turn_start(v: &Value, current: Option<&str>) -> Option<String> {
    let flag = |k: &str| v[k].as_bool().unwrap_or(false);
    if v["type"] != "user"
        || ["isMeta", "isSidechain", "isCompactSummary"]
            .into_iter()
            .any(flag)
        || flag("isVisibleInTranscriptOnly")
        || str_of(v, "uuid").is_empty()
    {
        return None;
    }
    let text = user_text(v)?;
    let pid = v["promptId"].as_str();
    (pid.is_none() || pid != current).then_some(text)
}

fn add_ids(to: &mut Vec<String>, ids: impl IntoIterator<Item = String>) {
    for i in ids {
        if !to.contains(&i) {
            to.push(i);
        }
    }
}

fn prompted_ids(text: &str) -> Vec<String> {
    segments(text).into_iter().filter_map(|s| s.task).collect()
}

/// The task ids in a worker's description, by the agent id or name `SendMessage` addressed.
fn worker_ids(transcript: &Path, to: &str) -> Vec<String> {
    let dir = transcript.with_extension("").join("subagents");
    let desc = |p: &Path| -> Option<(String, String)> {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
        Some((str_of(&v, "name").into(), str_of(&v, "description").into()))
    };
    if valid_id(to)
        && let Some((_, d)) = desc(&dir.join(format!("agent-{to}.meta.json")))
    {
        return ids_in(&d);
    }
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().ends_with(".meta.json")
            && let Some((n, d)) = desc(&e.path())
            && n == to
        {
            add_ids(&mut out, ids_in(&d));
        }
    }
    out
}

/// The tasks one tool call writes to.
fn call_writes(transcript: &Path, name: &str, input: &Value) -> Vec<Write> {
    let of = |ids: Vec<String>| {
        ids.into_iter()
            .map(|task| Write {
                task,
                what: name.into(),
            })
            .collect()
    };
    match name {
        "Bash" => tracker_writes(str_of(input, "command"))
            .into_iter()
            .map(|(task, what)| Write { task, what })
            .collect(),
        "SendMessage" => {
            let mut ids = worker_ids(transcript, str_of(input, "to"));
            add_ids(&mut ids, ids_in(str_of(input, "to")));
            of(ids)
        }
        "Agent" | "Task" => of(ids_in(str_of(input, "description"))),
        _ => vec![],
    }
}

impl Run {
    fn add(&mut self, v: &Value, line: &str, end: u64, transcript: &Path) {
        self.turn.end = end;
        let ts = v["timestamp"].as_str().map(str::to_string);
        if ts.is_some() {
            self.turn.ended = ts.clone();
        }
        let att = &v["attachment"];
        if v["type"] == "attachment" && att["type"] == "queued_command" {
            let text = str_of(att, "prompt");
            if human(att, text) {
                add_ids(&mut self.turn.prompted, prompted_ids(text));
                self.parts.push(Part::Prompt(ts, text.into()));
            }
        }
        if v["type"] != "assistant" || v["isSidechain"] == true {
            return;
        }
        self.turn.last = str_of(v, "uuid").into();
        for b in v["message"]["content"].as_array().into_iter().flatten() {
            match str_of(b, "type") {
                "text" => {
                    let t = str_of(b, "text");
                    let ids = sections(t).into_iter().flat_map(|s| s.0);
                    add_ids(&mut self.turn.sections, ids);
                }
                "tool_use" => {
                    for w in call_writes(transcript, str_of(b, "name"), &b["input"]) {
                        if !self.turn.writes.contains(&w) {
                            self.turn.writes.push(w);
                        }
                    }
                }
                _ => {}
            }
        }
        self.parts
            .extend(parse_line(line).into_iter().map(|e| match e.kind {
                Kind::Tool | Kind::ToolResult => Part::Tool(e),
                _ => Part::Reply(e.ts, e.body),
            }));
    }
}

/// Split a transcript into turns from byte `from`. Entries before the first turn start belong to
/// an earlier turn. With `one`, stop after the first turn; `only` skips to the turn that prompt
/// started.
fn scan(path: &Path, from: u64, one: bool, only: Option<&str>, max: u64) -> std::io::Result<Scan> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut pos = f.seek(SeekFrom::Start(from.min(len)))?;
    let stop = pos.saturating_add(max);
    let mut r = BufReader::new(f);
    let mut out = Scan {
        runs: vec![],
        cut: false,
    };
    let mut pid: Option<String> = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)? as u64;
        if n == 0 {
            break;
        }
        if pos + n > stop {
            out.cut = true;
            break;
        }
        let at = pos;
        pos += n;
        let line = String::from_utf8_lossy(&buf);
        // A line still being written does not parse; the next read takes it.
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(text) = turn_start(&v, pid.as_deref()) {
            pid = v["promptId"].as_str().map(str::to_string);
            if one && !out.runs.is_empty() {
                break;
            }
            let uuid = str_of(&v, "uuid");
            if only.is_some_and(|o| o != uuid) {
                continue;
            }
            let ts = v["timestamp"].as_str().map(str::to_string);
            let mut run = Run::default();
            run.turn.transcript = path.to_string_lossy().into_owned();
            run.turn.start = at;
            run.turn.end = pos;
            run.turn.prompt = uuid.into();
            run.turn.started = ts.clone().unwrap_or_default();
            if human(&v, &text) {
                add_ids(&mut run.turn.prompted, prompted_ids(&text));
                run.parts.push(Part::Prompt(ts, text));
            }
            out.runs.push(run);
            continue;
        }
        if let Some(run) = out.runs.last_mut() {
            run.add(&v, &line, pos, path);
        }
    }
    Ok(out)
}

/// A turn read back from its transcript: at its range when the prompt is still there, else
/// wherever its prompt is now. A running turn is the last one after its starting point.
pub fn read_parts(t: &Turn) -> Result<Vec<Run>> {
    let path = Path::new(&t.transcript);
    let open = || format!("read {}", t.transcript);
    if t.prompt.is_empty() {
        return Ok(scan(path, t.start, false, None, MAX_SCAN)
            .with_context(open)?
            .runs);
    }
    let here = scan(path, t.start, true, None, MAX_SCAN).with_context(open)?;
    if here.runs.first().is_some_and(|r| r.turn.prompt == t.prompt) {
        return Ok(here.runs);
    }
    let found = scan(path, 0, true, Some(&t.prompt), u64::MAX).with_context(open)?;
    anyhow::ensure!(!found.runs.is_empty(), "turn not found in {}", t.transcript);
    Ok(found.runs)
}

fn dir(store: &Store) -> PathBuf {
    store.root.join("turns")
}

fn open_path(store: &Store, session: &str) -> PathBuf {
    dir(store).join(format!("{session}.open.json"))
}

fn write(path: &Path, t: &Turn) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("turn path has no parent")?)?;
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(t)? + "\n")?;
    std::fs::rename(&tmp, path).context("write turn")?;
    Ok(())
}

fn read(path: &Path) -> Option<Turn> {
    let rec = parse_record(&std::fs::read_to_string(path).ok()?)?;
    serde_json::from_value(Value::Object(rec)).ok()
}

fn files(store: &Store) -> Vec<(PathBuf, Turn)> {
    let mut v: Vec<(PathBuf, Turn)> = std::fs::read_dir(dir(store))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| read(&p).map(|t| (p, t)))
        .collect();
    v.sort_by(|a, b| a.1.started.cmp(&b.1.started));
    v
}

/// Every recorded turn, oldest first, the running ones included.
pub fn all(store: &Store) -> Vec<Turn> {
    files(store).into_iter().map(|(_, t)| t).collect()
}

/// `UserPromptSubmit`: note where the turn starts and which tasks its prompt names.
pub fn open(store: &Store, session: &str, transcript: &str, prompt: &str) -> Result<()> {
    anyhow::ensure!(valid_id(session), "bad session id {session:?}");
    if transcript.is_empty() {
        return Ok(());
    }
    let len = std::fs::metadata(transcript).map(|m| m.len()).unwrap_or(0);
    store.locked(|| {
        let p = open_path(store, session);
        // Still there: the last turn was interrupted, or this message was queued into it.
        let mut t = read(&p)
            .filter(|t| t.transcript == transcript)
            .unwrap_or_else(|| Turn {
                session: session.into(),
                transcript: transcript.into(),
                start: len,
                started: now(),
                ..Default::default()
            });
        if !crate::hook::injected(prompt) {
            add_ids(&mut t.prompted, prompted_ids(prompt));
        }
        write(&p, &t)
    })?
}

/// `Stop`: turn the scanned range into one record per turn that is about a task or wrote to one.
pub fn close(store: &Store, session: &str, transcript: &str) -> Result<Vec<Turn>> {
    anyhow::ensure!(valid_id(session), "bad session id {session:?}");
    let op = open_path(store, session);
    let anchor = store.locked(|| read(&op))?;
    let file = match &anchor {
        Some(a) => a.transcript.clone(),
        None => transcript.to_string(),
    };
    if file.is_empty() {
        return Ok(vec![]);
    }
    let path = Path::new(&file);
    let found = match &anchor {
        Some(a) => scan(path, a.start, false, None, MAX_SCAN),
        None => scan_last(path),
    };
    // The prompt was already in the file when the hook measured it.
    let found = match found {
        Ok(s) if s.runs.is_empty() && !s.cut => scan_last(path),
        x => x,
    };
    let mut turns = match found {
        Ok(s) => {
            let mut v: Vec<Turn> = s.runs.into_iter().map(|r| r.turn).collect();
            let limit = s
                .cut
                .then(|| "the turn is too long to read in full".to_string());
            match (v.last_mut(), limit) {
                (Some(t), l) => t.error = l,
                (None, Some(l)) => v.extend(unread(&anchor, l)),
                (None, None) => v.extend(unread(&anchor, "no prompt found".into())),
            }
            v
        }
        Err(e) => unread(&anchor, e.to_string()).into_iter().collect(),
    };
    turns.retain(|t| !(t.prompted.is_empty() && t.sections.is_empty() && t.writes.is_empty()));
    store.locked(|| -> Result<()> {
        for t in &mut turns {
            t.session = session.into();
            let key = if valid_id(&t.prompt) {
                t.prompt.clone()
            } else {
                format!("unread-{}", t.start)
            };
            write(&dir(store).join(format!("{session}.{key}.json")), t)?;
        }
        // A prompt submitted while this ran has already moved the anchor's ids on; keep those.
        if read(&op) == anchor {
            let _ = std::fs::remove_file(&op);
        }
        Ok(())
    })??;
    Ok(turns)
}

/// The last turn in the file's final `TAIL` bytes.
fn scan_last(path: &Path) -> std::io::Result<Scan> {
    let len = std::fs::metadata(path)?.len();
    let mut s = scan(path, len.saturating_sub(TAIL), false, None, TAIL)?;
    let n = s.runs.len().saturating_sub(1);
    s.runs.drain(..n);
    Ok(s)
}

/// What is known of a turn whose transcript could not be scanned.
fn unread(anchor: &Option<Turn>, why: String) -> Option<Turn> {
    let mut t = anchor.clone()?;
    t.ended = Some(now());
    t.error = Some(why);
    Some(t)
}

/// Remove records older than `KEEP`. Returns how many went.
pub fn prune(store: &Store) -> usize {
    let at = Utc::now();
    files(store)
        .into_iter()
        .filter(|(p, t)| {
            crate::util::parse_ts(&t.started)
                .is_some_and(|s| (at - s).to_std().is_ok_and(|a| a > KEEP))
                && std::fs::remove_file(p).is_ok()
        })
        .count()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::store::tests::{Scratch, scratch};
    use serde_json::json;

    /// A transcript under construction, with lines shaped like Claude Code's.
    pub struct Log {
        pub path: PathBuf,
        n: u32,
        pid: String,
    }

    impl Log {
        pub fn new(t: &Scratch) -> Self {
            Log {
                path: t.0.join("s.jsonl"),
                n: 0,
                pid: String::new(),
            }
        }

        pub fn path(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }

        pub fn raw(&self, s: &str) {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .unwrap();
            f.write_all(s.as_bytes()).unwrap();
        }

        fn push(&mut self, mut v: Value) -> String {
            self.n += 1;
            let id = format!("u-{}", self.n);
            v["uuid"] = json!(id);
            v["timestamp"] = json!(format!("2026-11-23T14:{:02}:00.000Z", self.n));
            self.raw(&format!("{v}\n"));
            id
        }

        pub fn prompt(&mut self, text: &str) -> String {
            self.pid = format!("p-{}", self.n);
            let origin = if crate::hook::injected(text) {
                "task-notification"
            } else {
                "human"
            };
            let v = json!({"type":"user","promptId":self.pid,"origin":{"kind":origin},
                "message":{"content":text}});
            self.push(v)
        }

        pub fn queued(&mut self, text: &str) {
            self.push(
                json!({"type":"attachment","attachment":{"type":"queued_command",
                "prompt":text,"origin":{"kind":"human"}}}),
            );
        }

        pub fn say(&mut self, text: &str) -> String {
            self.push(json!({"type":"assistant","message":{"content":[
                {"type":"text","text":text}]}}))
        }

        pub fn call(&mut self, name: &str, input: Value) {
            self.push(json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","name":name,"input":input}]}}));
            let v = json!({"type":"user","promptId":self.pid,"message":{"content":[
                {"type":"tool_result","content":"ok"}]}});
            self.push(v);
            let v = json!({"type":"user","promptId":self.pid,"isMeta":true,
                "message":{"content":"[fermata ctx-99] skill text, not a prompt"}});
            self.push(v);
        }

        pub fn bash(&mut self, cmd: &str) {
            self.call("Bash", json!({"command": cmd}));
        }
    }

    fn turn(t: &Scratch, l: &mut Log, prompt: &str, f: impl FnOnce(&mut Log)) -> Vec<Turn> {
        open(&t.1, "s-1", &l.path(), prompt).unwrap();
        l.prompt(prompt);
        f(l);
        close(&t.1, "s-1", &l.path()).unwrap()
    }

    #[test]
    fn a_turn_is_filed_under_every_segment_and_section() {
        let t = scratch("turns-file");
        let mut l = Log::new(&t);
        l.prompt("old");
        l.say("### ctx-50\nan earlier turn");
        let v = turn(
            &t,
            &mut l,
            "[fermata ctx-12] what?\n[fermata ctx-13 d1 answer] yes",
            |l| {
                l.bash("$TRK note ctx-7 \"x\"");
                l.queued("[fermata ctx-14] and this");
                l.say("### ctx-3\ndone\n### `ctx-1v.2`: waiting");
            },
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].prompted, ["ctx-12", "ctx-13", "ctx-14"]);
        assert_eq!(v[0].sections, ["ctx-3", "ctx-1v.2"]);
        assert_eq!(v[0].unattributed(&["ctx-7"]), ["note"]);
        assert!(v[0].unattributed(&["ctx-1v"]).is_empty());
        assert!(!v[0].attributed(&["ctx-1"]) && !v[0].attributed(&["ctx-99"]));
        assert_eq!(all(&t.1), v);
        assert!(!open_path(&t.1, "s-1").exists());
        // The range starts at the prompt and reads back as the same turn.
        assert_eq!(read_parts(&v[0]).unwrap()[0].turn.prompt, v[0].prompt);
    }

    #[test]
    fn stop_again_rewrites_the_same_turn() {
        let t = scratch("turns-again");
        let mut l = Log::new(&t);
        turn(&t, &mut l, "[fermata ctx-1] go", |l| {
            l.say("one");
        });
        l.bash("tracker.sh state ctx-2 working");
        let v = close(&t.1, "s-1", &l.path()).unwrap();
        assert_eq!(all(&t.1), v);
        assert_eq!(v[0].unattributed(&["ctx-2"]), ["state"]);
    }

    #[test]
    fn an_interrupted_turn_is_still_filed() {
        let t = scratch("turns-int");
        let mut l = Log::new(&t);
        open(&t.1, "s-1", &l.path(), "[fermata ctx-1] a").unwrap();
        l.prompt("[fermata ctx-1] a");
        l.bash("tracker.sh close ctx-5");
        let running = all(&t.1);
        assert!(running[0].is_open() && running[0].prompted_for(&["ctx-1"]));
        assert_eq!(read_parts(&running[0]).unwrap()[0].turn.writes.len(), 1);
        let v = turn(&t, &mut l, "[fermata ctx-2] b", |l| {
            l.say("ok");
        });
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].prompted, ["ctx-1"]);
        assert_eq!(v[1].prompted, ["ctx-2"]);
    }

    #[test]
    fn nothing_about_a_task_keeps_nothing() {
        let t = scratch("turns-none");
        let mut l = Log::new(&t);
        let v = turn(&t, &mut l, "hello", |l| {
            l.bash("ls; tracker.sh get ctx-1");
            l.say("```\n### ctx-3\n```\nhi");
        });
        assert!(v.is_empty() && all(&t.1).is_empty());
    }

    #[test]
    fn injected_prompts_are_not_segments() {
        let t = scratch("turns-inj");
        let mut l = Log::new(&t);
        let p = "<task-notification>\n[fermata ctx-4] quoted\n</task-notification>";
        let v = turn(&t, &mut l, p, |l| {
            l.say("### ctx-3\nworker is done");
        });
        assert!(v[0].prompted.is_empty());
        assert_eq!(v[0].sections, ["ctx-3"]);
    }

    #[test]
    fn workers_written_to_are_found_by_agent_id_or_name() {
        let t = scratch("turns-send");
        let mut l = Log::new(&t);
        let d = l.path.with_extension("").join("subagents");
        std::fs::create_dir_all(&d).unwrap();
        let meta = json!({"description":"ctx-8: build","name":"builder"});
        std::fs::write(d.join("agent-a1b2.meta.json"), meta.to_string()).unwrap();
        for to in ["a1b2", "builder"] {
            let v = turn(&t, &mut l, "carry on", |l| {
                l.call("SendMessage", json!({"to": to, "message": "ctx-77"}));
                l.call("Agent", json!({"description": "ctx-9: review"}));
            });
            assert_eq!(v[0].unattributed(&["ctx-8"]), ["SendMessage"], "{to}");
            assert_eq!(v[0].unattributed(&["ctx-9"]), ["Agent"]);
            assert!(v[0].unattributed(&["ctx-77"]).is_empty());
        }
        let v = turn(&t, &mut l, "x", |l| {
            l.call("SendMessage", json!({"to": "../../etc", "message": "m"}));
        });
        assert!(v.is_empty());
    }

    #[test]
    fn unreadable_transcripts_leave_a_marked_record() {
        let t = scratch("turns-gone");
        open(&t.1, "s-1", "/nope/x.jsonl", "[fermata ctx-1] a").unwrap();
        let v = close(&t.1, "s-1", "/nope/x.jsonl").unwrap();
        assert!(v[0].error.is_some() && v[0].prompted_for(&["ctx-1"]) && !v[0].is_open());
        assert_eq!(all(&t.1), v);
        open(&t.1, "s-1", "/nope/x.jsonl", "no task").unwrap();
        assert!(close(&t.1, "s-1", "/nope/x.jsonl").unwrap().is_empty());
        assert!(close(&t.1, "../x", "/nope").is_err());
    }

    #[test]
    fn truncated_and_foreign_lines_are_skipped() {
        let t = scratch("turns-junk");
        let mut l = Log::new(&t);
        l.raw("{\"type\":\"user\",\"mess\n[]\n\"x\"\n\u{0}\n");
        let v = turn(&t, &mut l, "[fermata ctx-1] a", |l| {
            l.raw("{\"type\":\"assistant\",\"message\":{\"content\":\n");
            l.raw("{\"type\":\"assistant\",\"message\":7}\n");
            l.say("### ctx-2\nok");
            l.raw("{\"type\":\"assistant\",\"mess");
        });
        assert_eq!(v[0].sections, ["ctx-2"]);
    }

    #[test]
    fn a_turn_is_found_again_when_the_file_changed() {
        let t = scratch("turns-moved");
        let mut l = Log::new(&t);
        l.prompt("first");
        let v = turn(&t, &mut l, "[fermata ctx-1] a", |l| {
            l.say("ok");
        });
        // The reply can land after Stop; the next turn is not part of it.
        l.say("late");
        l.prompt("next");
        l.say("other");
        let replies = |t: &Turn| {
            let r = read_parts(t).unwrap();
            let n = r[0].parts.iter();
            n.filter(|p| matches!(p, Part::Reply(..))).count()
        };
        assert_eq!(replies(&v[0]), 2);
        let old = std::fs::read_to_string(&l.path).unwrap();
        std::fs::write(&l.path, format!("{{\"type\":\"mode\"}}\n{old}")).unwrap();
        assert_eq!(replies(&v[0]), 2);
        std::fs::write(&l.path, "{}\n").unwrap();
        assert!(read_parts(&v[0]).is_err());
    }

    #[test]
    fn a_prompt_already_in_the_file_is_found() {
        let t = scratch("turns-late");
        let mut l = Log::new(&t);
        l.prompt("[fermata ctx-1] a");
        open(&t.1, "s-1", &l.path(), "[fermata ctx-1] a").unwrap();
        l.say("ok");
        let v = close(&t.1, "s-1", &l.path()).unwrap();
        assert_eq!(v[0].prompted, ["ctx-1"]);
        assert!(v[0].error.is_none());
        // Without a starting point only the last turn is taken.
        l.prompt("[fermata ctx-2] b");
        l.say("ok");
        let v = close(&t.1, "s-1", &l.path()).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].prompted, ["ctx-2"]);
    }

    #[test]
    fn old_records_go() {
        let t = scratch("turns-prune");
        for (id, at) in [("a", "2020-01-01T00:00:00Z".to_string()), ("b", now())] {
            let r = Turn {
                started: at,
                ..Default::default()
            };
            write(&dir(&t.1).join(format!("s-1.{id}.json")), &r).unwrap();
        }
        std::fs::write(dir(&t.1).join("junk.json"), "{").unwrap();
        assert_eq!(prune(&t.1), 1);
        assert_eq!(all(&t.1).len(), 1);
    }
}
