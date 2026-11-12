mod adapter;
mod app;
mod baton;
mod cache;
mod ccmon;
mod config;
mod core;
mod history;
mod log;
mod model;
mod notes;
mod pr;
mod routing;
mod sessions;
mod signature;
mod transcript;
mod ui;
mod util;
mod watch;

use anyhow::{Result, bail};
use std::sync::Arc;

const HELP: &str = "fermata — a dashboard for orchestrated Claude Code work

USAGE:
  fermata [--dry-run]            run the TUI (--dry-run: log input instead of sending/noting)
  fermata --dump [--task <id>]   print the joined task list (or one task + timeline) as JSON
  fermata --dump-sessions        print ccmon sessions as JSON
  fermata --help";

#[derive(Default, Debug, PartialEq)]
struct Args {
    dump: bool,
    dump_sessions: bool,
    dry_run: bool,
    task: Option<String>,
    help: bool,
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
            "-h" | "--help" => a.help = true,
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
            "tasks": snap.tasks,
        })
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn main() -> Result<()> {
    let args = parse_args(std::env::args().skip(1))?;
    if args.help {
        println!("{HELP}");
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
    app::run(args.dry_run)
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
    }
}
