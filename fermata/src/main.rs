mod actions;
mod adapter;
mod app;
mod attribution;
mod baton;
mod cache;
mod config;
mod conversation;
mod core;
mod delivery;
mod fetch;
mod goto;
mod health;
mod history;
mod hook;
mod log;
mod model;
mod notes;
mod panes;
mod pr;
mod review;
mod routing;
mod sessions;
mod signature;
mod store;
mod theme;
mod transcript;
mod turns;
mod ui;
mod util;
mod watch;
mod worktrees;

use anyhow::{Result, bail};
use std::sync::Arc;

const HELP: &str = "fermata — a dashboard for orchestrated Claude Code work

USAGE:
  fermata [--dry-run] [--context <name>]
                                 run the TUI (--dry-run: log input instead of sending/noting;
                                 --context: start showing that one context)
  fermata --dump [--task <id>]   print the joined task list (or one task + timeline and
                                 conversation) as JSON
  fermata --dump-sessions        print live session records as JSON
  fermata --doctor               load once and report each source's health (exit 1 if degraded)
  fermata hook <event>           Claude Code hook: read its JSON on stdin, update the session
                                 record, always exit 0 (events: session-start, session-end,
                                 notification, stop, subagent-stop, user-prompt-submit,
                                 pre-tool-use)
  fermata goto                   jump to the running fermata pane, or start fermata in a tmux
                                 session ($FERMATA_SESSION, default \"fermata\") and jump there
  fermata cleanup [--dry-run]    end records whose claude process is gone (the TUI does this
                                 every minute)
  fermata snapshot save|restore|list
                                 save the running sessions; relaunch the latest snapshot's
                                 sessions that are not running; list saved snapshots
  fermata --version              version, git commit and build time
  fermata --help";

#[derive(Default, Debug, PartialEq)]
struct Args {
    dump: bool,
    dump_sessions: bool,
    doctor: bool,
    dry_run: bool,
    task: Option<String>,
    context: Option<String>,
    help: bool,
    version: bool,
}

fn parse_args(it: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut a = Args::default();
    let mut it = it.into_iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--dump" => a.dump = true,
            "--dump-sessions" => a.dump_sessions = true,
            "--doctor" => a.doctor = true,
            "--dry-run" => a.dry_run = true,
            "--task" => a.task = it.next(),
            "--context" => match it.next() {
                Some(c) => a.context = Some(c),
                None => bail!("--context needs a context name\n\n{HELP}"),
            },
            "-h" | "--help" => a.help = true,
            "-V" | "--version" => a.version = true,
            other => bail!("unknown argument: {other}\n\n{HELP}"),
        }
    }
    Ok(a)
}

fn dump(args: &Args) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    let loader = Arc::new(core::Loader::new());
    let snap = rt.block_on(loader.load(false));
    let out = if let Some(id) = &args.task {
        let Some(t) = snap.tasks.iter().find(|t| &t.task.id == id) else {
            bail!("task {id} is not in progress in any context");
        };
        let mut all = snap.sessions.clone();
        all.extend(sessions::closed_sessions());
        serde_json::json!({
            "task": t,
            "timeline": history::timeline(t, &all),
            "conversation": conversation::load(
                &loader,
                snap.contexts.iter().find(|c| c.name == t.context),
                t,
                &all,
            ),
        })
    } else {
        serde_json::json!({
            "loaded_at": snap.loaded_at,
            "baton": loader.baton,
            "contexts": snap.contexts,
            "errors": snap.errors,
            "me": snap.me,
            "tasks": snap.tasks,
            "backlog": snap.backlog,
            "tracker_stale": snap.tracker_stale,
            "health": snap.health,
        })
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Load once and print the health report; true when nothing is degraded.
fn doctor() -> Result<bool> {
    let rt = tokio::runtime::Runtime::new()?;
    let loader = Arc::new(core::Loader::new());
    let snap = rt.block_on(loader.load(false));
    print!(
        "{}",
        health::report(&snap.health, &health::paths(loader.baton.as_deref()))
    );
    for e in &snap.errors {
        println!("error: {e}");
    }
    for (c, st) in &snap.tracker_stale {
        println!("stale: {c} tasks {}", st.text());
    }
    Ok(!snap.health.iter().any(|r| r.degraded))
}

/// `fermata hook <event>`. Never fails: a broken hook must not block Claude.
fn hook(event: Option<String>) -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let event = event.unwrap_or_default();
    let store = store::Store::open();
    match hook::run(&event, &input, &store, &hook::RealProbe) {
        Ok(Some(out)) => println!("{out}"),
        Ok(None) => {}
        Err(e) => log::warn(format!("hook {event}: {e:#}")),
    }
    std::process::exit(0)
}

/// The non-TUI subcommands: `goto`, `cleanup [--dry-run]`, `snapshot save|restore|list`.
fn command(v: &[String]) -> Result<()> {
    let rest: Vec<&str> = v[1..].iter().map(String::as_str).collect();
    match (v[0].as_str(), rest.as_slice()) {
        ("goto", []) => goto::run(),
        ("cleanup", [] | ["--dry-run"]) => {
            let out = actions::Actions::new(!rest.is_empty()).sweep()?;
            if out.is_empty() {
                println!("nothing to clean up");
            }
            for l in out {
                println!("{l}");
            }
            Ok(())
        }
        ("snapshot", ["save"]) => {
            println!("{}", actions::Actions::new(false).snapshot_save()?);
            Ok(())
        }
        ("snapshot", ["restore"]) => {
            println!("{}", actions::Actions::new(false).snapshot_restore()?);
            Ok(())
        }
        ("snapshot", ["list"]) => {
            let a = actions::Actions::new(false);
            match a.latest_snapshot() {
                Ok((p, f)) => {
                    println!(
                        "latest: {} ({}, {} session(s))",
                        p.display(),
                        f.created,
                        f.sessions.len()
                    );
                    for e in f.sessions {
                        println!("  {}  {}", e.tmux_session, e.worktree);
                    }
                }
                Err(e) => println!("{e}"),
            }
            Ok(())
        }
        _ => bail!("bad arguments: fermata {}\n\n{HELP}", v.join(" ")),
    }
}

fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1).peekable();
    match argv.peek().map(String::as_str) {
        Some("hook") => hook(argv.nth(1)),
        Some("goto" | "cleanup" | "snapshot") => {
            let v: Vec<String> = argv.collect();
            return command(&v);
        }
        _ => {}
    }
    let args = parse_args(std::env::args().skip(1))?;
    if args.help {
        println!("{HELP}");
        return Ok(());
    }
    if args.version {
        println!("{}", health::build_line());
        return Ok(());
    }
    if args.dump_sessions {
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions::live_sessions())?
        );
        return Ok(());
    }
    if args.doctor {
        if !doctor()? {
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.dump {
        return dump(&args);
    }
    app::run(args.dry_run, args.context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let a = parse_args(["--dump", "--task", "ctx-1"].map(String::from)).unwrap();
        assert!(a.dump);
        assert_eq!(a.task.as_deref(), Some("ctx-1"));
        assert!(parse_args(["--nope".to_string()]).is_err());
        assert!(parse_args(["--dry-run".to_string()]).unwrap().dry_run);
        assert!(parse_args(["--version".to_string()]).unwrap().version);
        assert!(parse_args(["-V".to_string()]).unwrap().version);
        assert!(parse_args(["--doctor".to_string()]).unwrap().doctor);
        let a = parse_args(["--context", "work"].map(String::from)).unwrap();
        assert_eq!(a.context.as_deref(), Some("work"));
        assert!(parse_args(["--context".to_string()]).is_err());
    }

    #[test]
    fn subcommand_args_are_checked() {
        for bad in [
            &["goto", "x"][..],
            &["cleanup", "--force"],
            &["snapshot"],
            &["snapshot", "pause"],
        ] {
            let v: Vec<String> = bad.iter().map(|s| s.to_string()).collect();
            assert!(command(&v).is_err(), "{bad:?}");
        }
    }
}
