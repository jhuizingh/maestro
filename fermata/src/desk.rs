//! What a client does on the viewer's own machine: their tmux client and their browser.
//! Not part of the API: a server cannot move another machine's terminal.
use crate::actions::tmux;
use crate::api::Pane;
use anyhow::{Result, bail};
use std::path::PathBuf;

pub trait Terminal: Send + Sync {
    /// Switch the client to a pane id (`%NN`) or a session (`=name`).
    fn switch_client(&self, target: &str) -> Result<()>;
    /// A tmux session with exactly this name exists.
    fn has_session(&self, name: &str) -> bool;
    fn new_window(&self, dir: &str) -> Result<()>;
    fn open_url(&self, url: &str) -> Result<()>;
}

/// `dry_run` looks but moves nothing.
struct Real {
    dry_run: bool,
}

impl Terminal for Real {
    fn switch_client(&self, target: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        tmux(&["switch-client", "-t", target]).map(|_| ())
    }
    fn has_session(&self, name: &str) -> bool {
        tmux(&["has-session", "-t", &format!("={name}")]).is_ok()
    }
    fn new_window(&self, dir: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        tmux(&["new-window", "-c", dir]).map(|_| ())
    }
    fn open_url(&self, url: &str) -> Result<()> {
        Ok(open::that_detached(url)?)
    }
}

pub struct Desk {
    pub term: Box<dyn Terminal>,
    /// Running inside tmux, so `switch-client` has a client to move.
    pub in_tmux: bool,
    pub dry_run: bool,
    baton: Option<PathBuf>,
}

impl Desk {
    pub fn new(dry_run: bool) -> Self {
        Self {
            term: Box::new(Real { dry_run }),
            in_tmux: std::env::var_os("TMUX").is_some_and(|v| !v.is_empty()),
            dry_run,
            baton: crate::baton::locate_baton(),
        }
    }

    pub fn jump(&self, p: &Pane) -> Result<String> {
        self.switch(&p.id, &p.label)
    }

    fn switch(&self, target: &str, label: &str) -> Result<String> {
        if !self.in_tmux {
            bail!("not inside tmux; run: tmux attach -t '{target}'");
        }
        self.term.switch_client(target)?;
        Ok(if self.dry_run {
            format!("dry-run: would jump to {label}")
        } else {
            format!("jumped to {label}")
        })
    }

    /// tmux session names baton would give the worktree's task.
    pub fn names(&self, worktree: Option<&str>) -> Vec<String> {
        match (&self.baton, worktree) {
            (Some(b), Some(w)) => crate::baton::session_names(b, w),
            _ => vec![],
        }
    }

    /// `w` on a task: its live Claude pane, else a tmux session named for the task, else a new
    /// window in the worktree. Never the orchestrator: that is not the task's session.
    /// `live` is the pane, or why there is none.
    pub fn open_task(
        &self,
        live: Result<Pane, String>,
        names: &[String],
        worktree: Option<&str>,
    ) -> String {
        let why = match live {
            Ok(p) => return self.jump(&p).unwrap_or_else(|e| format!("jump: {e}")),
            Err(why) => why,
        };
        if let Some(n) = names.iter().find(|n| self.term.has_session(n)) {
            return self
                .switch(&format!("={n}"), &format!("session {n}"))
                .unwrap_or_else(|e| format!("jump: {e}"));
        }
        let Some(w) = worktree else {
            return format!("{why}; no worktree");
        };
        if !self.in_tmux {
            return format!("{why}; not inside tmux, worktree is {w}");
        }
        match self.term.new_window(w) {
            Ok(()) => format!("{why}; opened new window in {w}"),
            Err(e) => format!("{why}; tmux: {e}"),
        }
    }

    pub fn new_window(&self, dir: &str) -> String {
        match self.term.new_window(dir) {
            Ok(()) => format!("opened tmux window in {dir}"),
            Err(e) => format!("tmux: {e}"),
        }
    }

    pub fn open_url(&self, url: &str) -> String {
        match self.term.open_url(url) {
            Ok(()) => format!("opened {url}"),
            Err(e) => format!("open failed: {e}"),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    pub type Calls = Arc<Mutex<Vec<String>>>;

    /// Records what would have moved, in order.
    struct Fake {
        calls: Calls,
        sessions: Vec<String>,
    }

    impl Fake {
        fn call(&self, s: String) -> Result<()> {
            self.calls.lock().unwrap().push(s);
            Ok(())
        }
    }

    impl Terminal for Fake {
        fn switch_client(&self, t: &str) -> Result<()> {
            self.call(format!("switch-client {t}"))
        }
        fn has_session(&self, name: &str) -> bool {
            self.sessions.iter().any(|s| s == name)
        }
        fn new_window(&self, d: &str) -> Result<()> {
            self.call(format!("new-window {d}"))
        }
        fn open_url(&self, u: &str) -> Result<()> {
            self.call(format!("open {u}"))
        }
    }

    pub fn fake(sessions: &[&str], in_tmux: bool) -> (Desk, Calls) {
        let calls = Calls::default();
        let d = Desk {
            term: Box::new(Fake {
                calls: calls.clone(),
                sessions: names(sessions),
            }),
            in_tmux,
            dry_run: false,
            baton: None,
        };
        (d, calls)
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn pane() -> Result<Pane, String> {
        Ok(Pane {
            id: "%1".into(),
            label: "a:1.1".into(),
        })
    }

    fn none() -> Result<Pane, String> {
        Err("no live session".into())
    }

    #[test]
    fn w_jumps_to_the_live_session_pane() {
        let (d, calls) = fake(&["a-ctx-1"], true);
        let m = d.open_task(pane(), &names(&["a-ctx-1"]), Some("/wt/a"));
        assert_eq!(m, "jumped to a:1.1");
        assert_eq!(*calls.lock().unwrap(), vec!["switch-client %1"]);
    }

    #[test]
    fn w_without_a_session_opens_a_window() {
        let (d, calls) = fake(&[], true);
        let m = d.open_task(none(), &[], Some("/wt/b"));
        assert_eq!(m, "no live session; opened new window in /wt/b");
        assert_eq!(*calls.lock().unwrap(), vec!["new-window /wt/b"]);
        assert_eq!(
            d.open_task(none(), &[], None),
            "no live session; no worktree"
        );
    }

    #[test]
    fn w_falls_back_to_the_task_named_tmux_session() {
        let (d, calls) = fake(&["legacy-b"], true);
        let m = d.open_task(none(), &names(&["b-ctx-2", "legacy-b"]), Some("/wt/b"));
        assert_eq!(m, "jumped to session legacy-b");
        assert_eq!(*calls.lock().unwrap(), vec!["switch-client =legacy-b"]);
    }

    #[test]
    fn w_outside_tmux_gives_an_attach_hint() {
        let (d, calls) = fake(&[], false);
        let m = d.open_task(pane(), &[], Some("/wt/a"));
        assert!(m.contains("tmux attach -t '%1'"), "{m}");
        let m = d.open_task(none(), &[], Some("/wt/a"));
        assert_eq!(m, "no live session; not inside tmux, worktree is /wt/a");
        assert!(calls.lock().unwrap().is_empty());
    }

    /// `w` against real tmux: needs a client to move, so run it inside a throwaway server
    /// (`tmux -L x`), with `FERMATA_W_TEST=1` and `--ignored`. It skips otherwise.
    #[test]
    #[ignore]
    fn open_task_moves_the_real_client() {
        if std::env::var("FERMATA_W_TEST").as_deref() != Ok("1")
            || std::env::var_os("TMUX").is_none()
        {
            eprintln!("skipped: set FERMATA_W_TEST=1 and run inside a throwaway tmux server");
            return;
        }
        let tag = std::process::id();
        let (live, named) = (format!("fw-live-{tag}"), format!("fw-named-{tag}"));
        let dir = std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        for n in [&live, &named] {
            tmux(&["new-session", "-d", "-s", n, "-c", &dir, "sleep 60"]).unwrap();
        }
        let id = tmux(&["display", "-p", "-t", &format!("={live}:"), "#{pane_id}"]).unwrap();
        let client = || tmux(&["list-clients", "-F", "#{client_session}"]).unwrap();
        let d = Desk::new(false);
        let p = Pane {
            id: id.trim().into(),
            label: live.clone(),
        };
        let m1 = d.open_task(Ok(p), &[], Some(&dir));
        let c1 = client();
        let m2 = d.open_task(none(), &names(&["fw-absent", &named]), Some(&dir));
        let c2 = client();
        for n in [&live, &named] {
            let _ = tmux(&["kill-session", "-t", &format!("={n}")]);
        }
        assert!(m1.starts_with("jumped to"), "{m1}");
        assert!(c1.contains(&live), "{c1}");
        assert_eq!(m2, format!("jumped to session {named}"));
        assert!(c2.contains(&named), "{c2}");
    }
}
