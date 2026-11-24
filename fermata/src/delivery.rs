//! Delivery records: what happened to each routed answer, and whether its session received it.
//!
//! One JSON file per delivery under `<store root>/deliveries/`. fermata writes one when it
//! routes an answer; the `user-prompt-submit` hook marks it received, and the TUI cross-checks
//! the session's transcript. See "Answer delivery" in DESIGN.md.
use crate::log;
use crate::model::Session;
use crate::store::{Store, now, parse_record, valid_id};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a delivery to an idle session may go unacknowledged before it is flagged.
pub const RECEIPT_WINDOW: Duration = Duration::from_secs(30);
/// A delivery queued behind a busy session is flagged once it has waited this long.
pub const QUEUE_WINDOW: Duration = Duration::from_secs(10 * 60);
/// The TUI stops looking for a delivery in its transcript after this long.
pub const LOOK_WINDOW: Duration = Duration::from_secs(3600);
/// Records older than this are pruned.
pub const KEEP: Duration = Duration::from_secs(14 * 24 * 3600);
/// The transcript cross-check reads at most this much from the end of the file.
const TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Typed into an idle pane.
    #[default]
    Delivered,
    /// Typed into a working Claude Code pane, which queues it.
    Queued,
    /// Not typed: the pane is at a dialog that would eat the keystrokes. Sent once it closes.
    Held,
    /// No pane to type into; the tracker note is the only record.
    NoteOnly,
    /// Typing into the pane failed.
    Failed,
}

impl State {
    /// Typed into a pane, so a receipt is expected.
    pub fn sent(self) -> bool {
        matches!(self, State::Delivered | State::Queued)
    }

    /// The short form used in the ANSWER note's head and the log.
    pub fn verb(self) -> &'static str {
        match self {
            State::Delivered => "delivered to",
            State::Queued => "queued for",
            State::Held => "held for",
            State::NoteOnly => "note only",
            State::Failed => "send failed to",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Delivery {
    pub id: String,
    pub context: String,
    pub task: String,
    /// The answer as typed, on one line.
    pub text: String,
    /// What is typed into the pane: `text`, prefixed `[fermata <task>]` for the orchestrator.
    pub payload: String,
    /// `session`, `orchestrator` or `note`.
    pub route: String,
    pub session_id: String,
    /// The pane's address when routed (`o:1.2`), and its stable id (`%12`).
    pub pane: String,
    pub pane_id: String,
    pub state: State,
    /// Why it was held, noted only or failed.
    pub detail: String,
    pub created_at: String,
    /// When it was typed into the pane; a held delivery gets this when it is released.
    pub sent_at: Option<String>,
    pub received_at: Option<String>,
    /// `hook` (the session's UserPromptSubmit saw it) or `transcript` (found there later).
    pub received_via: Option<String>,
    /// The delivery id that resent this one.
    pub superseded_by: Option<String>,
    pub resend_of: Option<String>,
    /// The tracker decision it replied to, and as what (`answer` or `question`).
    pub decision: Option<String>,
    pub kind: Option<String>,
}

/// What the user should see for a delivery.
#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub label: String,
    /// Sent and still unacknowledged past its window (or failed): resend is offered.
    pub alarm: bool,
}

fn ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn hhmm(s: &str) -> String {
    ts(s)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| s.to_string())
}

impl Delivery {
    pub fn new(context: &str, task: &str, text: &str, payload: &str, route: &str) -> Self {
        let t = Utc::now();
        Delivery {
            id: format!(
                "{}-{:04x}",
                t.format("%Y%m%dT%H%M%S%6f"),
                std::process::id() & 0xffff
            ),
            context: context.into(),
            task: task.into(),
            text: text.into(),
            payload: payload.into(),
            route: route.into(),
            created_at: now(),
            ..Default::default()
        }
    }

    /// How long ago it was typed (or created, if never typed).
    pub fn age(&self, at: DateTime<Utc>) -> Duration {
        let from = self.sent_at.as_deref().unwrap_or(&self.created_at);
        ts(from)
            .and_then(|f| (at - f).to_std().ok())
            .unwrap_or_default()
    }

    /// Typed, not yet seen by its session, and not replaced by a resend.
    pub fn pending(&self) -> bool {
        self.state.sent() && self.received_at.is_none() && self.superseded_by.is_none()
    }

    /// `session` is the target's live record, if it still has one.
    pub fn view(&self, session: Option<&Session>, at: DateTime<Utc>) -> View {
        let v = |label: String, alarm| View { label, alarm };
        let pane = if self.pane.is_empty() {
            "its pane"
        } else {
            &self.pane
        };
        if let Some(r) = &self.received_at {
            let via = self.received_via.as_deref().unwrap_or("hook");
            return v(format!("received {} ({via})", hhmm(r)), false);
        }
        if let Some(n) = &self.superseded_by {
            return v(format!("resent as {n}"), false);
        }
        match self.state {
            State::NoteOnly => v(format!("note only: {}", self.detail), false),
            State::Failed => v(format!("send failed: {}", self.detail), true),
            State::Held => v(
                format!("held: {pane} is at {}; sent when it closes", self.detail),
                false,
            ),
            State::Delivered | State::Queued => {
                let age = self.age(at);
                let busy = session.is_some_and(|s| s.status == "working");
                if age < RECEIPT_WINDOW {
                    v(format!("sent to {pane}"), false)
                } else if busy && age < QUEUE_WINDOW {
                    v(format!("queued: {pane} is busy"), false)
                } else {
                    let at = self.sent_at.as_deref().unwrap_or(&self.created_at);
                    v(format!("sent {}, not yet received", hhmm(at)), true)
                }
            }
        }
    }

    /// The outcome for the ANSWER note's head: no `)` (it would end the head).
    pub fn note_outcome(&self) -> String {
        let pane = self.pane.replace(['(', ')'], "");
        match self.state {
            State::NoteOnly => "note only".into(),
            s => format!("{} {}", s.verb(), pane.trim()),
        }
    }

    /// One info line for the log.
    pub fn log_line(&self, what: &str) -> String {
        let mut line = format!(
            "delivery {} {what}: task {} [{}] via {}",
            self.id, self.task, self.context, self.route
        );
        if !self.pane.is_empty() {
            line += &format!(" pane {} [{}]", self.pane, self.pane_id);
        }
        if !self.session_id.is_empty() {
            line += &format!(" session {}", short(&self.session_id));
        }
        line += &format!(" — {}", self.state.verb());
        if !self.detail.is_empty() {
            line += &format!(" ({})", self.detail);
        }
        line + &format!(" — {} chars", self.text.chars().count())
    }
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// The dialog that would eat keystrokes typed into this session's pane, if one is open.
pub fn open_prompt(s: &Session) -> Option<String> {
    match s.prompt_open.as_deref() {
        Some("permission") => Some("a permission prompt".into()),
        Some("elicitation") => Some("an input dialog".into()),
        Some(t) if !t.is_empty() => Some(format!("an {t} prompt")),
        _ => (s.status == "needs_permission").then(|| "a permission prompt".into()),
    }
}

fn dir(store: &Store) -> PathBuf {
    store.root.join("deliveries")
}

fn path(store: &Store, id: &str) -> PathBuf {
    dir(store).join(format!("{id}.json"))
}

pub fn write(store: &Store, d: &Delivery) -> Result<()> {
    anyhow::ensure!(valid_id(&d.id), "bad delivery id {:?}", d.id);
    let p = path(store, &d.id);
    std::fs::create_dir_all(dir(store))?;
    let tmp = p.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(d)? + "\n")?;
    std::fs::rename(&tmp, &p).context("write delivery")?;
    Ok(())
}

pub fn read(store: &Store, id: &str) -> Option<Delivery> {
    if !valid_id(id) {
        return None;
    }
    let rec = parse_record(&std::fs::read_to_string(path(store, id)).ok()?)?;
    serde_json::from_value(Value::Object(rec)).ok()
}

/// Every delivery, oldest first.
pub fn all(store: &Store) -> Vec<Delivery> {
    let Ok(rd) = std::fs::read_dir(dir(store)) else {
        return vec![];
    };
    let mut v: Vec<Delivery> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let rec = parse_record(&std::fs::read_to_string(e.path()).ok()?)?;
            serde_json::from_value(Value::Object(rec)).ok()
        })
        .collect();
    v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    v
}

/// Change one record under the store lock; `f` returns false to leave it untouched.
pub fn update(store: &Store, id: &str, f: impl FnOnce(&mut Delivery) -> bool) -> Result<bool> {
    store.locked(|| {
        let Some(mut d) = read(store, id) else {
            return Ok(false);
        };
        if !f(&mut d) {
            return Ok(false);
        }
        write(store, &d).map(|()| true)
    })?
}

/// Whitespace-insensitive, so a payload still matches after Claude Code re-wraps a prompt.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The `user-prompt-submit` hook: mark every pending delivery to this session (or pane) whose
/// payload appears in the prompt as received. Returns the ids marked.
pub fn mark_received(
    store: &Store,
    session_id: &str,
    pane_id: &str,
    prompt: &str,
) -> Result<Vec<String>> {
    let Ok(rd) = std::fs::read_dir(dir(store)) else {
        return Ok(vec![]);
    };
    if rd.into_iter().next().is_none() {
        return Ok(vec![]);
    }
    let prompt = squash(prompt);
    let t = now();
    let mut marked = vec![];
    store.locked(|| -> Result<()> {
        for mut d in all(store) {
            let ours = d.session_id == session_id || (!pane_id.is_empty() && d.pane_id == pane_id);
            if !(ours && d.pending() && prompt.contains(&squash(&d.payload))) {
                continue;
            }
            d.received_at = Some(t.clone());
            d.received_via = Some("hook".into());
            write(store, &d)?;
            log::info(d.log_line("received"));
            marked.push(d.id);
        }
        Ok(())
    })??;
    Ok(marked)
}

/// The transcript cross-check: true if a user entry at or after `since` contains `payload`.
/// UserPromptSubmit may not fire for input Claude Code folds into a running turn.
pub fn in_transcript(path: &Path, payload: &str, since: &str) -> bool {
    let Some(since) = ts(since) else {
        return false;
    };
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > TAIL_BYTES && f.seek(SeekFrom::Start(len - TAIL_BYTES)).is_err() {
        return false;
    }
    // Search the JSON-escaped form so the raw line can be filtered before parsing.
    let esc = serde_json::to_string(&squash(payload)).unwrap_or_default();
    let needle = esc.trim_matches('"');
    if needle.is_empty() {
        return false;
    }
    BufReader::new(f).lines().map_while(|l| l.ok()).any(|line| {
        if !line.contains(needle) {
            return false;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            return false;
        };
        let user = v["type"].as_str() == Some("user")
            || v["message"]["role"].as_str() == Some("user")
            || v["type"].as_str() == Some("attachment")
            || v["type"].as_str() == Some("queue-operation");
        let at = v["timestamp"].as_str().and_then(ts);
        user && at.is_some_and(|a| a >= since - chrono::Duration::seconds(2))
    })
}

/// Remove records older than `KEEP`. Returns how many went.
pub fn prune(store: &Store) -> usize {
    let at = Utc::now();
    all(store)
        .into_iter()
        .filter(|d| {
            ts(&d.created_at).is_some_and(|c| (at - c).to_std().is_ok_and(|a| a > KEEP))
                && std::fs::remove_file(path(store, &d.id)).is_ok()
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::scratch;

    fn sent(store: &Store, session: &str, payload: &str) -> Delivery {
        let mut d = Delivery::new("ctx", "ctx-1", "go", payload, "orchestrator");
        d.session_id = session.into();
        d.pane = "o:1.2".into();
        d.pane_id = "%2".into();
        d.state = State::Delivered;
        d.sent_at = Some(now());
        write(store, &d).unwrap();
        d
    }

    #[test]
    fn hook_marks_only_this_sessions_matching_delivery() {
        let sc = scratch("deliv-mark");
        let a = sent(&sc.1, "s-1", "[fermata ctx-1] go");
        let mut b = sent(&sc.1, "s-2", "[fermata ctx-2] stop");
        b.id.push('b');
        write(&sc.1, &b).unwrap();
        // Two queued messages merged into one prompt, re-wrapped.
        let prompt = "[fermata ctx-9] other\n[fermata   ctx-1]\ngo";
        let m = mark_received(&sc.1, "s-1", "", prompt).unwrap();
        assert_eq!(m, vec![a.id.clone()]);
        let a2 = read(&sc.1, &a.id).unwrap();
        assert!(a2.received_at.is_some());
        assert_eq!(a2.received_via.as_deref(), Some("hook"));
        assert!(read(&sc.1, &b.id).unwrap().received_at.is_none());
        // Already received: not marked twice.
        assert!(mark_received(&sc.1, "s-1", "", prompt).unwrap().is_empty());
    }

    #[test]
    fn a_mangled_prefix_is_not_a_receipt() {
        let sc = scratch("deliv-mangled");
        let a = sent(&sc.1, "s-1", "[fermata art-12] yes");
        let m = mark_received(&sc.1, "s-1", "", "mta art-12 ] yes").unwrap();
        assert!(m.is_empty());
        assert!(read(&sc.1, &a.id).unwrap().pending());
    }

    #[test]
    fn the_pane_matches_when_the_session_id_changed() {
        let sc = scratch("deliv-pane");
        let a = sent(&sc.1, "s-old", "go");
        assert_eq!(
            mark_received(&sc.1, "s-new", "%2", "go").unwrap(),
            vec![a.id]
        );
    }

    #[test]
    fn views() {
        let at = Utc::now();
        let mut d = Delivery::new("c", "t", "go", "go", "session");
        d.pane = "a:1.1".into();
        d.state = State::Delivered;
        d.sent_at = Some(now());
        assert_eq!(d.view(None, at).label, "sent to a:1.1");
        let later = at + chrono::Duration::seconds(45);
        let idle = Session {
            status: "idle".into(),
            ..Default::default()
        };
        let busy = Session {
            status: "working".into(),
            ..Default::default()
        };
        let v = d.view(Some(&idle), later);
        assert!(v.alarm && v.label.contains("not yet received"), "{v:?}");
        assert_eq!(d.view(Some(&busy), later).label, "queued: a:1.1 is busy");
        assert!(d.view(Some(&busy), at + chrono::Duration::hours(1)).alarm);
        d.received_at = Some(now());
        assert!(d.view(Some(&idle), later).label.starts_with("received "));
        d.received_at = None;
        d.superseded_by = Some("x".into());
        assert!(!d.view(Some(&idle), later).alarm);
        d.superseded_by = None;
        d.state = State::Held;
        d.detail = "a permission prompt".into();
        assert!(!d.view(None, later).alarm);
    }

    #[test]
    fn note_outcome_never_closes_the_head() {
        let mut d = Delivery::new("c", "t", "go", "go", "session");
        d.pane = "o:1.3 (moved)".into();
        d.state = State::Queued;
        assert_eq!(d.note_outcome(), "queued for o:1.3 moved");
        d.state = State::NoteOnly;
        assert_eq!(d.note_outcome(), "note only");
    }

    #[test]
    fn open_prompts() {
        let mut s = Session::default();
        assert_eq!(open_prompt(&s), None);
        s.prompt_open = Some("AskUserQuestion".into());
        assert_eq!(open_prompt(&s).unwrap(), "an AskUserQuestion prompt");
        s.prompt_open = None;
        s.status = "needs_permission".into();
        assert_eq!(open_prompt(&s).unwrap(), "a permission prompt");
    }

    #[test]
    fn transcript_cross_check() {
        let sc = scratch("deliv-tx");
        let p = sc.0.join("t.jsonl");
        let before = "2026-11-23T10:00:00Z";
        let after = "2026-11-23T10:05:00Z";
        let line = |ts: &str, text: &str| {
            serde_json::json!({"type": "user", "timestamp": ts,
                "message": {"role": "user", "content": text}})
            .to_string()
        };
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n",
                line(before, "[fermata ctx-1] \"quoted\" go"),
                line(after, "something else")
            ),
        )
        .unwrap();
        let payload = "[fermata ctx-1] \"quoted\" go";
        assert!(in_transcript(&p, payload, "2026-11-23T09:59:00Z"));
        // Only entries after it was sent count.
        assert!(!in_transcript(&p, payload, "2026-11-23T10:01:00Z"));
        assert!(!in_transcript(&p, "absent", "2026-11-23T09:00:00Z"));
    }

    #[test]
    fn prune_drops_only_old_records() {
        let sc = scratch("deliv-prune");
        let fresh = sent(&sc.1, "s", "a");
        let mut old = Delivery::new("c", "t", "b", "b", "note");
        old.id = "old-1".into();
        old.created_at = "2020-01-01T00:00:00Z".into();
        write(&sc.1, &old).unwrap();
        assert_eq!(prune(&sc.1), 1);
        assert_eq!(all(&sc.1), vec![fresh]);
    }
}
