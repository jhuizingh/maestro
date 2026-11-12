//! ccmon session actions: describe, close, reopen, snapshot save/restore.
//! Live effects go through `System` so tests (and --dry-run) never touch real processes.
use crate::log;
use crate::model::Session;
use crate::util::run_cmd;
use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Everything that reaches outside ccmon's JSON files.
pub trait System: Send + Sync {
    /// Process name of `pid`, or None if it is not running.
    fn process_name(&self, pid: i64) -> Option<String>;
    /// SIGTERM, wait up to 2s, then SIGKILL. True when the process is gone.
    fn kill_pid(&self, pid: i64) -> bool;
    fn kill_pane(&self, target: &str) -> Result<()>;
    fn tmux_session_exists(&self, name: &str) -> bool;
    /// Run `cmd` in `dir`: a new window in `session` if it exists, else a new detached session.
    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()>;
    fn snapshot(&self, script: &Path, args: &[&str]) -> Result<String>;
}

pub struct RealSystem;

fn tmux(args: &[&str]) -> Result<String> {
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

    fn kill_pane(&self, target: &str) -> Result<()> {
        tmux(&["kill-pane", "-t", target]).map(|_| ())
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

    fn snapshot(&self, script: &Path, args: &[&str]) -> Result<String> {
        let mut a = vec![script.to_string_lossy().into_owned()];
        a.extend(args.iter().map(|s| s.to_string()));
        let out = run_cmd("python3", &a, None, &[], None, Duration::from_secs(60))?;
        if !out.ok() {
            bail!("{}", out.stderr.trim());
        }
        Ok(out.stdout)
    }
}

/// Records what it would do; used for --dry-run and in tests.
#[derive(Default)]
pub struct FakeSystem {
    pub calls: Mutex<Vec<String>>,
    pub procs: Mutex<std::collections::HashMap<i64, String>>,
    pub sessions: Vec<String>,
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
    fn kill_pid(&self, pid: i64) -> bool {
        self.rec(format!("kill {pid}"));
        self.procs.lock().unwrap().remove(&pid);
        true
    }
    fn kill_pane(&self, target: &str) -> Result<()> {
        self.rec(format!("kill-pane {target}"));
        Ok(())
    }
    fn tmux_session_exists(&self, name: &str) -> bool {
        self.sessions.iter().any(|s| s == name)
    }
    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()> {
        self.rec(format!("launch {session} {dir} {cmd}"));
        Ok(())
    }
    fn snapshot(&self, _script: &Path, args: &[&str]) -> Result<String> {
        self.rec(format!("snapshot {}", args.join(" ")));
        Ok("ok".into())
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
    fn kill_pid(&self, pid: i64) -> bool {
        self.rec.kill_pid(pid);
        false
    }
    fn kill_pane(&self, target: &str) -> Result<()> {
        self.rec.kill_pane(target)
    }
    fn tmux_session_exists(&self, name: &str) -> bool {
        RealSystem.tmux_session_exists(name)
    }
    fn launch(&self, session: &str, dir: &str, cmd: &str) -> Result<()> {
        self.rec.launch(session, dir, cmd)
    }
    fn snapshot(&self, script: &Path, args: &[&str]) -> Result<String> {
        self.rec.snapshot(script, args)
    }
}

pub struct Ccmon {
    pub dir: PathBuf,
    pub sys: Arc<dyn System>,
    /// When true, JSON files are not written either.
    pub dry_run: bool,
}

fn write_atomic(path: &Path, v: &serde_json::Value) -> Result<()> {
    let tmp = path.with_extension(format!("json.fermata{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(v)? + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `claude --resume <id>` inside a login shell that stays open afterwards.
pub fn resume_cmd(session_id: &str) -> String {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    format!("{shell} -lc 'claude --resume {session_id}; exec {shell} -l'")
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

impl Ccmon {
    pub fn new(dry_run: bool) -> Self {
        Self {
            dir: crate::sessions::ccmon_dir(),
            sys: if dry_run {
                Arc::new(DryRunSystem::default())
            } else {
                Arc::new(RealSystem)
            },
            dry_run,
        }
    }

    fn file(&self, s: &Session) -> Result<PathBuf> {
        if !valid_id(&s.session_id) {
            bail!("bad session id {:?}", s.session_id);
        }
        let sub = if s.closed { "closed" } else { "sessions" };
        Ok(self.dir.join(sub).join(format!("{}.json", s.session_id)))
    }

    /// Set `description` in the session's (or closed record's) JSON, keeping other fields.
    pub fn describe(&self, s: &Session, desc: &str) -> Result<String> {
        let p = self.file(s)?;
        let mut v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&p).with_context(|| p.display().to_string())?,
        )?;
        v["description"] = serde_json::Value::String(desc.trim().to_string());
        if self.dry_run {
            log::warn(format!("dry-run describe {}: {desc}", p.display()));
            return Ok(format!(
                "dry-run: would set description of {}",
                s.session_id
            ));
        }
        write_atomic(&p, &v)?;
        Ok(format!("description set for {}", short(&s.session_id)))
    }

    /// ccmon's save+close: write `closed/<id>.json`, kill the claude pid, optionally the pane,
    /// and drop the live session file.
    pub fn close(&self, s: &Session, kill_pane: bool) -> Result<String> {
        if s.closed {
            bail!("already closed");
        }
        let live = self.file(s)?;
        let closed = self
            .dir
            .join("closed")
            .join(format!("{}.json", s.session_id));
        let rec = serde_json::json!({
            "session_id": s.session_id,
            "description": s.description.clone().unwrap_or_default(),
            "worktree": s.worktree,
            "branch": s.branch.clone().unwrap_or_default(),
            "tmux_session": s.tmux_session.clone().unwrap_or_default(),
            "closed_at": chrono::Utc::now().to_rfc3339(),
        });
        let mut parts = vec![];
        if self.dry_run {
            parts.push(format!("dry-run: would write {}", closed.display()));
        } else {
            std::fs::create_dir_all(closed.parent().unwrap())?;
            write_atomic(&closed, &rec)?;
            parts.push("record saved".into());
        }
        match s.claude_pid {
            Some(pid) => match self.sys.process_name(pid) {
                Some(n) if n.contains("claude") || n.contains("node") => {
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
        if kill_pane && let Some(t) = &s.tmux_target {
            match self.sys.kill_pane(t) {
                Ok(()) if self.dry_run => parts.push(format!("would close pane {t}")),
                Ok(()) => parts.push(format!("pane {t} closed")),
                Err(e) => parts.push(format!("pane: {e}")),
            }
        }
        if !self.dry_run && live.exists() {
            std::fs::remove_file(&live)?;
        }
        Ok(parts.join("; "))
    }

    /// Relaunch a closed session with `claude --resume <id>`. ccmon's session_start hook
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
        let how = if self.sys.tmux_session_exists(&name) {
            "new window in"
        } else {
            "new session"
        };
        self.sys
            .launch(&name, &s.worktree, &resume_cmd(&s.session_id))?;
        Ok(format!(
            "{}reopened {} ({how} tmux {name})",
            if self.dry_run {
                "dry-run: would have "
            } else {
                ""
            },
            short(&s.session_id)
        ))
    }

    fn script(&self) -> PathBuf {
        self.dir.join("session_snapshot.py")
    }

    pub fn snapshot_save(&self) -> Result<String> {
        let out = self.sys.snapshot(&self.script(), &["save"])?;
        Ok(out.lines().next().unwrap_or("snapshot saved").to_string())
    }

    pub fn snapshot_restore(&self) -> Result<String> {
        let out = self.sys.snapshot(&self.script(), &["restore"])?;
        Ok(out
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("restore complete")
            .to_string())
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::load_dir;

    struct Tmp(PathBuf);
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A scratch ccmon dir holding a copy of a session JSON.
    fn scratch(name: &str) -> (Tmp, Ccmon, Session, Arc<FakeSystem>) {
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
        let c = Ccmon {
            dir: d.clone(),
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
    fn close_records_kills_and_removes() {
        let (t, c, s, f) = scratch("close");
        let msg = c.close(&s, true).unwrap();
        assert!(msg.contains("killed 4242"), "{msg}");
        assert_eq!(calls(&f), vec!["kill 4242", "kill-pane scratch:1.1"]);
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
    fn dry_run_writes_nothing() {
        let (t, mut c, s, _f) = scratch("dry");
        c.dry_run = true;
        c.describe(&s, "x").unwrap();
        c.close(&s, true).unwrap();
        assert!(t.0.join("sessions/abc-123.json").exists());
        assert!(!t.0.join("closed").exists());
    }

    #[test]
    fn reopen_and_snapshots() {
        let (_t, c, mut s, f) = scratch("reopen");
        s.closed = true;
        let msg = c.reopen(&s).unwrap();
        assert!(msg.contains("reopened abc-123"));
        c.snapshot_save().unwrap();
        c.snapshot_restore().unwrap();
        let k = calls(&f);
        assert!(k[0].starts_with("launch scratch ") && k[0].contains("claude --resume abc-123"));
        assert_eq!(&k[1..], ["snapshot save", "snapshot restore"]);
        s.worktree = "/nonexistent/fermata".into();
        assert!(c.reopen(&s).is_err());
        s.session_id = "../etc".into();
        assert!(c.reopen(&s).is_err());
    }
}
