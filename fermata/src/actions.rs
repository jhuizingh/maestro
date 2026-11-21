//! Session actions: describe, close, reopen, snapshot save/restore, and the dead-session sweep.
//! Live effects go through `System` so tests (and --dry-run) never touch real processes.
use crate::log;
use crate::model::Session;
use crate::panes::{self, PaneTable, Use};
use crate::store::{Kind, Store, valid_id};
use crate::util::run_cmd;
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Everything that reaches outside the session records.
pub trait System: Send + Sync {
    /// Process name of `pid`, or None if it is not running (or `ps` failed).
    fn process_name(&self, pid: i64) -> Option<String>;
    /// Whether a process with this pid exists, by `kill(pid, 0)`: no subprocess to fail.
    fn pid_exists(&self, pid: i64) -> bool;
    /// SIGTERM, wait up to 2s, then SIGKILL. True when the process is gone.
    fn kill_pid(&self, pid: i64) -> bool;
    /// Every tmux pane and the pid → ppid map, read fresh (destructive callers need that).
    fn pane_table(&self) -> Result<PaneTable>;
    /// `pane_id` is the stable `%NN` from `panes::resolve`, never a recorded address.
    fn kill_pane(&self, pane_id: &str) -> Result<()>;
    fn tmux_session_exists(&self, name: &str) -> bool;
    /// Run `cmd` in `dir`: a new window in `session` if it exists, else a new detached session.
    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()>;
    /// Run a user-configured command line with `sh -c` (`FERMATA_RELAUNCH`).
    fn sh(&self, cmd: &str) -> Result<()>;
}

pub struct RealSystem;

pub fn tmux(args: &[&str]) -> Result<String> {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = run_cmd("tmux", &a, None, &[], None, Duration::from_secs(5))?;
    if !out.ok() {
        bail!("tmux {}: {}", args[0], out.stderr.trim());
    }
    Ok(out.stdout)
}

impl System for RealSystem {
    fn process_name(&self, pid: i64) -> Option<String> {
        let out = run_cmd(
            "ps",
            &["-p".into(), pid.to_string(), "-o".into(), "comm=".into()],
            None,
            &[],
            None,
            Duration::from_secs(2),
        )
        .ok()?;
        let n = out.stdout.trim().to_string();
        (out.ok() && !n.is_empty()).then_some(n)
    }

    fn pid_exists(&self, pid: i64) -> bool {
        let Ok(p) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the pid exists; EPERM means it does.
        p > 0
            && (unsafe { libc::kill(p, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
    }

    fn kill_pid(&self, pid: i64) -> bool {
        let sig = |s: &str| {
            run_cmd(
                "kill",
                &[format!("-{s}"), pid.to_string()],
                None,
                &[],
                None,
                Duration::from_secs(2),
            )
            .map(|o| o.ok())
            .unwrap_or(false)
        };
        sig("TERM");
        for _ in 0..20 {
            if self.process_name(pid).is_none() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        sig("KILL");
        std::thread::sleep(Duration::from_millis(100));
        self.process_name(pid).is_none()
    }

    fn pane_table(&self) -> Result<PaneTable> {
        panes::real_table(std::time::Duration::ZERO)
    }

    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        tmux(&["kill-pane", "-t", pane_id]).map(|_| ())
    }

    fn tmux_session_exists(&self, name: &str) -> bool {
        tmux(&["has-session", "-t", &format!("={name}")]).is_ok()
    }

    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()> {
        if self.tmux_session_exists(session) {
            tmux(&["new-window", "-t", &format!("={session}:"), "-c", dir, cmd]).map(|_| ())
        } else {
            tmux(&["new-session", "-d", "-s", session, "-c", dir, cmd]).map(|_| ())
        }
    }

    fn sh(&self, cmd: &str) -> Result<()> {
        let a = ["-c".to_string(), cmd.to_string()];
        let out = run_cmd("sh", &a, None, &[], None, Duration::from_secs(60))?;
        if !out.ok() {
            bail!("{}", out.stderr.trim());
        }
        Ok(())
    }
}

/// Records what it would do; used for --dry-run and in tests.
#[derive(Default)]
pub struct FakeSystem {
    pub calls: Mutex<Vec<String>>,
    pub procs: Mutex<std::collections::HashMap<i64, String>>,
    pub sessions: Mutex<std::collections::HashSet<String>>,
    pub panes: Mutex<PaneTable>,
}

impl FakeSystem {
    fn rec(&self, s: String) {
        log::warn(format!("dry-run {s}"));
        self.calls.lock().unwrap().push(s);
    }
}

impl System for FakeSystem {
    fn process_name(&self, pid: i64) -> Option<String> {
        self.procs.lock().unwrap().get(&pid).cloned()
    }
    fn pid_exists(&self, pid: i64) -> bool {
        self.procs.lock().unwrap().contains_key(&pid)
    }
    fn kill_pid(&self, pid: i64) -> bool {
        self.rec(format!("kill {pid}"));
        self.procs.lock().unwrap().remove(&pid);
        true
    }
    fn pane_table(&self) -> Result<PaneTable> {
        Ok(self.panes.lock().unwrap().clone())
    }
    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.rec(format!("kill-pane {pane_id}"));
        Ok(())
    }
    fn tmux_session_exists(&self, name: &str) -> bool {
        self.sessions.lock().unwrap().contains(name)
    }
    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()> {
        self.rec(format!("launch {session} {dir} {cmd}"));
        Ok(())
    }
    fn sh(&self, cmd: &str) -> Result<()> {
        self.rec(format!("sh {cmd}"));
        Ok(())
    }
}

/// --dry-run: real read-only probes (ps, has-session), recorded no-ops for everything else.
#[derive(Default)]
pub struct DryRunSystem {
    pub rec: FakeSystem,
}

impl System for DryRunSystem {
    fn process_name(&self, pid: i64) -> Option<String> {
        RealSystem.process_name(pid)
    }
    fn pid_exists(&self, pid: i64) -> bool {
        RealSystem.pid_exists(pid)
    }
    fn kill_pid(&self, pid: i64) -> bool {
        self.rec.kill_pid(pid);
        false
    }
    fn pane_table(&self) -> Result<PaneTable> {
        RealSystem.pane_table()
    }
    fn kill_pane(&self, pane_id: &str) -> Result<()> {
        self.rec.kill_pane(pane_id)
    }
    fn tmux_session_exists(&self, name: &str) -> bool {
        RealSystem.tmux_session_exists(name)
    }
    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()> {
        self.rec.launch(session, dir, cmd)
    }
    fn sh(&self, cmd: &str) -> Result<()> {
        self.rec.sh(cmd)
    }
}

pub struct Actions {
    pub store: Store,
    /// `FERMATA_RELAUNCH`: how to start a session whose tmux session is gone (see `relaunch`).
    pub relaunch: Option<String>,
    pub sys: Arc<dyn System>,
    /// When true, JSON files are not written either.
    pub dry_run: bool,
}

/// How old an unparseable live record must be before the sweep removes it.
const SWEEP_UNREADABLE_AGE: Duration = Duration::from_secs(3600);

fn is_claude(name: &str) -> bool {
    name.contains("claude") || name.contains("node")
}

/// Single-quote `s` for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Fill `{session}`, `{worktree}` and `{session_id}` in a relaunch template, each shell-quoted.
pub fn fill_relaunch(tpl: &str, session: &str, worktree: &str, id: &str) -> String {
    tpl.replace("{session_id}", &sh_quote(id))
        .replace("{session}", &sh_quote(session))
        .replace("{worktree}", &sh_quote(worktree))
}

/// One live session in a snapshot. Field names match ccmon's snapshots, which still restore.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SnapEntry {
    pub tmux_session: String,
    pub worktree: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub last_activity: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct SnapFile {
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub sessions: Vec<SnapEntry>,
}

/// `claude --resume <id>` inside a login shell that stays open afterwards.
pub fn resume_cmd(session_id: &str) -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    format!("{shell} -lc 'claude --resume {session_id}; exec {shell} -l'")
}

impl Actions {
    pub fn new(dry_run: bool) -> Self {
        Self {
            store: Store::open(),
            relaunch: std::env::var("FERMATA_RELAUNCH")
                .ok()
                .filter(|v| !v.trim().is_empty()),
            sys: if dry_run {
                Arc::new(DryRunSystem::default())
            } else {
                Arc::new(RealSystem)
            },
            dry_run,
        }
    }

    fn kind(s: &Session) -> Kind {
        if s.closed { Kind::Closed } else { Kind::Live }
    }

    /// Set `description` in the session's (or closed record's) JSON, keeping other fields.
    pub fn describe(&self, s: &Session, desc: &str) -> Result<String> {
        if !valid_id(&s.session_id) {
            bail!("bad session id {:?}", s.session_id);
        }
        if self.dry_run {
            log::warn(format!("dry-run describe {}: {desc}", s.session_id));
            return Ok(format!(
                "dry-run: would set description of {}",
                s.session_id
            ));
        }
        let d = desc.trim().to_string();
        let found = self.store.update(Self::kind(s), &s.session_id, |r| {
            r.insert("description".into(), d.into());
        })?;
        if !found {
            bail!("no record for {}", s.session_id);
        }
        Ok(format!("description set for {}", short(&s.session_id)))
    }

    /// Save+close: write `closed/<id>.json`, kill the claude pid, optionally the pane,
    /// and drop the live session file.
    pub fn close(&self, s: &Session, kill_pane: bool) -> Result<String> {
        if s.closed {
            bail!("already closed");
        }
        if !valid_id(&s.session_id) {
            bail!("bad session id {:?}", s.session_id);
        }
        let serde_json::Value::Object(rec) = serde_json::json!({
            "session_id": s.session_id,
            "description": s.description.clone().unwrap_or_default(),
            "worktree": s.worktree,
            "branch": s.branch.clone().unwrap_or_default(),
            "tmux_session": s.tmux_session.clone().unwrap_or_default(),
            "closed_at": crate::store::now(),
        }) else {
            unreachable!()
        };
        let mut parts = vec![];
        if self.dry_run {
            parts.push(format!("dry-run: would save closed/{}.json", s.session_id));
        } else {
            self.store.write(Kind::Closed, &s.session_id, &rec)?;
            parts.push("record saved".into());
        }
        // Resolve the pane while claude still runs in it: afterwards there is no pid to match.
        let pane = kill_pane.then(|| {
            match s.claude_pid.and_then(|p| self.sys.process_name(p)) {
                Some(n) if is_claude(&n) => {}
                _ => bail!("claude is not running, so its pane cannot be identified"),
            }
            panes::resolve(&self.sys.pane_table()?, s, Use::Destroy)
        });
        match s.claude_pid {
            Some(pid) => match self.sys.process_name(pid) {
                Some(n) if is_claude(&n) => {
                    if self.dry_run {
                        self.sys.kill_pid(pid);
                        parts.push(format!("would kill {pid} ({n})"));
                    } else if self.sys.kill_pid(pid) {
                        parts.push(format!("killed {pid}"));
                    } else {
                        parts.push(format!("{pid} still running"));
                    }
                }
                Some(n) => parts.push(format!("pid {pid} is {n:?}, not claude; left alone")),
                None => parts.push(format!("pid {pid} not running")),
            },
            None => parts.push("no pid recorded".into()),
        }
        match pane {
            Some(Ok(p)) => match self.sys.kill_pane(&p.id) {
                Ok(()) if self.dry_run => parts.push(format!("would close pane {}", p.label())),
                Ok(()) => parts.push(format!("pane {} closed", p.label())),
                Err(e) => parts.push(format!("pane: {e}")),
            },
            Some(Err(e)) => parts.push(format!("pane left open: {e}")),
            None => {}
        }
        if !self.dry_run {
            self.store.remove(Kind::Live, &s.session_id);
        }
        Ok(parts.join("; "))
    }

    /// Start `claude --resume <id>` in `worktree`: a new window when tmux session `name` exists,
    /// else `FERMATA_RELAUNCH` if set, else a new detached session called `name`.
    /// Returns how it was launched.
    fn relaunch(&self, name: &str, worktree: &str, id: &str) -> Result<String> {
        if self.sys.tmux_session_exists(name) {
            self.sys.launch(name, worktree, &resume_cmd(id))?;
            return Ok(format!("new window in tmux {name}"));
        }
        match &self.relaunch {
            Some(tpl) => {
                self.sys.sh(&fill_relaunch(tpl, name, worktree, id))?;
                Ok(format!("FERMATA_RELAUNCH for {name}"))
            }
            None => {
                self.sys.launch(name, worktree, &resume_cmd(id))?;
                Ok(format!("new session {name}"))
            }
        }
    }

    /// Relaunch a closed session with `claude --resume <id>`. The session-start hook
    /// removes the closed record once the session starts.
    pub fn reopen(&self, s: &Session) -> Result<String> {
        if !valid_id(&s.session_id) {
            bail!("bad session id");
        }
        if s.worktree.is_empty() || !Path::new(&s.worktree).is_dir() {
            bail!("worktree {:?} is gone", s.worktree);
        }
        let name = s
            .tmux_session
            .clone()
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| format!("resume-{}", short(&s.session_id)));
        let how = self.relaunch(&name, &s.worktree, &s.session_id)?;
        Ok(format!(
            "{}reopened {} ({how})",
            if self.dry_run {
                "dry-run: would have "
            } else {
                ""
            },
            short(&s.session_id)
        ))
    }

    fn snap_dir(&self) -> PathBuf {
        self.store.root.join("snapshots")
    }

    /// Every live session whose claude still runs and that has a worktree and tmux session.
    pub fn snapshot_entries(&self) -> Vec<SnapEntry> {
        self.store
            .sessions(Kind::Live)
            .into_iter()
            .filter(|s| {
                s.claude_pid.is_some_and(|p| {
                    self.sys.pid_exists(p) && self.sys.process_name(p).is_none_or(|n| is_claude(&n))
                })
            })
            .filter_map(|s| {
                let tmux_session = s.tmux_session.filter(|t| !t.is_empty())?;
                (!s.worktree.is_empty()).then_some(SnapEntry {
                    tmux_session,
                    worktree: s.worktree,
                    branch: s.branch,
                    session_id: Some(s.session_id),
                    status: Some(s.status),
                    last_activity: s.last_activity,
                })
            })
            .collect()
    }

    /// Write `snapshots/<UTC time>.json` and a copy as `snapshots/latest.json` under the store root.
    pub fn snapshot_save(&self) -> Result<String> {
        let sessions = self.snapshot_entries();
        if sessions.is_empty() {
            return Ok("no live sessions to snapshot".into());
        }
        let n = sessions.len();
        if self.dry_run {
            return Ok(format!("dry-run: would snapshot {n} session(s)"));
        }
        let now = chrono::Utc::now();
        let file = SnapFile {
            created: now.to_rfc3339(),
            sessions,
        };
        let body = serde_json::to_string_pretty(&file)? + "\n";
        let dir = self.snap_dir();
        std::fs::create_dir_all(&dir)?;
        let name = format!("{}.json", now.format("%Y%m%dT%H%M%SZ"));
        std::fs::write(dir.join(&name), &body)?;
        let tmp = dir.join(format!("latest.json.tmp{}", std::process::id()));
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, dir.join("latest.json"))?;
        Ok(format!("snapshot of {n} session(s) saved ({name})"))
    }

    /// The latest snapshot: the root's, else ccmon's (`<legacy>/snapshots/latest.json`).
    pub fn latest_snapshot(&self) -> Result<(PathBuf, SnapFile)> {
        let mut dirs = vec![self.snap_dir()];
        dirs.extend(self.store.legacy.iter().map(|l| l.join("snapshots")));
        for d in dirs {
            let p = d.join("latest.json");
            if let Ok(txt) = std::fs::read_to_string(&p) {
                let f: SnapFile = serde_json::from_str(&txt)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
                return Ok((p, f));
            }
        }
        bail!("no snapshot saved yet")
    }

    /// Relaunch every session in the latest snapshot that is not running. A tmux session that
    /// already existed is skipped; a second entry for a session this restore created gets a
    /// window in it.
    pub fn snapshot_restore(&self) -> Result<String> {
        let (_, file) = self.latest_snapshot()?;
        let live: std::collections::HashSet<String> = self.store.live_ids().into_iter().collect();
        let mut created = std::collections::HashSet::new();
        let (mut launched, mut skipped, mut failed) = (0, vec![], vec![]);
        for e in &file.sessions {
            let id = e.session_id.as_deref().unwrap_or("");
            let why = if !valid_id(id) {
                Some("no session id")
            } else if live.contains(id) {
                Some("running")
            } else if !Path::new(&e.worktree).is_dir() {
                Some("worktree gone")
            } else if !created.contains(&e.tmux_session)
                && self.sys.tmux_session_exists(&e.tmux_session)
            {
                Some("tmux session exists")
            } else {
                None
            };
            if let Some(w) = why {
                skipped.push(format!("{} ({w})", e.tmux_session));
                continue;
            }
            match self.relaunch(&e.tmux_session, &e.worktree, id) {
                Ok(_) => {
                    created.insert(e.tmux_session.clone());
                    launched += 1;
                }
                Err(err) => failed.push(format!("{}: {err}", e.tmux_session)),
            }
        }
        let mut msg = format!(
            "{}restored {launched}",
            if self.dry_run { "dry-run: " } else { "" }
        );
        if !skipped.is_empty() {
            msg += &format!(", skipped {}", skipped.join(", "));
        }
        if !failed.is_empty() {
            msg += &format!(", failed {}", failed.join("; "));
        }
        Ok(msg)
    }

    /// End live records whose claude is gone (crashed, killed, pane closed: no session-end hook
    /// fired), archiving named ones like session-end; drop unreadable records over an hour old.
    pub fn sweep(&self) -> Result<Vec<String>> {
        let mut out = vec![];
        for s in self.store.sessions(Kind::Live) {
            let (Some(pid), id) = (s.claude_pid, s.session_id.as_str()) else {
                continue;
            };
            let gone = !self.sys.pid_exists(pid)
                || self.sys.process_name(pid).is_some_and(|n| !is_claude(&n));
            if !gone || !valid_id(id) {
                continue;
            }
            let line = format!("{} (pid {pid} gone)", short(id));
            if self.dry_run {
                out.push(format!("would end {line}"));
                continue;
            }
            // Re-check under the lock: a hook may have rewritten the record since it was read.
            let ended = self.store.locked(|| {
                let now_pid = self
                    .store
                    .read(Kind::Live, id)
                    .and_then(|r| r.get("claude_pid").and_then(serde_json::Value::as_i64));
                now_pid == Some(pid) && self.store.end(id, true)
            })?;
            if ended {
                let _ = self
                    .store
                    .log(id, &format!("SWEPT: claude pid {pid} is gone"));
                out.push(format!("ended {line}"));
            }
        }
        for id in self.store.unreadable_live(SWEEP_UNREADABLE_AGE) {
            if self.dry_run {
                out.push(format!("would remove unreadable record {}", short(&id)));
            } else if self.store.locked(|| self.store.remove(Kind::Live, &id))? {
                out.push(format!("removed unreadable record {}", short(&id)));
            }
        }
        Ok(out)
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_dir(d: &Path, closed: bool) -> Vec<Session> {
        let k = if closed { Kind::Closed } else { Kind::Live };
        Store::new(d.parent().unwrap().to_path_buf(), d.join("none")).sessions(k)
    }

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A scratch dir holding a copy of a session JSON.
    fn scratch(name: &str) -> (Tmp, Actions, Session, Arc<FakeSystem>) {
        let d = std::env::temp_dir().join(format!("fermata-ccmon-{name}-{}", std::process::id()));
        std::fs::create_dir_all(d.join("sessions")).unwrap();
        let wt = d.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let body = serde_json::json!({
            "session_id": "abc-123", "claude_pid": 4242, "tmux_target": "scratch:1.1",
            "tmux_session": "scratch", "worktree": wt, "branch": "main", "status": "idle",
            "tool_calls": 7, "extra_field": "kept"
        });
        std::fs::write(d.join("sessions/abc-123.json"), body.to_string()).unwrap();
        let fake = Arc::new(FakeSystem::default());
        fake.procs.lock().unwrap().insert(4242, "claude".into());
        // claude 4242 runs under the shell of pane %8, which moved from 1.1 to 1.2.
        *fake.panes.lock().unwrap() = crate::panes::tests::table(
            vec![
                crate::panes::tests::pane("%3", 30, "scratch:1.1", "zsh", "/elsewhere"),
                crate::panes::tests::pane("%8", 80, "scratch:1.2", "claude", "/x"),
            ],
            &[(4242, 80), (80, 1), (30, 1)],
        );
        let c = Actions {
            store: Store::new(d.clone(), d.join("legacy")),
            relaunch: None,
            sys: fake.clone(),
            dry_run: false,
        };
        let s = load_dir(&d.join("sessions"), false).pop().unwrap();
        (Tmp(d), c, s, fake)
    }

    fn calls(f: &FakeSystem) -> Vec<String> {
        f.calls.lock().unwrap().clone()
    }

    #[test]
    fn describe_keeps_other_fields() {
        let (t, c, s, _f) = scratch("desc");
        c.describe(&s, "  new words ").unwrap();
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(t.0.join("sessions/abc-123.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["description"], "new words");
        assert_eq!(v["extra_field"], "kept");
        assert_eq!(v["tool_calls"], 7);
    }

    #[test]
    fn describe_moves_a_legacy_record() {
        let (t, c, s, _f) = scratch("legacy");
        let legacy = t.0.join("legacy/sessions");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::rename(
            t.0.join("sessions/abc-123.json"),
            legacy.join("abc-123.json"),
        )
        .unwrap();
        c.describe(&s, "moved").unwrap();
        assert!(!legacy.join("abc-123.json").exists());
        assert_eq!(
            c.store.read(Kind::Live, "abc-123").unwrap()["description"],
            "moved"
        );
    }

    #[test]
    fn close_records_kills_and_removes() {
        let (t, c, s, f) = scratch("close");
        let msg = c.close(&s, true).unwrap();
        assert!(msg.contains("killed 4242"), "{msg}");
        assert_eq!(calls(&f), vec!["kill 4242", "kill-pane %8"]);
        assert!(msg.contains("pane scratch:1.2 (moved) closed"), "{msg}");
        assert!(!t.0.join("sessions/abc-123.json").exists());
        let closed = load_dir(&t.0.join("closed"), true);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].tmux_session.as_deref(), Some("scratch"));
    }

    #[test]
    fn close_refuses_non_claude_pid() {
        let (_t, c, s, f) = scratch("pid");
        f.procs.lock().unwrap().insert(4242, "vim".into());
        let msg = c.close(&s, false).unwrap();
        assert!(msg.contains("not claude"), "{msg}");
        assert!(calls(&f).is_empty());
    }

    #[test]
    fn close_never_kills_a_pane_by_address_alone() {
        let (_t, c, s, f) = scratch("nopid");
        // No pane runs claude's process any more; the recorded address now holds a shell.
        *f.panes.lock().unwrap() = crate::panes::tests::table(
            vec![crate::panes::tests::pane(
                "%3",
                30,
                "scratch:1.1",
                "zsh",
                &s.worktree,
            )],
            &[(4242, 1), (30, 1)],
        );
        let msg = c.close(&s, true).unwrap();
        assert!(msg.contains("pane left open"), "{msg}");
        assert_eq!(calls(&f), vec!["kill 4242"]);
        // And when claude is not running at all, the pane is left alone too.
        let (_t, c, s, f) = scratch("dead");
        f.procs.lock().unwrap().clear();
        let msg = c.close(&s, true).unwrap();
        assert!(msg.contains("pane left open"), "{msg}");
        assert!(calls(&f).is_empty());
    }

    #[test]
    fn dry_run_writes_nothing() {
        let (t, mut c, s, _f) = scratch("dry");
        c.dry_run = true;
        c.describe(&s, "x").unwrap();
        c.close(&s, true).unwrap();
        assert!(t.0.join("sessions/abc-123.json").exists());
        assert!(!t.0.join("closed").exists());
    }

    #[test]
    fn reopen_paths() {
        let (_t, mut c, mut s, f) = scratch("reopen");
        s.closed = true;
        // No tmux session: a new detached session.
        let msg = c.reopen(&s).unwrap();
        assert!(
            msg.contains("reopened abc-123") && msg.contains("new session"),
            "{msg}"
        );
        // Session exists: a new window in it.
        f.sessions.lock().unwrap().insert("scratch".into());
        assert!(c.reopen(&s).unwrap().contains("new window"));
        // Gone, with a template configured: the template, quoted.
        f.sessions.lock().unwrap().clear();
        c.relaunch = Some("tmuxinator start code {worktree} {session_id}".into());
        c.reopen(&s).unwrap();
        let k = calls(&f);
        assert!(k[0].starts_with("launch scratch ") && k[0].contains("claude --resume abc-123"));
        assert!(k[1].starts_with("launch scratch "));
        assert_eq!(
            k[2],
            format!("sh tmuxinator start code '{}' 'abc-123'", s.worktree)
        );
        s.worktree = "/nonexistent/fermata".into();
        assert!(c.reopen(&s).is_err());
        s.session_id = "../etc".into();
        assert!(c.reopen(&s).is_err());
    }

    #[test]
    fn relaunch_template_quotes() {
        assert_eq!(
            fill_relaunch("x {session} {worktree} {session_id}", "a b", "/it's", "id"),
            r#"x 'a b' '/it'\''s' 'id'"#
        );
    }

    #[test]
    fn snapshot_save_then_restore() {
        let (t, c, _s, f) = scratch("snap");
        assert!(c.snapshot_save().unwrap().contains("1 session"));
        let (_, file) = c.latest_snapshot().unwrap();
        assert_eq!(file.sessions.len(), 1);
        assert_eq!(file.sessions[0].session_id.as_deref(), Some("abc-123"));
        // Still running: skipped.
        assert!(
            c.snapshot_restore()
                .unwrap()
                .contains("skipped scratch (running)")
        );
        // After a reboot: the record is gone, the tmux session too.
        std::fs::remove_file(t.0.join("sessions/abc-123.json")).unwrap();
        let msg = c.snapshot_restore().unwrap();
        assert!(msg.contains("restored 1"), "{msg}");
        assert!(calls(&f).iter().any(|c| c.starts_with("launch scratch ")));
        // The tmux session exists already: skipped, nothing launched.
        f.calls.lock().unwrap().clear();
        f.sessions.lock().unwrap().insert("scratch".into());
        assert!(
            c.snapshot_restore()
                .unwrap()
                .contains("tmux session exists")
        );
        assert!(calls(&f).is_empty());
    }

    #[test]
    fn snapshot_skips_dead_and_reads_ccmon_fallback() {
        let (t, c, _s, f) = scratch("snapdead");
        f.procs.lock().unwrap().clear();
        assert!(c.snapshot_save().unwrap().contains("no live sessions"));
        assert!(c.latest_snapshot().is_err());
        let legacy = t.0.join("legacy/snapshots");
        std::fs::create_dir_all(&legacy).unwrap();
        let wt = t.0.join("wt");
        std::fs::write(
            legacy.join("latest.json"),
            serde_json::json!({"created": "x", "sessions": [{
                "tmux_session": "old", "worktree": wt, "branch": "b", "tmc_arg": "z",
                "session_id": "def-456", "claude_pid": 1, "alive": true}]})
            .to_string(),
        )
        .unwrap();
        let msg = c.snapshot_restore().unwrap();
        assert!(msg.contains("restored 1"), "{msg}");
        assert!(calls(&f)[0].contains("claude --resume def-456"));
    }

    #[test]
    fn sweep_ends_dead_sessions() {
        let (t, c, _s, f) = scratch("sweep");
        // Alive: untouched.
        assert!(c.sweep().unwrap().is_empty());
        // pid reused by another program: ended. Unnamed, so not archived.
        f.procs.lock().unwrap().insert(4242, "vim".into());
        let out = c.sweep().unwrap();
        assert_eq!(out, vec!["ended abc-123 (pid 4242 gone)"]);
        assert!(!t.0.join("sessions/abc-123.json").exists());
        assert!(!t.0.join("closed").exists());
        assert!(
            std::fs::read_to_string(c.store.log_path("abc-123"))
                .unwrap()
                .contains("SWEPT")
        );
    }

    #[test]
    fn sweep_archives_named_and_respects_dry_run() {
        let (t, mut c, s, f) = scratch("sweepnamed");
        c.describe(&s, "named work").unwrap();
        f.procs.lock().unwrap().clear();
        c.dry_run = true;
        assert_eq!(
            c.sweep().unwrap(),
            vec!["would end abc-123 (pid 4242 gone)"]
        );
        assert!(t.0.join("sessions/abc-123.json").exists());
        c.dry_run = false;
        c.sweep().unwrap();
        let closed = c.store.read(Kind::Closed, "abc-123").unwrap();
        assert_eq!(closed["description"], "named work");
        assert!(c.store.read(Kind::Live, "abc-123").is_none());
    }

    #[test]
    fn sweep_keeps_records_without_pid_and_fresh_unreadable() {
        let (t, c, _s, f) = scratch("sweepnopid");
        f.procs.lock().unwrap().clear();
        std::fs::write(
            t.0.join("sessions/abc-123.json"),
            r#"{"session_id":"abc-123","status":"idle"}"#,
        )
        .unwrap();
        std::fs::write(t.0.join("sessions/eee-1.json"), "").unwrap();
        assert!(c.sweep().unwrap().is_empty());
        assert!(t.0.join("sessions/eee-1.json").exists());
        assert_eq!(
            c.store.unreadable_live(Duration::ZERO),
            vec!["eee-1".to_string()]
        );
    }
}
