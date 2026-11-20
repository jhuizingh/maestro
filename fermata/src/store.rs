//! The session-record store: the one place that knows the record directory layout.
//!
//! Layout under the root (`$FERMATA_DATA_DIR`, default `<data dir>/fermata`): `sessions/<id>.json`
//! for live sessions, `closed/<id>.json` for named sessions that ended, `logs/<id>.log` for each
//! session's activity log. ccmon's directory (`$FERMATA_CCMON_DIR`) has the same layout and is
//! read as a fallback during the switch; any write moves the record into the root.
use crate::model::Session;
use crate::util::expand_tilde;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

pub type Record = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Live,
    Closed,
}

impl Kind {
    fn sub(self) -> &'static str {
        match self {
            Kind::Live => "sessions",
            Kind::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
    /// ccmon's directory, read but never written to; `None` when it is the root.
    pub legacy: Option<PathBuf>,
}

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("FERMATA_DATA_DIR") {
        return PathBuf::from(d);
    }
    dirs::data_dir()
        .map(|d| d.join("fermata"))
        .unwrap_or_else(|| expand_tilde("~/.local/share/fermata"))
}

pub fn ccmon_dir() -> PathBuf {
    std::env::var("FERMATA_CCMON_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_tilde("~/code/claude-monitor"))
}

/// Session ids name files, so only accept what Claude Code mints (uuid-like).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Parse the first JSON object in `txt`, ignoring trailing bytes a torn write left behind.
pub fn parse_record(txt: &str) -> Option<Record> {
    match serde_json::Deserializer::from_str(txt)
        .into_iter::<Value>()
        .next()?
    {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

fn write_atomic(path: &Path, rec: &Record) -> Result<()> {
    let dir = path.parent().context("record path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension(format!("json.tmp{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(rec)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl Store {
    pub fn new(root: PathBuf, legacy: PathBuf) -> Self {
        let same = std::fs::canonicalize(&root)
            .ok()
            .zip(std::fs::canonicalize(&legacy).ok())
            .is_some_and(|(a, b)| a == b)
            || root == legacy;
        Store {
            root,
            legacy: (!same).then_some(legacy),
        }
    }

    pub fn open() -> Self {
        Self::new(data_dir(), ccmon_dir())
    }

    fn dirs(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.root).chain(self.legacy.iter())
    }

    fn path_in(dir: &Path, kind: Kind, id: &str) -> PathBuf {
        dir.join(kind.sub()).join(format!("{id}.json"))
    }

    /// Every directory holding live records, for the file watcher.
    pub fn live_dirs(&self) -> Vec<PathBuf> {
        self.dirs().map(|d| d.join("sessions")).collect()
    }

    /// Serialises read-modify-write across hook processes and the TUI. The lock is a separate
    /// file because records are replaced by rename, which would orphan a lock held on them.
    pub fn locked<T>(&self, f: impl FnOnce() -> T) -> Result<T> {
        std::fs::create_dir_all(&self.root)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(".lock"))?;
        lock.lock()?;
        Ok(f())
    }

    /// The record for `id`, from the root first, then the legacy directory.
    pub fn read(&self, kind: Kind, id: &str) -> Option<Record> {
        if !valid_id(id) {
            return None;
        }
        self.dirs().find_map(|d| {
            let txt = std::fs::read_to_string(Self::path_in(d, kind, id)).ok()?;
            parse_record(&txt)
        })
    }

    /// Write `rec` into the root and drop any legacy copy, so a record moves on its next write.
    pub fn write(&self, kind: Kind, id: &str, rec: &Record) -> Result<()> {
        anyhow::ensure!(valid_id(id), "bad session id {id:?}");
        write_atomic(&Self::path_in(&self.root, kind, id), rec)?;
        if let Some(l) = &self.legacy {
            let _ = std::fs::remove_file(Self::path_in(l, kind, id));
        }
        Ok(())
    }

    /// Remove the record from every directory. True if one existed.
    pub fn remove(&self, kind: Kind, id: &str) -> bool {
        valid_id(id)
            && self
                .dirs()
                .filter(|d| std::fs::remove_file(Self::path_in(d, kind, id)).is_ok())
                .count()
                > 0
    }

    /// Read, change and write back an existing record under the lock. False if there is none.
    pub fn update(&self, kind: Kind, id: &str, f: impl FnOnce(&mut Record)) -> Result<bool> {
        self.locked(|| {
            let Some(mut rec) = self.read(kind, id) else {
                return Ok(false);
            };
            f(&mut rec);
            self.write(kind, id, &rec).map(|()| true)
        })?
    }

    /// Ids of every live record, root first, each id once.
    pub fn live_ids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = vec![];
        for d in self.dirs() {
            let Ok(rd) = std::fs::read_dir(d.join("sessions")) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().is_some_and(|x| x == "json")
                    && let Some(id) = p.file_stem().and_then(|s| s.to_str())
                    && valid_id(id)
                    && seen.insert(id.to_string())
                {
                    out.push(id.to_string());
                }
            }
        }
        out
    }

    /// Typed sessions of one kind across both directories, newest activity first. A record in
    /// the root shadows the legacy one with the same id.
    pub fn sessions(&self, kind: Kind) -> Vec<Session> {
        let mut seen = HashSet::new();
        let mut v = vec![];
        for d in self.dirs() {
            for mut s in load_dir(&d.join(kind.sub())) {
                if seen.insert(s.session_id.clone()) {
                    s.closed = kind == Kind::Closed;
                    v.push(s);
                }
            }
        }
        v.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));
        v
    }

    /// The activity log for `id`: the root's if it exists, else ccmon's, else the root's path.
    pub fn log_path(&self, id: &str) -> PathBuf {
        let name = format!("{id}.log");
        self.dirs()
            .map(|d| d.join("logs").join(&name))
            .find(|p| p.exists())
            .unwrap_or_else(|| self.root.join("logs").join(name))
    }

    /// Append `[<now>] <line>` to the root's log for `id`.
    pub fn log(&self, id: &str, line: &str) -> Result<()> {
        anyhow::ensure!(valid_id(id), "bad session id {id:?}");
        let dir = self.root.join("logs");
        std::fs::create_dir_all(&dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{id}.log")))?;
        writeln!(f, "[{}] {line}", now())?;
        Ok(())
    }
}

fn load_dir(dir: &Path) -> Vec<Session> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return vec![];
    };
    rd.flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let rec = parse_record(&std::fs::read_to_string(e.path()).ok()?)?;
            serde_json::from_value(Value::Object(rec)).ok()
        })
        .collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A scratch store (root plus legacy dir) removed on drop.
    pub struct Scratch(pub PathBuf, pub Store);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub fn scratch(name: &str) -> Scratch {
        let d = std::env::temp_dir().join(format!("fermata-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let s = Store::new(d.join("root"), d.join("ccmon"));
        Scratch(d, s)
    }

    pub fn put(dir: &Path, kind: Kind, id: &str, v: Value) {
        let p = Store::path_in(dir, kind, id);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, v.to_string()).unwrap();
    }

    #[test]
    fn legacy_read_then_moves_on_write() {
        let t = scratch("move");
        let legacy = t.1.legacy.clone().unwrap();
        put(
            &legacy,
            Kind::Live,
            "a-1",
            serde_json::json!({"session_id":"a-1","tool_calls":1}),
        );
        assert_eq!(t.1.sessions(Kind::Live).len(), 1);
        assert!(
            t.1.update(Kind::Live, "a-1", |r| {
                r.insert("tool_calls".into(), 2.into());
            })
            .unwrap()
        );
        assert!(!Store::path_in(&legacy, Kind::Live, "a-1").exists());
        assert_eq!(t.1.read(Kind::Live, "a-1").unwrap()["tool_calls"], 2);
        assert!(!t.1.update(Kind::Live, "nope", |_| {}).unwrap());
    }

    #[test]
    fn root_shadows_legacy() {
        let t = scratch("shadow");
        let legacy = t.1.legacy.clone().unwrap();
        put(
            &legacy,
            Kind::Live,
            "a-1",
            serde_json::json!({"session_id":"a-1","status":"old"}),
        );
        put(
            &t.1.root,
            Kind::Live,
            "a-1",
            serde_json::json!({"session_id":"a-1","status":"new"}),
        );
        put(
            &legacy,
            Kind::Closed,
            "b-2",
            serde_json::json!({"session_id":"b-2"}),
        );
        let live = t.1.sessions(Kind::Live);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].status, "new");
        assert_eq!(t.1.live_ids(), vec!["a-1".to_string()]);
        let closed = t.1.sessions(Kind::Closed);
        assert!(closed[0].closed);
        assert!(t.1.remove(Kind::Live, "a-1"));
        assert!(t.1.sessions(Kind::Live).is_empty());
    }

    #[test]
    fn torn_and_bad() {
        assert!(parse_record(r#"{"a":1}tail"#).is_some());
        assert!(parse_record("[1]").is_none());
        let t = scratch("bad");
        assert!(t.1.read(Kind::Live, "../x").is_none());
        assert!(t.1.write(Kind::Live, "../x", &Record::new()).is_err());
        assert!(!t.1.remove(Kind::Live, "../x"));
    }

    #[test]
    fn same_dir_has_no_legacy() {
        let d = std::env::temp_dir();
        assert!(Store::new(d.clone(), d).legacy.is_none());
    }
}
