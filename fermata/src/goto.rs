//! `fermata goto`: jump to the running fermata pane, or start one in its own tmux session.
//! Found by process (`pane_current_command`), never by a remembered address.
use crate::actions::tmux;
use anyhow::{Result, bail};

/// tmux session fermata starts in when none is running: `$FERMATA_SESSION`, else `fermata`.
pub fn session_name() -> String {
    std::env::var("FERMATA_SESSION")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "fermata".into())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pane {
    pub id: String,
    pub session: String,
    pub command: String,
}

/// `list-panes -a` output, one `id \t session \t command` per line.
pub fn parse(out: &str) -> Vec<Pane> {
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some(Pane {
                id: f.next()?.to_string(),
                session: f.next()?.to_string(),
                command: f.next()?.to_string(),
            })
        })
        .collect()
}

/// The pane to jump to: one running fermata, preferring the one in `home`.
pub fn pick<'a>(panes: &'a [Pane], home: &str) -> Option<&'a Pane> {
    let running = || panes.iter().filter(|p| p.command == "fermata");
    running()
        .find(|p| p.session == home)
        .or_else(|| running().next())
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Find or start fermata, then switch this client to it (attach when outside tmux).
pub fn run() -> Result<()> {
    let home = session_name();
    if home.contains(['.', ':']) {
        bail!("FERMATA_SESSION {home:?}: tmux reads '.' and ':' as target separators");
    }
    let out = tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}\t#{session_name}\t#{pane_current_command}",
    ])
    .unwrap_or_default();
    let pane = match pick(&parse(&out), &home) {
        Some(p) => p.id.clone(),
        None => start(&home)?,
    };
    if std::env::var_os("TMUX").is_some() {
        tmux(&["switch-client", "-t", &pane])?;
        tmux(&["select-window", "-t", &pane])?;
        tmux(&["select-pane", "-t", &pane])?;
    } else {
        tmux(&["select-window", "-t", &pane])?;
        tmux(&["select-pane", "-t", &pane])?;
        let st = std::process::Command::new("tmux")
            .args(["attach-session", "-t", &format!("={home}")])
            .status()?;
        if !st.success() {
            bail!("tmux attach-session failed");
        }
    }
    Ok(())
}

/// Start fermata in a fresh pane of `home` (a new window if the session exists, so nothing
/// already in it gets typed into). send-keys, not the pane command: the shell outlives fermata.
fn start(home: &str) -> Result<String> {
    let exists = tmux(&["has-session", "-t", &format!("={home}")]).is_ok();
    let pane = if exists {
        tmux(&[
            "new-window",
            "-t",
            &format!("={home}:"),
            "-P",
            "-F",
            "#{pane_id}",
        ])?
    } else {
        tmux(&["new-session", "-d", "-s", home, "-P", "-F", "#{pane_id}"])?
    };
    let pane = pane.trim().to_string();
    let exe = std::env::current_exe()?;
    tmux(&[
        "send-keys",
        "-t",
        &pane,
        &sh_quote(&exe.to_string_lossy()),
        "C-m",
    ])?;
    Ok(pane)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_fermata_preferring_home() {
        let p = parse("%1\twork\tzsh\n%2\tother\tfermata\n%3\tfermata\tfermata\nbad line\n");
        assert_eq!(p.len(), 3);
        assert_eq!(pick(&p, "fermata").unwrap().id, "%3");
        assert_eq!(pick(&p, "nope").unwrap().id, "%2");
        assert!(pick(&p[..1], "fermata").is_none());
    }
}
