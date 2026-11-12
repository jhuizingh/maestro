//! Routing typed input: live session pane, else the orchestrator pane, and always a note.
use crate::baton::Tracker;
use crate::log;
use crate::model::Session;
use crate::notes::answer_note;
use crate::sessions::same_path;
use crate::util::run_cmd;
use anyhow::{Result, bail};
use std::sync::Mutex;
use std::time::Duration;

pub trait Tmux: Send + Sync {
    fn pane_exists(&self, target: &str) -> bool;
    /// Type `text` literally into the pane, then press Enter.
    fn send_text(&self, target: &str, text: &str) -> Result<()>;
    fn switch_client(&self, target: &str) -> Result<()>;
    fn new_window(&self, dir: &str) -> Result<()>;
}

pub struct RealTmux;

fn tmux(args: &[&str]) -> Result<String> {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = run_cmd("tmux", &a, None, &[], None, Duration::from_secs(3))?;
    if !out.ok() {
        bail!("tmux {}: {}", args[0], out.stderr.trim());
    }
    Ok(out.stdout)
}

impl Tmux for RealTmux {
    fn pane_exists(&self, target: &str) -> bool {
        tmux(&["display-message", "-p", "-t", target, "#{pane_id}"]).is_ok()
    }
    fn send_text(&self, target: &str, text: &str) -> Result<()> {
        tmux(&["send-keys", "-t", target, "-l", text])?;
        tmux(&["send-keys", "-t", target, "Enter"])?;
        Ok(())
    }
    fn switch_client(&self, target: &str) -> Result<()> {
        tmux(&["switch-client", "-t", target]).map(|_| ())
    }
    fn new_window(&self, dir: &str) -> Result<()> {
        tmux(&["new-window", "-c", dir]).map(|_| ())
    }
}

/// Records what would have been sent; touches nothing.
#[derive(Default)]
pub struct DryRunTmux {
    pub sent: Mutex<Vec<String>>,
}

impl Tmux for DryRunTmux {
    fn pane_exists(&self, _target: &str) -> bool {
        true
    }
    fn send_text(&self, target: &str, text: &str) -> Result<()> {
        let line = format!("dry-run send-keys -t {target}: {text}");
        log::warn(&line);
        self.sent.lock().unwrap().push(line);
        Ok(())
    }
    fn switch_client(&self, target: &str) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push(format!("dry-run switch-client {target}"));
        Ok(())
    }
    fn new_window(&self, dir: &str) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push(format!("dry-run new-window {dir}"));
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Session { target: String, status: String },
    Orchestrator { target: String, status: String },
    NoteOnly { reason: String },
}

/// The task's live session (cwd is one of its worktrees), else the orchestrator (cwd is the
/// context's workspace dir), else note only.
pub fn plan(worktrees: &[&str], workspace: &str, sessions: &[Session]) -> Route {
    let live = |s: &&Session| !s.closed && s.tmux_target.is_some();
    if let Some(s) = sessions
        .iter()
        .filter(live)
        .find(|s| worktrees.iter().any(|w| same_path(&s.worktree, w)))
    {
        return Route::Session {
            target: s.tmux_target.clone().unwrap_or_default(),
            status: s.status.clone(),
        };
    }
    if let Some(s) = sessions
        .iter()
        .filter(live)
        .find(|s| same_path(&s.worktree, workspace))
    {
        return Route::Orchestrator {
            target: s.tmux_target.clone().unwrap_or_default(),
            status: s.status.clone(),
        };
    }
    Route::NoteOnly {
        reason: "no orchestrator session found; note kept for the next sweep".into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The target pane is busy; ask the user before typing into it.
    NeedsConfirm {
        target: String,
        status: String,
    },
    Delivered {
        summary: String,
    },
}

pub struct Router {
    pub tmux: Box<dyn Tmux>,
    pub dry_run: bool,
}

impl Router {
    pub fn new(dry_run: bool) -> Self {
        let tmux: Box<dyn Tmux> = if dry_run {
            Box::new(DryRunTmux::default())
        } else {
            Box::new(RealTmux)
        };
        Self { tmux, dry_run }
    }

    pub fn deliver(
        &self,
        tracker: &dyn Tracker,
        task_id: &str,
        text: &str,
        route: &Route,
        confirmed: bool,
    ) -> Result<Outcome> {
        let text = crate::util::one_line(text);
        if text.is_empty() {
            bail!("nothing to send");
        }
        let (target, status, payload) = match route {
            Route::Session { target, status } => (Some(target), status, text.clone()),
            Route::Orchestrator { target, status } => {
                (Some(target), status, format!("[fermata {task_id}] {text}"))
            }
            Route::NoteOnly { .. } => (None, &String::new(), String::new()),
        };
        if let Some(t) = target
            && status == "working"
            && !confirmed
        {
            return Ok(Outcome::NeedsConfirm {
                target: t.clone(),
                status: status.clone(),
            });
        }
        let mut parts = vec![];
        match (target, route) {
            (Some(t), _) if !self.tmux.pane_exists(t) => {
                parts.push(format!("pane {t} not found; note only"));
            }
            (Some(t), _) => match self.tmux.send_text(t, &payload) {
                Ok(()) if self.dry_run => parts.push(format!("dry-run: would send to {t}")),
                Ok(()) => parts.push(format!("sent to {t}")),
                Err(e) => {
                    log::error(format!("send to {t}: {e}"));
                    parts.push(format!("send to {t} failed: {e}"))
                }
            },
            (None, Route::NoteOnly { reason }) => parts.push(reason.clone()),
            _ => {}
        }
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%MZ").to_string();
        let note = answer_note(&ts, &text);
        if self.dry_run {
            log::warn(format!("dry-run note {task_id}: {note}"));
            parts.push("note (dry-run)".into());
        } else {
            match tracker.note(task_id, &note) {
                Ok(()) => parts.push("noted".into()),
                Err(e) => {
                    log::error(format!("note {task_id}: {e}"));
                    parts.push(format!("note failed: {e}"))
                }
            }
        }
        Ok(Outcome::Delivered {
            summary: parts.join("; "),
        })
    }

    /// Quick reply straight to a session pane (Sessions view), no task attached.
    pub fn send_to_pane(
        &self,
        target: &str,
        status: &str,
        text: &str,
        confirmed: bool,
    ) -> Result<Outcome> {
        if status == "working" && !confirmed {
            return Ok(Outcome::NeedsConfirm {
                target: target.into(),
                status: status.into(),
            });
        }
        if !self.tmux.pane_exists(target) {
            bail!("pane {target} not found");
        }
        self.tmux.send_text(target, &crate::util::one_line(text))?;
        Ok(Outcome::Delivered {
            summary: format!(
                "{}sent to {target}",
                if self.dry_run { "dry-run: " } else { "" }
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::FakeTracker;
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeTmux {
        sent: Sent,
        missing: bool,
    }

    impl Tmux for FakeTmux {
        fn pane_exists(&self, _t: &str) -> bool {
            !self.missing
        }
        fn send_text(&self, t: &str, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push((t.into(), text.into()));
            Ok(())
        }
        fn switch_client(&self, _t: &str) -> Result<()> {
            Ok(())
        }
        fn new_window(&self, _d: &str) -> Result<()> {
            Ok(())
        }
    }

    fn sess(wt: &str, target: &str, status: &str) -> Session {
        Session {
            worktree: wt.into(),
            tmux_target: Some(target.into()),
            status: status.into(),
            ..Default::default()
        }
    }

    type Sent = Arc<Mutex<Vec<(String, String)>>>;

    fn router() -> (Router, Sent) {
        let t = FakeTmux::default();
        let sent = t.sent.clone();
        (
            Router {
                tmux: Box::new(t),
                dry_run: false,
            },
            sent,
        )
    }

    #[test]
    fn plans() {
        let s = vec![
            sess("/ws", "orch:1.1", "idle"),
            sess("/wt/a", "a:1.1", "idle"),
        ];
        assert_eq!(
            plan(&["/wt/a"], "/ws", &s),
            Route::Session {
                target: "a:1.1".into(),
                status: "idle".into()
            }
        );
        assert!(matches!(
            plan(&["/wt/b"], "/ws", &s),
            Route::Orchestrator { .. }
        ));
        assert!(matches!(plan(&["/wt/b"], "/x", &s), Route::NoteOnly { .. }));
    }

    #[test]
    fn session_route_sends_and_notes() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let route = plan(&["/wt/a"], "/ws", &[sess("/wt/a", "a:1.1", "idle")]);
        let o = r
            .deliver(&tr, "ctx-1", "yes\nplease", &route, false)
            .unwrap();
        assert!(matches!(o, Outcome::Delivered { .. }));
        assert_eq!(
            sent.lock().unwrap()[0],
            ("a:1.1".into(), "yes please".into())
        );
        let notes = tr.notes.lock().unwrap();
        assert_eq!(notes[0].0, "ctx-1");
        assert!(notes[0].1.starts_with("ANSWER (fermata, "));
        assert!(notes[0].1.ends_with("): yes please"));
    }

    #[test]
    fn orchestrator_prefix() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", "idle")]);
        r.deliver(&tr, "ctx-2", "go", &route, false).unwrap();
        assert_eq!(sent.lock().unwrap()[0].1, "[fermata ctx-2] go");
    }

    #[test]
    fn busy_needs_confirm() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", "working")]);
        let o = r.deliver(&tr, "ctx-3", "go", &route, false).unwrap();
        assert!(matches!(o, Outcome::NeedsConfirm { .. }));
        assert!(sent.lock().unwrap().is_empty());
        assert!(tr.notes.lock().unwrap().is_empty());
        r.deliver(&tr, "ctx-3", "go", &route, true).unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn note_only_and_missing_pane() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let o = r
            .deliver(&tr, "ctx-4", "x", &plan(&[], "/ws", &[]), false)
            .unwrap();
        assert!(
            matches!(o, Outcome::Delivered { ref summary } if summary.contains("no orchestrator"))
        );
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(tr.notes.lock().unwrap().len(), 1);
        let r2 = Router {
            tmux: Box::new(FakeTmux {
                missing: true,
                ..Default::default()
            }),
            dry_run: false,
        };
        let route = plan(&[], "/ws", &[sess("/ws", "o:1", "idle")]);
        let o = r2.deliver(&tr, "ctx-4", "x", &route, false).unwrap();
        assert!(matches!(o, Outcome::Delivered { ref summary } if summary.contains("not found")));
    }

    #[test]
    fn dry_run_touches_nothing() {
        let r = Router::new(true);
        let tr = FakeTracker::default();
        let route = plan(&["/wt"], "/ws", &[sess("/wt", "real:1.1", "idle")]);
        r.deliver(&tr, "ctx-5", "x", &route, false).unwrap();
        assert!(tr.notes.lock().unwrap().is_empty());
    }

    /// Real tmux against a scratch session this test creates and kills. Run with `--ignored`.
    #[test]
    #[ignore]
    fn scratch_tmux_roundtrip() {
        let name = format!("fermata-scratch-{}", std::process::id());
        tmux(&[
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "120",
            "-y",
            "20",
            "cat",
        ])
        .unwrap();
        let target = format!("={name}:");
        let r = Router {
            tmux: Box::new(RealTmux),
            dry_run: false,
        };
        let tr = FakeTracker::default();
        let route = Route::Orchestrator {
            target: target.clone(),
            status: "idle".into(),
        };
        let o = r.deliver(&tr, "ctx-x", "hello $HOME; Enter", &route, false);
        std::thread::sleep(Duration::from_millis(300));
        let pane = tmux(&["capture-pane", "-p", "-t", &target]).unwrap_or_default();
        let _ = tmux(&["kill-session", "-t", &format!("={name}")]);
        assert!(
            matches!(o.unwrap(), Outcome::Delivered { ref summary } if summary.starts_with("sent"))
        );
        assert!(
            pane.contains("[fermata ctx-x] hello $HOME; Enter"),
            "pane: {pane}"
        );
        assert_eq!(tr.notes.lock().unwrap().len(), 1);
    }
}
