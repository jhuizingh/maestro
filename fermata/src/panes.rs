//! Finds a ccmon session's tmux pane by process. ccmon records `tmux_target` once, at
//! SessionStart, and pane indexes shift as panes come and go, so that address goes stale.
use crate::model::Session;
use crate::sessions::same_path;
use crate::util::run_cmd;
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a pane table is reused for anything that is not destructive.
pub const CACHE_TTL: Duration = Duration::from_secs(2);
/// Ancestors of `claude_pid` checked against pane pids, to allow for a wrapper process.
const MAX_ANCESTORS: usize = 3;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneInfo {
    /// Stable pane id, `%NN`.
    pub id: String,
    pub pid: i64,
    /// `session:window.pane`, which moves.
    pub addr: String,
    pub command: String,
    pub path: String,
}

/// Every tmux pane plus the process table's pid → ppid map, read together.
#[derive(Debug, Clone, Default)]
pub struct PaneTable {
    pub panes: Vec<PaneInfo>,
    pub ppid: HashMap<i64, i64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Use {
    /// Typing, jumping, capturing: the recorded address may stand in if its cwd checks out.
    Interact,
    /// kill-pane: only a PID match will do.
    Destroy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PaneRef {
    pub id: String,
    pub addr: String,
    pub command: String,
    /// Found through the session's claude process, which is therefore running in it.
    pub by_pid: bool,
    /// The pane no longer sits at the recorded address.
    pub moved: bool,
}

impl PaneRef {
    /// Claude Code queues input typed while it works, and its input line is safe to clear.
    pub fn is_claude(&self) -> bool {
        self.by_pid || self.command == "claude"
    }

    pub fn label(&self) -> String {
        if self.moved {
            format!("{} (moved)", self.addr)
        } else {
            self.addr.clone()
        }
    }
}

/// The pane `s` runs in: its claude pid (or up to three ancestors) as a pane's pid, else the
/// recorded address when that pane's cwd is the session's worktree. Destroy never falls back.
pub fn resolve(table: &PaneTable, s: &Session, use_: Use) -> Result<PaneRef> {
    let recorded = s.tmux_target.as_deref().unwrap_or("");
    let found = |p: &PaneInfo, by_pid: bool| PaneRef {
        id: p.id.clone(),
        addr: p.addr.clone(),
        command: p.command.clone(),
        by_pid,
        moved: !recorded.is_empty() && p.addr != recorded && p.id != recorded,
    };
    if let Some(pid) = s.claude_pid.filter(|p| table.ppid.contains_key(p)) {
        let mut cur = pid;
        for _ in 0..=MAX_ANCESTORS {
            if let Some(p) = table.panes.iter().find(|p| p.pid == cur) {
                return Ok(found(p, true));
            }
            match table.ppid.get(&cur) {
                Some(&pp) if pp > 1 => cur = pp,
                _ => break,
            }
        }
    }
    if use_ == Use::Destroy {
        bail!("no pane runs this session's claude process; refusing to close by address");
    }
    match table
        .panes
        .iter()
        .find(|p| !recorded.is_empty() && (p.addr == recorded || p.id == recorded))
    {
        Some(p) if same_path(&p.path, &s.worktree) => Ok(found(p, false)),
        _ => bail!("session pane not found; it may have closed"),
    }
}

const FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{session_name}:#{window_index}.#{pane_index}\t#{pane_current_command}\t#{pane_current_path}";

pub fn parse_panes(out: &str) -> Vec<PaneInfo> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(5, '\t').collect();
            Some(PaneInfo {
                id: f.first()?.to_string(),
                pid: f.get(1)?.trim().parse().ok()?,
                addr: f.get(2)?.to_string(),
                command: f.get(3)?.to_string(),
                path: f.get(4)?.to_string(),
            })
        })
        .collect()
}

pub fn parse_ppids(out: &str) -> HashMap<i64, i64> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace().map(|n| n.parse::<i64>());
            Some((it.next()?.ok()?, it.next()?.ok()?))
        })
        .collect()
}

static CACHE: Mutex<Option<(Instant, PaneTable)>> = Mutex::new(None);

/// The live pane table, reused when younger than `max_age` (pass zero for a fresh read).
pub fn real_table(max_age: Duration) -> Result<PaneTable> {
    if let Some((at, t)) = CACHE.lock().unwrap().as_ref()
        && at.elapsed() < max_age
    {
        return Ok(t.clone());
    }
    let run = |p: &str, a: &[&str]| {
        let a: Vec<String> = a.iter().map(|s| s.to_string()).collect();
        let out = run_cmd(p, &a, None, &[], None, Duration::from_secs(3))?;
        if !out.ok() {
            bail!("{p}: {}", out.stderr.trim());
        }
        Ok(out.stdout)
    };
    let t = PaneTable {
        panes: parse_panes(&run("tmux", &["list-panes", "-a", "-F", FORMAT])?),
        ppid: parse_ppids(&run("ps", &["-A", "-o", "pid=,ppid="])?),
    };
    *CACHE.lock().unwrap() = Some((Instant::now(), t.clone()));
    Ok(t)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn pane(id: &str, pid: i64, addr: &str, cmd: &str, path: &str) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            pid,
            addr: addr.into(),
            command: cmd.into(),
            path: path.into(),
        }
    }

    pub fn table(panes: Vec<PaneInfo>, ppid: &[(i64, i64)]) -> PaneTable {
        PaneTable {
            panes,
            ppid: ppid.iter().copied().collect(),
        }
    }

    pub fn sess(pid: Option<i64>, target: &str, wt: &str) -> Session {
        Session {
            claude_pid: pid,
            tmux_target: Some(target.into()),
            worktree: wt.into(),
            ..Default::default()
        }
    }

    #[test]
    fn moved_pane_resolves_to_its_new_id() {
        // Recorded at w:1.2; a new pane took index 2 and claude's shell is now 1.3.
        let t = table(
            vec![
                pane("%5", 100, "w:1.2", "zsh", "/elsewhere"),
                pane("%7", 200, "w:1.3", "claude", "/wt"),
            ],
            &[(201, 200), (200, 1), (100, 1)],
        );
        let p = resolve(&t, &sess(Some(201), "w:1.2", "/wt"), Use::Interact).unwrap();
        assert_eq!((p.id.as_str(), p.moved, p.by_pid), ("%7", true, true));
        assert_eq!(p.label(), "w:1.3 (moved)");
        let p = resolve(&t, &sess(Some(201), "w:1.2", "/wt"), Use::Destroy).unwrap();
        assert_eq!(p.id, "%7");
    }

    #[test]
    fn claude_as_the_pane_command_and_wrapper_ancestors() {
        let t = table(
            vec![pane("%1", 300, "w:1.1", "claude", "/wt")],
            &[(300, 1), (303, 302), (302, 301), (301, 300), (404, 403)],
        );
        let direct = resolve(&t, &sess(Some(300), "w:1.1", "/wt"), Use::Destroy).unwrap();
        assert_eq!((direct.id.as_str(), direct.moved), ("%1", false));
        // Three ancestors up is in range.
        assert!(resolve(&t, &sess(Some(303), "x", "/nope"), Use::Destroy).is_ok());
        // A live pid with no pane among its ancestors does not match.
        let t2 = table(
            vec![pane("%1", 300, "w:1.1", "zsh", "/wt")],
            &[(304, 303), (303, 302), (302, 301), (301, 300)],
        );
        assert!(resolve(&t2, &sess(Some(304), "x", "/nope"), Use::Interact).is_err());
    }

    #[test]
    fn dead_pid_falls_back_only_when_the_path_matches() {
        let t = table(vec![pane("%3", 50, "w:1.1", "zsh", "/wt/a/")], &[(50, 1)]);
        let p = resolve(&t, &sess(Some(999), "w:1.1", "/wt/a"), Use::Interact).unwrap();
        assert_eq!((p.id.as_str(), p.by_pid, p.moved), ("%3", false, false));
        assert!(!p.is_claude());
        let e = resolve(&t, &sess(Some(999), "w:1.1", "/wt/b"), Use::Interact).unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
        assert!(resolve(&t, &sess(None, "w:9.9", "/wt/a"), Use::Interact).is_err());
    }

    #[test]
    fn destroy_without_a_pid_match_is_refused() {
        let t = table(vec![pane("%3", 50, "w:1.1", "claude", "/wt")], &[(50, 1)]);
        for pid in [None, Some(999)] {
            let e = resolve(&t, &sess(pid, "w:1.1", "/wt"), Use::Destroy).unwrap_err();
            assert!(e.to_string().contains("refusing"), "{e}");
        }
    }

    #[test]
    fn parses_tmux_and_ps_output() {
        let p = parse_panes("%1\t42\tmain:1.1\tclaude\t/a dir/with\ttab\nbad\n");
        assert_eq!(
            p,
            vec![pane("%1", 42, "main:1.1", "claude", "/a dir/with\ttab")]
        );
        let m = parse_ppids("  1     0\n 42  1\nPID PPID\n");
        assert_eq!((m.get(&42), m.len()), (Some(&1), 2));
    }
}
