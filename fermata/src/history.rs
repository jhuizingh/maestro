//! A task's timeline: tracker notes, worker reports, ccmon session events and transcripts.
use crate::model::{JoinedTask, Session};
use crate::notes::split_notes;
use crate::sessions::{ccmon_dir, projects_dir, same_path};
use crate::transcript::worker_report;
use crate::util::{one_line, truncate};
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const LOG_TAIL: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Item {
    pub ts: Option<String>,
    pub source: String,
    pub title: String,
    pub body: String,
    pub transcript: Option<String>,
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

/// ccmon log lines look like `[2026-06-26T16:30:21+00:00] PROMPT: text`.
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

pub fn timeline(t: &JoinedTask, sessions: &[Session]) -> Vec<Item> {
    let mut items = vec![];
    for n in split_notes(&t.task.notes) {
        items.push(Item {
            ts: n.date.clone(),
            source: format!("note {}", n.kind.to_lowercase()),
            title: truncate(&one_line(&n.text), 200),
            body: n.text,
            transcript: None,
        });
    }
    for w in &t.workers {
        let report = worker_report(Path::new(&w.transcript));
        let state = if w.finished { "finished" } else { "running" };
        items.push(Item {
            ts: w.modified.clone(),
            source: "worker".into(),
            title: format!("{} ({state})", w.description),
            body: report.unwrap_or_else(|| "(no report yet)".into()),
            transcript: Some(w.transcript.clone()),
        });
    }
    let wts = t.worktrees();
    let logs = ccmon_dir().join("logs");
    for s in sessions
        .iter()
        .filter(|s| wts.iter().any(|w| same_path(&s.worktree, w)))
    {
        if let Some(txt) = tail(&logs.join(format!("{}.log", s.session_id)), LOG_TAIL) {
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
pub fn sort_key(i: &Item, updated: &str) -> String {
    let ts = i.ts.clone().unwrap_or_default();
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
        };
        let up = "2026-11-12T18:25:38Z";
        assert_eq!(sort_key(&it("2026-11-12"), up), up);
        assert_eq!(sort_key(&it("2026-11-10"), up), "2026-11-10T23:59:59");
        assert!(sort_key(&it("2026-11-12"), up).as_str() > "2026-11-12T17:55:51");
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
