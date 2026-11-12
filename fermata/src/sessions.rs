//! ccmon session records (read-only) and background-worker (subagent) discovery.
use crate::model::{Session, Worker};
use crate::util::expand_tilde;
use std::path::{Path, PathBuf};

pub fn ccmon_dir() -> PathBuf {
    std::env::var("FERMATA_CCMON_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_tilde("~/code/claude-monitor"))
}

pub fn load_dir(dir: &Path, closed: bool) -> Vec<Session> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut v: Vec<Session> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let mut s: Session =
                serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            s.closed = closed;
            Some(s)
        })
        .collect();
    v.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
    v
}

pub fn live_sessions() -> Vec<Session> {
    load_dir(&ccmon_dir().join("sessions"), false)
}

pub fn closed_sessions() -> Vec<Session> {
    load_dir(&ccmon_dir().join("closed"), true)
}

pub fn same_path(a: &str, b: &str) -> bool {
    !a.is_empty() && a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The live session whose cwd is one of `worktrees`.
pub fn session_for<'a>(sessions: &'a [Session], worktrees: &[&str]) -> Option<&'a Session> {
    sessions
        .iter()
        .find(|s| worktrees.iter().any(|w| same_path(&s.worktree, w)))
}

#[derive(Debug, Clone)]
pub struct WorkerMeta {
    pub description: String,
    pub transcript: PathBuf,
    pub finished: bool,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

pub fn projects_dir() -> PathBuf {
    std::env::var("FERMATA_PROJECTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_tilde("~/.claude/projects"))
}

/// Every subagent `.meta.json` under `<projects>/*/<session>/subagents/`.
pub fn scan_workers(projects: &Path) -> Vec<WorkerMeta> {
    let mut out = vec![];
    let Ok(projs) = std::fs::read_dir(projects) else {
        return out;
    };
    for p in projs.flatten() {
        let Ok(sess) = std::fs::read_dir(p.path()) else {
            continue;
        };
        for s in sess.flatten() {
            let sub = s.path().join("subagents");
            let Ok(metas) = std::fs::read_dir(&sub) else {
                continue;
            };
            for m in metas.flatten() {
                let path = m.path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let Some(stem) = name.strip_suffix(".meta.json") else {
                    continue;
                };
                let Ok(txt) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else {
                    continue;
                };
                let transcript = sub.join(format!("{stem}.jsonl"));
                let modified = std::fs::metadata(&transcript)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(chrono::DateTime::<chrono::Utc>::from);
                out.push(WorkerMeta {
                    description: v["description"].as_str().unwrap_or("").to_string(),
                    finished: v["finished"].as_bool().unwrap_or(false),
                    transcript,
                    modified,
                });
            }
        }
    }
    out
}

/// Does a worker description name this task? `<id>:` is the convention; a leading ticket
/// key is accepted as a fallback for workers launched before it.
pub fn worker_matches(desc: &str, task_id: &str, ticket: Option<&str>) -> bool {
    let starts = |p: &str| {
        desc.strip_prefix(p).is_some_and(|r| {
            r.is_empty() || r.starts_with(':') || r.starts_with(' ') || r.starts_with(',')
        })
    };
    starts(task_id) || ticket.is_some_and(starts)
}

pub fn workers_for(all: &[WorkerMeta], task_id: &str, ticket: Option<&str>) -> Vec<Worker> {
    let mut v: Vec<&WorkerMeta> = all
        .iter()
        .filter(|w| worker_matches(&w.description, task_id, ticket))
        .collect();
    v.sort_by_key(|w| std::cmp::Reverse(w.modified));
    v.into_iter()
        .map(|w| Worker {
            description: w.description.clone(),
            transcript: w.transcript.to_string_lossy().into_owned(),
            finished: w.finished,
            modified: w.modified.map(|m| m.to_rfc3339()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching() {
        assert!(worker_matches("ctx-01z: review #4602", "ctx-01z", None));
        assert!(!worker_matches("ctx-01zz: x", "ctx-01z", None));
        assert!(worker_matches("ABC-123 dedupe", "ctx-1kj", Some("ABC-123")));
        assert!(!worker_matches("ABC-1234 x", "ctx-1kj", Some("ABC-123")));
    }

    #[test]
    fn scan_layout() {
        let root = std::env::temp_dir().join(format!("fermata-proj-{}", std::process::id()));
        let sub = root.join("-Users-x/sess-1/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("agent-a1.meta.json"),
            r#"{"description":"ctx-9: build","finished":true}"#,
        )
        .unwrap();
        std::fs::write(sub.join("agent-a1.jsonl"), "{}\n").unwrap();
        let w = scan_workers(&root);
        assert_eq!(w.len(), 1);
        assert!(w[0].finished);
        assert_eq!(workers_for(&w, "ctx-9", None).len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn session_match() {
        let s = vec![Session {
            worktree: "/a/b".into(),
            ..Default::default()
        }];
        assert!(session_for(&s, &["/a/b/"]).is_some());
        assert!(session_for(&s, &["/a"]).is_none());
    }
}
