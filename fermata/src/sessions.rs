//! Session records (via `store`) and background-worker (subagent) discovery.
use crate::model::{Session, Worker};
use crate::util::{Stamp, expand_tilde};
use regex::Regex;
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};

pub use crate::store::ccmon_dir;
use crate::store::{Kind, Store};

pub fn live_sessions() -> Vec<Session> {
    let mut v = Store::open().sessions(Kind::Live);
    for s in v.iter_mut().filter(|s| s.is_idle()) {
        s.recap = s.transcript_path.as_deref().and_then(recap);
    }
    v
}

/// The last recap read from each transcript, and the stamp it was read at.
type Recaps = HashMap<PathBuf, (Stamp, Option<String>)>;
static RECAPS: LazyLock<Mutex<Recaps>> = LazyLock::new(Default::default);

/// A transcript's recap, read again only when the file changed.
fn recap(path: &str) -> Option<String> {
    let p = expand_tilde(path);
    let stamp = Stamp::of(&std::fs::metadata(&p).ok()?);
    let mut cache = RECAPS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, r)) = cache.get(&p).filter(|(s, _)| *s == stamp && s.settled()) {
        return r.clone();
    }
    let r = crate::transcript::recap(&p);
    cache.insert(p, (stamp, r.clone()));
    r
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

/// The distinct `cwd` values in the transcript's last `CWD_TAIL` bytes.
fn transcript_cwds(path: &Path, len: u64) -> Vec<String> {
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
    out
}

pub fn projects_dir() -> PathBuf {
    std::env::var("FERMATA_PROJECTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_tilde("~/.claude/projects"))
}

/// A worker's meta file and transcript, as last read.
#[derive(Clone)]
struct Seen {
    stamp: Stamp,
    /// A later stat with the same stamp proves the meta file's content is the same.
    settled: bool,
    /// Description and finished; None when it is not valid JSON.
    meta: Option<(String, bool)>,
    /// The transcript's length when read, and the cwds found.
    cwds: Option<(u64, Vec<String>)>,
}

impl Seen {
    /// None when the meta file cannot be read.
    fn check(old: Option<Self>, path: &Path) -> Option<Self> {
        let stamp = Stamp::of(&std::fs::metadata(path).ok()?);
        if let Some(s) = &old
            && s.settled
            && s.stamp == stamp
        {
            return old;
        }
        let settled = stamp.settled();
        let txt = std::fs::read_to_string(path).ok()?;
        let v = serde_json::from_str::<serde_json::Value>(&txt).ok();
        Some(Self {
            stamp,
            settled,
            meta: v.map(|v| {
                (
                    v["description"].as_str().unwrap_or("").to_string(),
                    v["finished"].as_bool().unwrap_or(false),
                )
            }),
            cwds: old.and_then(|s| s.cwds),
        })
    }

    fn worker(&mut self, transcript: PathBuf) -> Option<WorkerMeta> {
        let (description, finished) = self.meta.clone()?;
        let stat = std::fs::metadata(&transcript).ok();
        // Finished transcripts never change, so only growing ones are read again.
        let cwds = stat.as_ref().map(|m| match &self.cwds {
            Some((len, v)) if *len == m.len() => v.clone(),
            _ => {
                let v = transcript_cwds(&transcript, m.len());
                self.cwds = Some((m.len(), v.clone()));
                v
            }
        });
        Some(WorkerMeta {
            description,
            finished,
            transcript,
            modified: stat.and_then(|m| m.modified().ok()).map(Into::into),
            cwds: cwds.unwrap_or_default(),
        })
    }
}

/// What the last scan of each projects directory read, by meta file.
type Scans = HashMap<PathBuf, HashMap<PathBuf, Seen>>;
static SCANS: LazyLock<Mutex<Scans>> = LazyLock::new(Default::default);

/// Every subagent `.meta.json` under `<projects>/*/<session>/subagents/`. Files are all found
/// and statted on every scan, but read only when new or changed.
pub fn scan_workers(projects: &Path) -> Vec<WorkerMeta> {
    let mut scans = SCANS.lock().unwrap_or_else(PoisonError::into_inner);
    let mut old = scans.remove(projects).unwrap_or_default();
    let mut now = HashMap::with_capacity(old.len());
    let mut out = Vec::with_capacity(old.len());
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
                let name = m.file_name();
                let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".meta.json")) else {
                    continue;
                };
                let path = m.path();
                let Some(mut seen) = Seen::check(old.remove(&path), &path) else {
                    continue;
                };
                out.extend(seen.worker(sub.join(format!("{stem}.jsonl"))));
                now.insert(path, seen);
            }
        }
    }
    scans.insert(projects.to_path_buf(), now);
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

/// Whether `cwd` is one of the worktrees or below one.
fn inside(cwd: &str, worktrees: &[&str]) -> bool {
    worktrees.iter().any(|wt| {
        let wt = wt.trim_end_matches('/');
        !wt.is_empty()
            && cwd
                .trim_end_matches('/')
                .strip_prefix(wt)
                .is_some_and(|r| r.is_empty() || r.starts_with('/'))
    })
}

/// The leading run of id characters in an id: what any text naming it has as a whole piece.
fn head(id: &str) -> &str {
    id.split(|c| !id_char(c)).next().unwrap_or("")
}

/// Every worker, findable by task without looking at each one.
#[derive(Default)]
pub struct Workers {
    all: Vec<WorkerMeta>,
    /// Workers by the pieces (runs of id characters) of their description.
    pieces: HashMap<String, Vec<usize>>,
    cwds: HashMap<String, Vec<usize>>,
}

impl From<Vec<WorkerMeta>> for Workers {
    fn from(all: Vec<WorkerMeta>) -> Self {
        let mut w = Self::default();
        for (i, m) in all.iter().enumerate() {
            for p in m.description.split(|c| !id_char(c)) {
                push(&mut w.pieces, p, i);
            }
            for c in &m.cwds {
                push(&mut w.cwds, c, i);
            }
        }
        w.all = all;
        w
    }
}

fn push(m: &mut HashMap<String, Vec<usize>>, k: &str, i: usize) {
    match m.get_mut(k) {
        Some(v) => v.push(i),
        None => {
            m.insert(k.to_string(), vec![i]);
        }
    }
}

impl Workers {
    /// Those whose description could name `id`.
    fn naming(&self, id: &str) -> Vec<usize> {
        let h = head(id);
        if h.is_empty() {
            return (0..self.all.len()).collect();
        }
        self.pieces.get(h).cloned().unwrap_or_default()
    }

    /// The task's workers, latest first: named for it or its ticket, or run in its worktrees.
    pub fn of(&self, task_id: &str, ticket: Option<&str>, worktrees: &[&str]) -> Vec<Worker> {
        let mut hits = BTreeSet::new();
        for id in [Some(task_id), ticket].into_iter().flatten() {
            let named = |i: &usize| contains_id(&self.all[*i].description, id);
            hits.extend(self.naming(id).into_iter().filter(named));
        }
        for (_, v) in self.cwds.iter().filter(|(c, _)| inside(c, worktrees)) {
            hits.extend(v);
        }
        let mut v: Vec<&WorkerMeta> = hits.into_iter().map(|i| &self.all[i]).collect();
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker(description: &str, cwds: &[&str]) -> WorkerMeta {
        WorkerMeta {
            description: description.into(),
            transcript: PathBuf::from("/t.jsonl"),
            finished: false,
            modified: None,
            cwds: cwds.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn matching() {
        let hit = |desc: &str, id: &str, ticket: Option<&str>| {
            let w = Workers::from(vec![worker("another", &[]), worker(desc, &[])]);
            let v = w.of(id, ticket, &[]);
            v.iter().any(|w| w.description == desc)
        };
        // `<id>:` is the convention.
        assert!(hit("ctx-01z: review #4602", "ctx-01z", None));
        assert!(!hit("ctx-01zz: x", "ctx-01z", None));
        assert!(hit("ABC-123 dedupe", "ctx-1kj", Some("ABC-123")));
        assert!(!hit("ABC-1234 x", "ctx-1kj", Some("ABC-123")));
        // Anywhere, as a whole word.
        assert!(hit("[XY] ctx-1v.2.8 resume and ship", "ctx-1v.2.8", None));
        assert!(hit("ship (ctx-9)", "ctx-9", None));
        assert!(hit("finish ctx-9.", "ctx-9", None));
        assert!(hit("finish ctx-9. Then rest", "ctx-9", None));
        assert!(!hit("[XY] ctx-1v.2.8 resume", "ctx-1v.2", None));
        assert!(!hit("[XY] ctx-1v.2.8 resume", "ctx-1v", None));
        assert!(!hit("xctx-9 build", "ctx-9", None));
        assert!(!hit("a.ctx-9 build", "ctx-9", None));
        assert!(!hit("ctx-9-slug build", "ctx-9", None));
        assert!(!hit("anything", "", None));
        // Ids that do not start with, or hold more than, id characters.
        assert!(hit("fix #42 now", "#42", None));
        assert!(hit("fix team/7: x", "team/7", None));
        assert!(hit("ctx-9 and ctx-9 and ABC-1", "ctx-9", Some("ABC-1")));
    }

    #[test]
    fn by_cwd() {
        let w = |cwds: &[&str]| worker("no id here", cwds);
        let all = Workers::from(vec![
            w(&["/home"]),
            w(&["/home", "/wt/ctx-9-x/src"]),
            w(&["/wt/ctx-9-xy"]),
        ]);
        assert_eq!(all.of("ctx-9", None, &["/wt/ctx-9-x/"]).len(), 1);
        assert!(all.of("ctx-9", None, &[]).is_empty());
        let both = Workers::from(vec![worker("ctx-9: x", &["/wt/a", "/wt/a/b"])]);
        assert_eq!(both.of("ctx-9", None, &["/wt/a"]).len(), 1, "listed once");
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
        assert_eq!(Workers::from(w).of("ctx-9", None, &[]).len(), 1);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_rescan_notices_every_change() {
        let root = std::env::temp_dir().join(format!("fermata-rescan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("p/s/subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let meta = |n: &str, t: &str| {
            std::fs::write(sub.join(format!("{n}.meta.json")), t).unwrap();
        };
        let names = || {
            let mut v: Vec<_> = scan_workers(&root)
                .into_iter()
                .map(|w| (w.description, w.finished, w.cwds))
                .collect();
            v.sort();
            v
        };
        let row = |d: &str, f: bool, c: &[&str]| {
            let c: Vec<String> = c.iter().map(|s| s.to_string()).collect();
            (d.to_string(), f, c)
        };
        meta("a", r#"{"description":"one"}"#);
        assert_eq!(names(), [row("one", false, &[])]);
        // Same size, at once: the stamp is too young to trust.
        meta("a", r#"{"description":"two"}"#);
        assert_eq!(names(), [row("two", false, &[])]);
        std::fs::write(sub.join("a.jsonl"), "{\"cwd\":\"/x\"}\n").unwrap();
        assert_eq!(names(), [row("two", false, &["/x"])]);
        std::thread::sleep(crate::util::SETTLE + std::time::Duration::from_millis(50));
        assert_eq!(names(), [row("two", false, &["/x"])]);
        // Settled, then rewritten at the same size and put back to the same mtime.
        let p = sub.join("a.meta.json");
        let at = std::fs::metadata(&p).unwrap().modified().unwrap();
        meta("a", r#"{"description":"six"}"#);
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(at))
            .unwrap();
        assert_eq!(names(), [row("six", false, &["/x"])]);
        meta("b", r#"{"description":"new","finished":true}"#);
        meta("c", "not json");
        std::fs::write(sub.join("a.jsonl"), "{\"cwd\":\"/x\"}\n{\"cwd\":\"/y\"}\n").unwrap();
        assert_eq!(
            names(),
            [row("new", true, &[]), row("six", false, &["/x", "/y"])]
        );
        std::fs::remove_file(&p).unwrap();
        assert_eq!(names(), [row("new", true, &[])]);
        assert_eq!(
            SCANS.lock().unwrap()[&root].len(),
            2,
            "gone ones are forgotten"
        );
        std::fs::remove_dir_all(&root).unwrap();
        assert!(scan_workers(&root).is_empty());
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
