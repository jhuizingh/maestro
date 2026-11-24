//! A task's timeline: tracker notes, worker reports, session-log events and transcripts.
use crate::model::{JoinedTask, Session};
use crate::notes::split_notes;
use crate::sessions::{projects_dir, same_path};
use crate::transcript::worker_report;
use crate::turns::{self, Turn};
use crate::util::{one_line, truncate};
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const LOG_TAIL: u64 = 64 * 1024;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Item {
    pub ts: Option<String>,
    pub source: String,
    pub title: String,
    pub body: String,
    pub transcript: Option<String>,
    /// Sorts the item when `ts` cannot (a note stamped with a time but no date).
    #[serde(skip)]
    pub sort_ts: Option<String>,
}

/// What a time-only note shows in the time column: the stamp as written, date unknown.
pub fn undated(time: &str) -> String {
    format!("?? {time}")
}

pub fn tail(path: &Path, n: u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(n);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut b = Vec::new();
    f.read_to_end(&mut b).ok()?;
    let s = String::from_utf8_lossy(&b).into_owned();
    Some(if start > 0 {
        s.split_once('\n')
            .map(|x| x.1.to_string())
            .unwrap_or_default()
    } else {
        s
    })
}

/// Session log lines look like `[2026-06-26T16:30:21+00:00] PROMPT: text`.
pub fn parse_log(text: &str, session_id: &str) -> Vec<Item> {
    text.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix('[')?;
            let (ts, msg) = rest.split_once("] ")?;
            if msg.contains("(ignored)") {
                return None;
            }
            Some(Item {
                ts: Some(ts.to_string()),
                source: "session".into(),
                title: truncate(&one_line(msg), 200),
                body: format!("session {session_id}\n\n{msg}"),
                transcript: None,
                sort_ts: None,
            })
        })
        .collect()
}

pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn session_transcript(s: &Session) -> Option<PathBuf> {
    if let Some(p) = &s.transcript_path {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    let p = projects_dir()
        .join(project_dir_name(&s.worktree))
        .join(format!("{}.jsonl", s.session_id));
    p.exists().then_some(p)
}

/// The task's notes as items, in blob order.
pub fn note_items(t: &JoinedTask) -> Vec<Item> {
    let mut items = vec![];
    // A time-only note sorts with the dated note before it, keeping its place in the blob.
    let mut prev_date: Option<String> = None;
    for n in split_notes(&t.task.notes) {
        let sort_ts = match (&n.date, &n.time) {
            (None, Some(_)) => prev_date.clone(),
            _ => None,
        };
        if n.date.is_some() {
            prev_date = n.date.clone();
        }
        items.push(Item {
            ts: n.date.clone().or_else(|| n.time.as_deref().map(undated)),
            source: format!("note {}", n.kind.to_lowercase()),
            title: truncate(&one_line(&n.text), 200),
            body: n.text,
            transcript: None,
            sort_ts,
        });
    }
    items
}

/// Turns that wrote to the task without being about it.
fn unattributed_items(t: &JoinedTask, turns: &[Turn]) -> Vec<Item> {
    let names = crate::conversation::names(t);
    turns
        .iter()
        .filter_map(|r| {
            let what = r.unattributed(&names);
            (!what.is_empty()).then(|| Item {
                ts: Some(r.started.clone()),
                source: "unattributed".into(),
                title: format!("{}: {}", r.unattributed_title(), what.join(", ")),
                body: format!("session {}\n\n{}", r.session, what.join("\n")),
                transcript: Some(r.transcript.clone()),
                sort_ts: None,
            })
        })
        .collect()
}

pub fn timeline(t: &JoinedTask, sessions: &[Session]) -> Vec<Item> {
    let store = crate::store::Store::open();
    let mut items = note_items(t);
    items.extend(unattributed_items(t, &turns::all(&store)));
    for w in &t.workers {
        let report = worker_report(Path::new(&w.transcript));
        let state = if w.finished { "finished" } else { "running" };
        items.push(Item {
            ts: w.modified.clone(),
            source: "worker".into(),
            title: format!("{} ({state})", w.description),
            body: report.unwrap_or_else(|| "(no report yet)".into()),
            transcript: Some(w.transcript.clone()),
            sort_ts: None,
        });
    }
    let wts = t.worktrees();
    for s in sessions
        .iter()
        .filter(|s| wts.iter().any(|w| same_path(&s.worktree, w)))
    {
        if let Some(txt) = tail(&store.log_path(&s.session_id), LOG_TAIL) {
            items.extend(parse_log(&txt, &s.session_id));
        }
        if let Some(p) = session_transcript(s) {
            items.push(Item {
                ts: s.last_activity.clone(),
                source: "session".into(),
                title: format!(
                    "session transcript {} ({})",
                    &s.session_id[..s.session_id.len().min(8)],
                    if s.closed { "closed" } else { &s.status }
                ),
                body: p.to_string_lossy().into_owned(),
                transcript: Some(p.to_string_lossy().into_owned()),
                sort_ts: None,
            });
        }
    }
    let updated = t.task.updated_at.clone().unwrap_or_default();
    let mut keyed: Vec<(String, usize, Item)> = items
        .into_iter()
        .enumerate()
        .map(|(n, i)| (sort_key(&i, &updated), n, i))
        .collect();
    // Same key: later in the notes blob (or log) is newer.
    keyed.sort_by(|a, b| (&b.0, b.1).cmp(&(&a.0, a.1)));
    keyed.into_iter().map(|(_, _, i)| i).collect()
}

/// Date-only note stamps sort at the end of their day, but never after the task's last update.
/// A time-only note takes its sort time from `sort_ts` (or none: oldest).
pub fn sort_key(i: &Item, updated: &str) -> String {
    let ts = match &i.sort_ts {
        Some(s) => s.clone(),
        None if i.ts.as_deref().is_some_and(|t| t.starts_with("??")) => String::new(),
        None => i.ts.clone().unwrap_or_default(),
    };
    if ts.len() == 10 {
        let eod = format!("{ts}T23:59:59");
        if !updated.is_empty() && updated.starts_with(&ts) && updated < eod.as_str() {
            return updated.to_string();
        }
        return eod;
    }
    ts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_lines() {
        let i = parse_log(
            "[2026-06-26T16:30:21+00:00] PROMPT: open it\n[x] NOTIFICATION (ignored) type=''\n  DEBUG: x\n",
            "s1",
        );
        assert_eq!(i.len(), 1);
        assert_eq!(i[0].title, "PROMPT: open it");
    }

    #[test]
    fn note_sort_key() {
        let it = |ts: &str| Item {
            ts: Some(ts.into()),
            source: String::new(),
            title: String::new(),
            body: String::new(),
            transcript: None,
            sort_ts: None,
        };
        let up = "2026-11-12T18:25:38Z";
        assert_eq!(sort_key(&it("2026-11-12"), up), up);
        assert_eq!(sort_key(&it("2026-11-10"), up), "2026-11-10T23:59:59");
        assert!(sort_key(&it("2026-11-12"), up).as_str() > "2026-11-12T17:55:51");
        // A time-only note sorts by its hint, else as the oldest; it never sorts as "?? …".
        let undated = |hint: Option<&str>| Item {
            sort_ts: hint.map(str::to_string),
            ..it(&super::undated("14:02Z"))
        };
        assert_eq!(
            sort_key(&undated(Some("2026-11-10T09:00Z")), up),
            "2026-11-10T09:00Z"
        );
        assert_eq!(sort_key(&undated(None), up), "");
    }

    #[test]
    fn time_only_note_keeps_its_place() {
        let mut t = crate::core::tests::jt("ctx-1");
        t.task.notes =
            "NOTE (2026-11-10T09:00Z): a\nDECISION (14:02Z): b?\nNOTE (2026-11-11T08:00Z): c"
                .into();
        t.branches.clear();
        let items = timeline(&t, &[]);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert!(
            titles[0].ends_with(": c") && titles[1].ends_with(": b?"),
            "{titles:?}"
        );
        assert_eq!(items[1].ts.as_deref(), Some("?? 14:02Z"));
    }

    #[test]
    fn project_dir() {
        assert_eq!(
            project_dir_name("/home/user/code/work-workspace"),
            "-home-user-code-work-workspace"
        );
        assert_eq!(project_dir_name("/a/b.c_d"), "-a-b-c-d");
    }
}
