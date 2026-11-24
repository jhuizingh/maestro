//! File watching (kqueue/FSEvents via `notify`): tracker dirs and the live session-record dirs.
use crate::log;
use crate::signature::{Touched, noise};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Something in the tracker directory `dir` (as given to `start`).
    Tasks {
        dir: PathBuf,
        touched: Touched,
    },
    Sessions,
}

/// What an event in a tracker directory means; `root` is its canonical path. Events that only
/// name files the write signature skips mean nothing.
fn tasks(ev: Event, dir: &Path, root: &Path) -> Option<Change> {
    if matches!(ev.kind, EventKind::Access(_)) {
        return None;
    }
    let touched = if ev.need_rescan() || ev.paths.is_empty() {
        Touched::Unknown
    } else {
        let skip = |p: &PathBuf| p.strip_prefix(root).is_ok_and(noise);
        let v: Vec<PathBuf> = ev.paths.into_iter().filter(|p| !skip(p)).collect();
        if v.is_empty() {
            return None;
        }
        Touched::Paths(v)
    };
    Some(Change::Tasks {
        dir: dir.to_path_buf(),
        touched,
    })
}

/// As `tasks`, for a records directory: only `.json` files (and the directory itself) are read.
fn sessions(ev: Event, root: &Path) -> Option<Change> {
    let counts = |p: &PathBuf| p == root || p.extension().is_some_and(|x| x == "json");
    let named = ev.need_rescan() || ev.paths.is_empty() || ev.paths.iter().any(counts);
    (named && !matches!(ev.kind, EventKind::Access(_))).then_some(Change::Sessions)
}

/// Keep it for as long as events are wanted.
#[derive(Default)]
pub struct Watching {
    _watchers: Vec<notify::RecommendedWatcher>,
    /// The tracker directories really watched.
    pub trackers: HashSet<PathBuf>,
}

pub fn start(trackers: Vec<PathBuf>, session_dirs: Vec<PathBuf>, tx: Sender<Change>) -> Watching {
    let mut out = Watching::default();
    let mut add = |path: PathBuf, tracker: bool, mode: RecursiveMode| {
        let tx = tx.clone();
        // Events name canonical paths; watching one makes that so on every platform.
        let root = path.canonicalize().unwrap_or_else(|_| path.clone());
        let (dir, at) = (path.clone(), root.clone());
        let w = notify::recommended_watcher(move |r: notify::Result<Event>| {
            let c = match r {
                Ok(ev) if tracker => tasks(ev, &dir, &at),
                Ok(ev) => sessions(ev, &at),
                Err(_) => None,
            };
            if let Some(c) = c {
                let _ = tx.send(c);
            }
        });
        match w {
            Ok(mut w) => match w.watch(&root, mode) {
                Ok(()) => {
                    out._watchers.push(w);
                    if tracker {
                        out.trackers.insert(path);
                    }
                }
                Err(e) => log::warn(format!("watch {}: {e}", path.display())),
            },
            Err(e) => log::warn(format!("watcher: {e}")),
        }
    };
    for t in trackers.into_iter().filter(|p| p.exists()) {
        add(t, true, RecursiveMode::Recursive);
    }
    for d in session_dirs {
        if d.exists() {
            add(d, false, RecursiveMode::NonRecursive);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{AccessKind, Flag, ModifyKind};

    fn ev(paths: &[&str]) -> Event {
        let mut e = Event::new(EventKind::Modify(ModifyKind::Any));
        e.paths = paths.iter().map(PathBuf::from).collect();
        e
    }

    fn touched(e: Event) -> Option<Touched> {
        match tasks(e, Path::new("/link"), Path::new("/t")) {
            Some(Change::Tasks { dir, touched }) => {
                assert_eq!(dir, Path::new("/link"), "named as the context names it");
                Some(touched)
            }
            _ => None,
        }
    }

    #[test]
    fn tracker_events() {
        let paths = |v: &[&str]| Some(Touched::Paths(v.iter().map(PathBuf::from).collect()));
        assert_eq!(
            touched(ev(&["/t/noms/manifest"])),
            paths(&["/t/noms/manifest"])
        );
        assert_eq!(touched(ev(&["/t"])), paths(&["/t"]));
        for p in ["/t/last-touched", "/t/a/LOCK", "/t/x.lock", "/t/a/temptf/m"] {
            assert_eq!(touched(ev(&[p])), None, "{p}");
        }
        assert_eq!(touched(ev(&["/t/a/LOCK", "/t/b"])), paths(&["/t/b"]));
        assert_eq!(touched(ev(&[])), Some(Touched::Unknown));
        let dropped = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert_eq!(touched(dropped), Some(Touched::Unknown));
        let mut read = ev(&["/t/b"]);
        read.kind = EventKind::Access(AccessKind::Any);
        assert_eq!(touched(read), None);
    }

    #[test]
    fn session_events() {
        let root = Path::new("/s/sessions");
        let hit = |e| sessions(e, root).is_some();
        assert!(hit(ev(&["/s/sessions/a.json"])));
        assert!(hit(ev(&["/s/sessions/.a.tmp", "/s/sessions/a.json"])));
        assert!(hit(ev(&["/s/sessions"])));
        assert!(hit(ev(&[])));
        assert!(hit(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
        assert!(!hit(ev(&["/s/sessions/.a.json.tmp"])));
        let mut read = ev(&["/s/sessions/a.json"]);
        read.kind = EventKind::Access(AccessKind::Any);
        assert!(!hit(read));
    }
}
