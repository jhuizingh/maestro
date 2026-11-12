use anyhow::{Context as _, Result, anyhow};
use chrono::{DateTime, Utc};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

/// Run a command with an optional stdin payload, killing it after `timeout`.
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
        .stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().with_context(|| format!("spawn {program}"))?;
    if let Some(data) = stdin {
        let mut si = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let data = data.to_vec();
        std::thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let mut so = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut se = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;
    let t_out = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t_err = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("{program} timed out after {}s", timeout.as_secs()));
        }
        std::thread::sleep(Duration::from_millis(15));
    };
    let stdout = String::from_utf8_lossy(&t_out.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    Ok(CmdOutput {
        status: status.code(),
        stdout,
        stderr,
    })
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
}
