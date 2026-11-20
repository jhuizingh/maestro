//! File watching (kqueue/FSEvents via `notify`): tracker dirs and the live session-record dirs.
use crate::log;
use notify::{RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::mpsc::Sender;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Change {
    Tasks,
    Sessions,
}

/// Keep the returned watchers alive for as long as events are wanted.
pub fn start(
    trackers: Vec<PathBuf>,
    session_dirs: Vec<PathBuf>,
    tx: Sender<Change>,
) -> Vec<notify::RecommendedWatcher> {
    let mut out = vec![];
    let mut add = |path: PathBuf, kind: Change, mode: RecursiveMode| {
        let tx = tx.clone();
        let w = notify::recommended_watcher(move |r: notify::Result<notify::Event>| {
            if let Ok(ev) = r
                && !matches!(ev.kind, notify::EventKind::Access(_))
            {
                let _ = tx.send(kind);
            }
        });
        match w {
            Ok(mut w) => match w.watch(&path, mode) {
                Ok(()) => out.push(w),
                Err(e) => log::warn(format!("watch {}: {e}", path.display())),
            },
            Err(e) => log::warn(format!("watcher: {e}")),
        }
    };
    for t in trackers.into_iter().filter(|p| p.exists()) {
        add(t, Change::Tasks, RecursiveMode::Recursive);
    }
    for d in session_dirs {
        if d.exists() {
            add(d, Change::Sessions, RecursiveMode::NonRecursive);
        }
    }
    out
}
