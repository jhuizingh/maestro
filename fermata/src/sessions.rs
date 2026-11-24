//! Session records (via `store`) and background-worker (subagent) discovery.
use crate::model::{Session, Worker};
use crate::util::expand_tilde;
use regex::Regex;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

pub use crate::store::ccmon_dir;
use crate::store::{Kind, Store};

pub fn live_sessions() -> Vec<Session> {
    Store::open().sessions(Kind::Live)
}

/// Whether any of the store's `sessions/` dirs (fermata's, else ccmon's) can be read.
pub fn probe_sessions() -> Result<(), String> {
    let mut errs = vec![];
    for d in Store::open().live_dirs() {
        match std::fs::read_dir(&d) {
            Ok(_) => return Ok(()),
            Err(e) => errs.push(format!("{}: {e}", d.display())),
        }
    }
    Err(errs.join("; "))
}

pub fn closed_sessions() -> Vec<Session> {
    Store::open().sessions(Kind::Closed)
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
    /// Working directories recorded near the end of the transcript (latest last).
    pub cwds: Vec<String>,
}

/// How much of a transcript's end is read for `cwd`. Each entry records it after its message,
/// so the tail holds the latest one unless a single entry outgrows this.
const CWD_TAIL: u64 = 8 * 1024;

static CWD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""cwd"\s*:\s*"((?:[^"\\]|\\.)*)""#).unwrap());

/// Per transcript: its length when read and the cwds found. Finished transcripts never change,
/// so after the first scan only growing ones are read again.
type CwdCache = HashMap<PathBuf, (u64, Vec<String>)>;
static CWD_CACHE: LazyLock<Mutex<CwdCache>> = LazyLock::new(Default::default);

/// The distinct `cwd` values in the transcript's last `CWD_TAIL` bytes.
fn transcript_cwds(path: &Path, len: u64) -> Vec<String> {
    if let Some((l, v)) = CWD_CACHE.lock().unwrap().get(path)
        && *l == len
    {
        return v.clone();
    }
    let mut buf = Vec::new();
    let read = std::fs::File::open(path).and_then(|mut f| {
        f.seek(SeekFrom::Start(len.saturating_sub(CWD_TAIL)))?;
        f.read_to_end(&mut buf)
    });
    let mut out: Vec<String> = vec![];
    if read.is_ok() {
        for c in CWD.captures_iter(&String::from_utf8_lossy(&buf)) {
            let v = serde_json::from_str::<String>(&format!("\"{}\"", &c[1]))
                .unwrap_or_else(|_| c[1].to_string());
            out.retain(|x| x != &v);
            out.push(v);
        }
    }
    CWD_CACHE
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (len, out.clone()));
    out
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
                let meta = std::fs::metadata(&transcript).ok();
                let modified = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(chrono::DateTime::<chrono::Utc>::from);
                let cwds = meta
                    .map(|m| transcript_cwds(&transcript, m.len()))
                    .unwrap_or_default();
                out.push(WorkerMeta {
                    description: v["description"].as_str().unwrap_or("").to_string(),
                    finished: v["finished"].as_bool().unwrap_or(false),
                    transcript,
                    modified,
                    cwds,
                });
            }
        }
    }
    out
}

/// Characters that continue an id: a match must not be followed or preceded by one. A `.` only
/// continues it when more id follows (`ctx-1.2` is not `ctx-1`, but `ctx-1.` ends a sentence).
fn id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Whether `word` appears in `text` as a whole id, not as part of a longer one.
pub fn contains_id(text: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back();
        let mut after = text[i + word.len()..].chars();
        let ok_before = !before.is_some_and(|c| id_char(c) || c == '.');
        let ok_after = match after.next() {
            None => true,
            Some('.') => !after.next().is_some_and(id_char),
            Some(c) => !id_char(c),
        };
        ok_before && ok_after
    })
}

/// Does a worker description name this task? `<id>:` is the convention, but the id (or the
/// task's ticket key) anywhere as a whole word counts too.
pub fn worker_matches(desc: &str, task_id: &str, ticket: Option<&str>) -> bool {
    contains_id(desc, task_id) || ticket.is_some_and(|k| contains_id(desc, k))
}

/// Whether a worker ran inside one of the task's worktrees (or below one).
fn worker_in(w: &WorkerMeta, worktrees: &[&str]) -> bool {
    w.cwds.iter().any(|c| {
        worktrees.iter().any(|wt| {
            let wt = wt.trim_end_matches('/');
            !wt.is_empty()
                && c.trim_end_matches('/')
                    .strip_prefix(wt)
                    .is_some_and(|r| r.is_empty() || r.starts_with('/'))
        })
    })
}

pub fn workers_for(
    all: &[WorkerMeta],
    task_id: &str,
    ticket: Option<&str>,
    worktrees: &[&str],
) -> Vec<Worker> {
    let mut v: Vec<&WorkerMeta> = all
        .iter()
        .filter(|w| worker_matches(&w.description, task_id, ticket) || worker_in(w, worktrees))
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
        // Anywhere, as a whole word.
        assert!(worker_matches(
            "[XY] ctx-1v.2.8 resume and ship",
            "ctx-1v.2.8",
            None
        ));
        assert!(worker_matches("ship (ctx-9)", "ctx-9", None));
        assert!(worker_matches("finish ctx-9.", "ctx-9", None));
        assert!(!worker_matches("[XY] ctx-1v.2.8 resume", "ctx-1v.2", None));
        assert!(!worker_matches("[XY] ctx-1v.2.8 resume", "ctx-1v", None));
        assert!(!worker_matches("xctx-9 build", "ctx-9", None));
        assert!(!worker_matches("ctx-9-slug build", "ctx-9", None));
        assert!(!worker_matches("anything", "", None));
    }

    #[test]
    fn by_cwd() {
        let w = |cwds: &[&str]| WorkerMeta {
            description: "no id here".into(),
            transcript: PathBuf::from("/t.jsonl"),
            finished: false,
            modified: None,
            cwds: cwds.iter().map(|s| s.to_string()).collect(),
        };
        let all = vec![
            w(&["/home"]),
            w(&["/home", "/wt/ctx-9-x/src"]),
            w(&["/wt/ctx-9-xy"]),
        ];
        assert_eq!(workers_for(&all, "ctx-9", None, &["/wt/ctx-9-x/"]).len(), 1);
        assert!(workers_for(&all, "ctx-9", None, &[]).is_empty());
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
        std::fs::write(
            sub.join("agent-a1.jsonl"),
            "{\"message\":\"x\",\"cwd\":\"/home\"}\n{\"cwd\":\"/wt/a \\\"b\\\"\"}\n",
        )
        .unwrap();
        let w = scan_workers(&root);
        assert_eq!(w.len(), 1);
        assert!(w[0].finished);
        assert_eq!(w[0].cwds, vec!["/home", "/wt/a \"b\""]);
        assert_eq!(workers_for(&w, "ctx-9", None, &[]).len(), 1);
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
