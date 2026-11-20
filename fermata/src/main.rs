mod adapter;
mod app;
mod baton;
mod cache;
mod ccmon;
mod config;
mod core;
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
mod ui;
mod util;
mod watch;

use anyhow::{Result, bail};
use std::sync::Arc;

const HELP: &str = "fermata — a dashboard for orchestrated Claude Code work

USAGE:
  fermata [--dry-run] [--context <name>]
                                 run the TUI (--dry-run: log input instead of sending/noting;
                                 --context: start showing that one context)
  fermata --dump [--task <id>]   print the joined task list (or one task + timeline) as JSON
  fermata --dump-sessions        print live session records as JSON
  fermata hook <event>           Claude Code hook: read its JSON on stdin, update the session
                                 record, always exit 0 (events: session-start, session-end,
                                 notification, stop, subagent-stop, user-prompt-submit,
                                 pre-tool-use)
  fermata --version
  fermata --help";

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Default, Debug, PartialEq)]
struct Args {
    dump: bool,
    dump_sessions: bool,
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
        })
    } else {
        serde_json::json!({
            "loaded_at": snap.loaded_at,
            "baton": loader.baton,
            "contexts": snap.contexts,
            "errors": snap.errors,
            "me": snap.me,
            "tasks": snap.tasks,
        })
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// `fermata hook <event>`. Never fails: a broken hook must not block Claude.
fn hook(event: Option<String>) -> ! {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let event = event.unwrap_or_default();
    let store = store::Store::open();
    if let Err(e) = hook::run(&event, &input, &store, &hook::RealProbe) {
        log::warn(format!("hook {event}: {e:#}"));
    }
    std::process::exit(0)
}

fn main() -> Result<()> {
    let mut argv = std::env::args().skip(1).peekable();
    if argv.peek().is_some_and(|a| a == "hook") {
        hook(argv.nth(1));
    }
    let args = parse_args(std::env::args().skip(1))?;
    if args.help {
        println!("{HELP}");
        return Ok(());
    }
    if args.version {
        println!("fermata {VERSION}");
        return Ok(());
    }
    if args.dump_sessions {
        println!(
            "{}",
            serde_json::to_string_pretty(&sessions::live_sessions())?
        );
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
        let a = parse_args(["--context", "work"].map(String::from)).unwrap();
        assert_eq!(a.context.as_deref(), Some("work"));
        assert!(parse_args(["--context".to_string()]).is_err());
    }
}
