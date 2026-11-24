use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How old a file's times must be before an unchanged stamp proves unchanged content: longer
/// than any filesystem's timestamp step, so a write right after a read cannot share its stamp.
pub const SETTLE: Duration = Duration::from_millis(2100);

/// What a write to a file changes. `ctime` moves on every write and cannot be set back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub len: u64,
    id: (u64, u64),
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    pub fn of(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt as _;
        Self {
            len: m.len(),
            id: (m.dev(), m.ino()),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }

    /// Whether its times were `SETTLE` in the past already; ask before reading the content.
    pub fn settled(&self) -> bool {
        let now = Utc::now() - chrono::Duration::from_std(SETTLE).unwrap_or_default();
        let last = self.mtime.max(self.ctime);
        last < (now.timestamp(), i64::from(now.timestamp_subsec_nanos()))
    }
}

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(p: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    if p == "~" {
        home
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(p)
    }
}

#[derive(Debug)]
pub struct CmdOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

/// `run_cmd`'s error when it killed the command at its timeout (downcast to tell it apart).
#[derive(Debug)]
pub struct TimedOut {
    pub program: String,
    pub secs: u64,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} timed out after {}s", self.program, self.secs)
    }
}

impl std::error::Error for TimedOut {}

/// Run a command with an optional stdin payload, killing it after `timeout`.
///
/// The whole run is bounded, output included: the command gets its own process group, and if it
/// or anything it forked still holds stdout/stderr at the deadline, the group is killed and the
/// reader threads are abandoned rather than joined.
pub fn run_cmd(
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    envs: &[(&str, &str)],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Result<CmdOutput> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let deadline = start + timeout;
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    let pgid = child.id() as libc::pid_t;
    if let Some(data) = stdin {
        let mut si = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let so = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let se = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    for (is_err, mut pipe) in [
        (false, Box::new(so) as Box<dyn Read + Send>),
        (true, Box::new(se)),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = pipe.read_to_end(&mut b);
            let _ = tx.send((is_err, b));
        });
    }
    drop(tx);
    let timed_out = || -> anyhow::Error {
        TimedOut {
            program: program.to_string(),
            secs: timeout.as_secs(),
        }
        .into()
    };
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if Instant::now() >= deadline {
            kill_group(pgid);
            let _ = child.kill();
            let _ = child.wait();
            return Err(timed_out());
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    // The child exited, but a forked descendant may still hold a pipe open.
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok((false, b)) => stdout = b,
            Ok((true, b)) => stderr = b,
            Err(_) => {
                kill_group(pgid);
                return Err(timed_out());
            }
        }
    }
    Ok(CmdOutput {
        status: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// SIGKILL every process left in `run_cmd`'s per-command process group.
fn kill_group(pgid: libc::pid_t) {
    // SAFETY: kill(2) takes no pointers; a negative pid addresses the process group.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Local `MM-DD HH:MM` for RFC 3339 stamps; date-only stamps pass through.
pub fn short_ts(s: &str) -> String {
    match parse_ts(s) {
        Some(t) => t
            .with_timezone(&chrono::Local)
            .format("%m-%d %H:%M")
            .to_string(),
        None => s.chars().take(16).collect(),
    }
}

/// Compact relative age: `45s`, `12m`, `3h`, `5d`.
pub fn rel_age(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = (now - t).num_seconds().max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// Truncate to at most `n` chars, adding an ellipsis.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde() {
        let h = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/x"), h.join("x"));
        assert_eq!(expand_tilde("/a"), PathBuf::from("/a"));
    }

    #[test]
    fn ages() {
        let now = parse_ts("2026-11-12T12:00:00Z").unwrap();
        assert_eq!(
            rel_age(parse_ts("2026-11-12T11:59:30Z").unwrap(), now),
            "30s"
        );
        assert_eq!(
            rel_age(parse_ts("2026-11-12T10:00:00Z").unwrap(), now),
            "2h"
        );
        assert_eq!(
            rel_age(parse_ts("2026-11-10T12:00:00Z").unwrap(), now),
            "2d"
        );
    }

    #[test]
    fn trunc() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("ab", 4), "ab");
    }

    #[test]
    fn cmd_timeout_and_stdin() {
        let out = run_cmd("cat", &[], None, &[], Some(b"hi"), Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, "hi");
        let r = run_cmd(
            "sleep",
            &["5".into()],
            None,
            &[],
            None,
            Duration::from_millis(200),
        );
        assert!(r.is_err());
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn pid_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fermata-{tag}-{}", std::process::id()))
    }

    /// A grandchild holding stdout after its parent exits must not outlive the timeout.
    #[test]
    fn cmd_orphan_holding_pipe_is_bounded_and_killed() {
        let f = pid_file("orphan");
        let script = format!("sleep 30 & echo $! > {}; echo hi", f.display());
        let t = Instant::now();
        let r = run_cmd(
            "sh",
            &["-c".into(), script],
            None,
            &[],
            None,
            Duration::from_millis(500),
        );
        assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
        assert!(r.unwrap_err().downcast_ref::<TimedOut>().is_some());
        let pid = std::fs::read_to_string(&f).unwrap();
        let _ = std::fs::remove_file(&f);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(&pid), "orphan {pid} survived");
    }

    /// A command still running at the timeout takes its whole process group with it.
    #[test]
    fn cmd_timeout_kills_descendants() {
        let f = pid_file("descendant");
        let script = format!("sleep 30 & echo $! > {}; wait", f.display());
        let r = run_cmd(
            "sh",
            &["-c".into(), script],
            None,
            &[],
            None,
            Duration::from_millis(500),
        );
        assert!(r.is_err());
        let pid = std::fs::read_to_string(&f).unwrap();
        let _ = std::fs::remove_file(&f);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(&pid), "descendant {pid} survived");
    }

    /// A background child that let go of the pipes is left alone and the output returns at once.
    #[test]
    fn cmd_detached_background_child_returns_promptly() {
        let t = Instant::now();
        let out = run_cmd(
            "sh",
            &["-c".into(), "sleep 2 >/dev/null 2>&1 & echo hi".into()],
            None,
            &[],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out.stdout.trim(), "hi");
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }
}
