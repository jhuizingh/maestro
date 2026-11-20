//! `fermata hook <event>`: Claude Code hook handlers that keep the session records current.
//!
//! Each reads the hook JSON on stdin and updates the session's record through `Store`. A hook
//! must never block or slow Claude, so `main` always exits 0 and errors only reach the log.
use crate::store::{Kind, Record, Store, now};
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub const EVENTS: &[&str] = &[
    "session-start",
    "session-end",
    "notification",
    "stop",
    "subagent-stop",
    "user-prompt-submit",
    "pre-tool-use",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tmux {
    pub session: String,
    pub window: String,
    pub pane: String,
    /// The stable `%NN` pane id (`$TMUX_PANE`).
    pub pane_id: String,
}

impl Tmux {
    pub fn target(&self) -> String {
        format!("{}:{}.{}", self.session, self.window, self.pane)
    }
}

/// What a hook learns from its environment rather than its payload.
pub trait Probe {
    fn tmux(&self) -> Option<Tmux>;
    fn branch(&self, cwd: &str) -> String;
    /// The Claude Code process that ran this hook.
    fn claude_pid(&self) -> i64;
}

pub struct RealProbe;

fn output(cmd: &str, args: &[&str]) -> Option<String> {
    let o = std::process::Command::new(cmd)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

impl Probe for RealProbe {
    fn tmux(&self) -> Option<Tmux> {
        std::env::var_os("TMUX")?;
        let pane_id = std::env::var("TMUX_PANE").unwrap_or_default();
        let mut args = vec!["display-message", "-p"];
        if !pane_id.is_empty() {
            args.extend(["-t", &pane_id]);
        }
        args.push("#{session_name}\t#{window_index}\t#{pane_index}");
        let out = output("tmux", &args)?;
        let mut it = out.split('\t').map(str::to_string);
        Some(Tmux {
            session: it.next()?,
            window: it.next()?,
            pane: it.next()?,
            pane_id,
        })
    }

    fn branch(&self, cwd: &str) -> String {
        output("git", &["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default()
    }

    fn claude_pid(&self) -> i64 {
        // Claude runs hooks through a shell (and the plugin through a wrapper script), so skip
        // shells that did not exec; at most three hops.
        let mut pid = std::os::unix::process::parent_id() as i64;
        for _ in 0..3 {
            let Some(line) = output("ps", &["-o", "ppid=,comm=", "-p", &pid.to_string()]) else {
                break;
            };
            let mut it = line.split_whitespace();
            let (Some(ppid), Some(comm)) = (it.next(), it.next()) else {
                break;
            };
            let name = comm
                .rsplit('/')
                .next()
                .unwrap_or(comm)
                .trim_start_matches('-');
            if !matches!(name, "sh" | "bash" | "zsh" | "dash") {
                break;
            }
            match ppid.parse() {
                Ok(p) if p > 1 => pid = p,
                _ => break,
            }
        }
        pid
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

/// Run one hook. `input` is the hook's stdin.
pub fn run(event: &str, input: &str, store: &Store, probe: &dyn Probe) -> Result<()> {
    let data: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let id = s(&data, "session_id");
    if !crate::store::valid_id(id) {
        bail!("{event}: no usable session_id");
    }
    match event {
        "session-start" => session_start(&data, id, store, probe),
        "session-end" => session_end(&data, id, store),
        "notification" => notification(&data, id, store),
        "stop" => touch(store, id, &data, |r, t| {
            r.insert("status".into(), "idle".into());
            r.insert("status_message".into(), "Waiting for input".into());
            r.insert("idle_since".into(), t.into());
        }),
        // A subagent finishing says nothing about the main session, which may still be working
        // or waiting on you, so this only records activity.
        "subagent-stop" => touch(store, id, &data, |_, _| {}),
        "user-prompt-submit" => {
            let summary = summarise(s(&data, "prompt"));
            touch(store, id, &data, |r, _| {
                r.insert("status".into(), "working".into());
                r.insert("status_message".into(), "".into());
                r.insert("last_prompt".into(), summary.clone().into());
                r.insert("idle_since".into(), Value::Null);
            })?;
            store.log(id, &format!("PROMPT: {summary}"))
        }
        "pre-tool-use" => {
            let tool = s(&data, "tool_name").to_string();
            touch(store, id, &data, |r, _| {
                let n = r.get("tool_calls").and_then(Value::as_u64).unwrap_or(0);
                r.insert("last_tool".into(), tool.into());
                r.insert("status".into(), "working".into());
                r.insert("idle_since".into(), Value::Null);
                r.insert("tool_calls".into(), (n + 1).into());
            })
        }
        other => bail!(
            "unknown hook event {other:?} (expected one of {})",
            EVENTS.join(", ")
        ),
    }
}

/// Update an existing live record and stamp `last_activity`; no record means not ours to track.
fn touch(store: &Store, id: &str, data: &Value, f: impl FnOnce(&mut Record, &str)) -> Result<()> {
    let t = now();
    let tp = s(data, "transcript_path").to_string();
    store.update(Kind::Live, id, |r| {
        f(r, &t);
        r.insert("last_activity".into(), t.clone().into());
        if !tp.is_empty() {
            r.insert("transcript_path".into(), tp.into());
        }
    })?;
    Ok(())
}

fn summarise(prompt: &str) -> String {
    let one = prompt.replace('\n', " ");
    if one.chars().count() > 120 {
        one.chars().take(120).collect::<String>() + "…"
    } else {
        one
    }
}

/// Map a notification to a status. Claude Code sends top-level `notification_type`
/// (`permission_prompt`, `idle_prompt`, …); ccmon read a nested object, which is still accepted.
fn notification_status(kind: &str, message: &str) -> Option<&'static str> {
    let k: String = kind
        .chars()
        .filter(|c| *c != '_')
        .collect::<String>()
        .to_lowercase();
    match k.as_str() {
        "permissionprompt" => Some("needs_permission"),
        "idleprompt" | "elicitationdialog" => Some("needs_input"),
        "authsuccess" => Some("working"),
        _ => {
            const ASKS: &[&str] = &[
                "how should",
                "what would you like",
                "which option",
                "do you want",
                "should i",
                "should we",
                "would you like",
                "please choose",
                "please select",
            ];
            let m = message.to_lowercase();
            ASKS.iter().any(|a| m.contains(a)).then_some("needs_input")
        }
    }
}

fn notification(data: &Value, id: &str, store: &Store) -> Result<()> {
    let pick = |k: &str| match s(data, k) {
        "" => data["notification"][if k == "notification_type" { "type" } else { k }]
            .as_str()
            .unwrap_or("")
            .to_string(),
        v => v.to_string(),
    };
    let (kind, message) = (pick("notification_type"), pick("message"));
    let Some(status) = notification_status(&kind, &message) else {
        return store.log(
            id,
            &format!("NOTIFICATION (ignored) type='{kind}' message='{message}'"),
        );
    };
    touch(store, id, data, |r, t| {
        r.insert("status".into(), status.into());
        let msg = if message.is_empty() { &kind } else { &message };
        r.insert("status_message".into(), msg.clone().into());
        let idle = if status == "working" {
            Value::Null
        } else {
            t.into()
        };
        r.insert("idle_since".into(), idle);
    })?;
    store.log(id, &format!("NOTIFICATION {kind}: {message}"))
}

fn session_start(data: &Value, id: &str, store: &Store, probe: &dyn Probe) -> Result<()> {
    let cwd = match s(data, "cwd") {
        "" => std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        c => c.to_string(),
    };
    let tmux = probe.tmux();
    let target = tmux.as_ref().map(Tmux::target).unwrap_or_default();
    let branch = probe.branch(&cwd);
    let pid = probe.claude_pid();
    let t = now();
    store.locked(|| -> Result<()> {
        retire_seat(store, id, &target, pid, &t);
        let mut rec = match store.read(Kind::Live, id) {
            Some(r) => r,
            None => {
                // A session reopened after a save+close gets its description back.
                let desc = store
                    .read(Kind::Closed, id)
                    .and_then(|c| c.get("description").cloned())
                    .unwrap_or_else(|| "".into());
                store.remove(Kind::Closed, id);
                let Value::Object(m) = json!({
                    "session_id": id, "last_prompt": "", "last_tool": "", "description": desc,
                    "session_start": t, "tool_calls": 0,
                }) else {
                    unreachable!()
                };
                m
            }
        };
        let tm = tmux.unwrap_or_default();
        for (k, v) in [
            ("claude_pid", json!(pid)),
            ("tmux_target", json!(target)),
            ("tmux_session", json!(tm.session)),
            ("tmux_window", json!(tm.window)),
            ("tmux_pane", json!(tm.pane)),
            ("tmux_pane_id", json!(tm.pane_id)),
            ("worktree", json!(cwd)),
            ("branch", json!(branch)),
            ("status", json!("idle")),
            ("status_message", json!("")),
            ("last_activity", json!(t)),
            ("idle_since", json!(t)),
        ] {
            rec.insert(k.into(), v);
        }
        if let tp @ Value::String(_) = &data["transcript_path"] {
            rec.insert("transcript_path".into(), tp.clone());
        }
        store.write(Kind::Live, id, &rec)
    })?
}

/// Retire older records in the seat this session now occupies: the same tmux pane (only when
/// there is one, or every non-tmux session would match), or the same Claude process.
fn retire_seat(store: &Store, id: &str, target: &str, pid: i64, t: &str) {
    for old in store.live_ids().into_iter().filter(|o| o != id) {
        let Some(mut rec) = store.read(Kind::Live, &old) else {
            continue;
        };
        let old_pid = rec.get("claude_pid").and_then(Value::as_i64);
        let same_pane =
            !target.is_empty() && rec.get("tmux_target").and_then(Value::as_str) == Some(target);
        if old_pid == Some(pid) {
            // Same process, new id: `/clear` or a fork. The old record is simply gone.
            store.remove(Kind::Live, &old);
            let _ = store.log(&old, &format!("Session cleared - new session_id: {id}"));
        } else if same_pane {
            rec.insert("status".into(), "dead".into());
            rec.insert(
                "status_message".into(),
                format!("Replaced by new session {}", &id[..id.len().min(8)]).into(),
            );
            rec.insert("last_activity".into(), t.into());
            let _ = store.write(Kind::Live, &old, &rec);
            let _ = store.log(
                &old,
                &format!(
                    "Session replaced by {id} (PID changed: {} -> {pid})",
                    old_pid.unwrap_or(0)
                ),
            );
        }
    }
}

/// Unregister the session. A named one is archived to `closed/` so it can be reopened, except
/// on `/clear`, which retires the id in place.
fn session_end(data: &Value, id: &str, store: &Store) -> Result<()> {
    store.locked(|| -> Result<()> {
        let Some(rec) = store.read(Kind::Live, id) else {
            return Ok(());
        };
        let g = |k: &str| {
            rec.get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        };
        let desc = g("description");
        let reason = match s(data, "reason") {
            "" => "other",
            r => r,
        };
        if reason != "clear" && !desc.is_empty() {
            let Value::Object(closed) = json!({
                "session_id": id, "description": desc, "worktree": g("worktree"),
                "branch": g("branch"), "tmux_session": g("tmux_session"), "closed_at": now(),
            }) else {
                unreachable!()
            };
            // Losing the closed record beats leaving a dead session registered.
            if let Err(e) = store.write(Kind::Closed, id, &closed) {
                crate::log::warn(format!("hook session-end: archive {id}: {e}"));
            }
        }
        store.remove(Kind::Live, id);
        Ok(())
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{put, scratch};

    struct Fake(Option<Tmux>, i64);
    impl Probe for Fake {
        fn tmux(&self) -> Option<Tmux> {
            self.0.clone()
        }
        fn branch(&self, _: &str) -> String {
            "main".into()
        }
        fn claude_pid(&self) -> i64 {
            self.1
        }
    }

    fn pane(p: &str) -> Option<Tmux> {
        Some(Tmux {
            session: "w".into(),
            window: "1".into(),
            pane: p.into(),
            pane_id: format!("%{p}"),
        })
    }

    fn fire(st: &Store, ev: &str, v: Value) {
        run(ev, &v.to_string(), st, &Fake(pane("0"), 100)).unwrap();
    }

    fn get(st: &Store, id: &str) -> Record {
        st.read(Kind::Live, id).unwrap()
    }

    #[test]
    fn lifecycle() {
        let t = scratch("hook-life");
        let st = &t.1;
        fire(
            st,
            "session-start",
            json!({"session_id":"s-1","cwd":"/w","transcript_path":"/t.jsonl"}),
        );
        let r = get(st, "s-1");
        assert_eq!(r["status"], "idle");
        assert_eq!(r["tmux_target"], "w:1.0");
        assert_eq!(r["tmux_pane_id"], "%0");
        assert_eq!(r["claude_pid"], 100);
        assert_eq!(r["transcript_path"], "/t.jsonl");

        fire(
            st,
            "user-prompt-submit",
            json!({"session_id":"s-1","prompt":"a\nb"}),
        );
        assert_eq!(get(st, "s-1")["last_prompt"], "a b");
        fire(
            st,
            "pre-tool-use",
            json!({"session_id":"s-1","tool_name":"Bash"}),
        );
        fire(
            st,
            "pre-tool-use",
            json!({"session_id":"s-1","tool_name":"Read"}),
        );
        let r = get(st, "s-1");
        assert_eq!(
            (r["tool_calls"].as_u64(), r["last_tool"].as_str()),
            (Some(2), Some("Read"))
        );
        assert_eq!(r["status"], "working");

        fire(
            st,
            "notification",
            json!({"session_id":"s-1","notification_type":"permission_prompt","message":"Claude needs permission"}),
        );
        assert_eq!(get(st, "s-1")["status"], "needs_permission");
        fire(st, "subagent-stop", json!({"session_id":"s-1"}));
        assert_eq!(get(st, "s-1")["status"], "needs_permission");
        fire(st, "stop", json!({"session_id":"s-1"}));
        assert_eq!(get(st, "s-1")["status"], "idle");

        let log = std::fs::read_to_string(st.log_path("s-1")).unwrap();
        assert!(log.contains("PROMPT: a b") && log.contains("NOTIFICATION permission_prompt"));

        fire(
            st,
            "session-end",
            json!({"session_id":"s-1","reason":"other"}),
        );
        assert!(st.read(Kind::Live, "s-1").is_none());
        assert!(
            st.read(Kind::Closed, "s-1").is_none(),
            "unnamed sessions are not archived"
        );
    }

    #[test]
    fn named_session_closes_and_reopens() {
        let t = scratch("hook-named");
        let st = &t.1;
        fire(st, "session-start", json!({"session_id":"s-2","cwd":"/w"}));
        st.update(Kind::Live, "s-2", |r| {
            r.insert("description".into(), "keep me".into());
        })
        .unwrap();
        fire(
            st,
            "session-end",
            json!({"session_id":"s-2","reason":"clear"}),
        );
        assert!(
            st.read(Kind::Closed, "s-2").is_none(),
            "/clear does not archive"
        );

        fire(st, "session-start", json!({"session_id":"s-2","cwd":"/w"}));
        st.update(Kind::Live, "s-2", |r| {
            r.insert("description".into(), "keep me".into());
        })
        .unwrap();
        fire(
            st,
            "session-end",
            json!({"session_id":"s-2","reason":"logout"}),
        );
        assert_eq!(
            st.read(Kind::Closed, "s-2").unwrap()["description"],
            "keep me"
        );
        fire(st, "session-start", json!({"session_id":"s-2","cwd":"/w"}));
        assert_eq!(get(st, "s-2")["description"], "keep me");
        assert!(st.read(Kind::Closed, "s-2").is_none());
    }

    #[test]
    fn seat_retirement() {
        let t = scratch("hook-seat");
        let st = &t.1;
        let legacy = st.legacy.clone().unwrap();
        // Same process (`/clear`): removed, even from ccmon's dir.
        put(
            &legacy,
            Kind::Live,
            "old-1",
            json!({"session_id":"old-1","claude_pid":100,"tmux_target":"x:9.9"}),
        );
        // Same pane, other process (restart): marked dead.
        put(
            &st.root,
            Kind::Live,
            "old-2",
            json!({"session_id":"old-2","claude_pid":7,"tmux_target":"w:1.0"}),
        );
        // Unrelated non-tmux session: untouched.
        put(
            &st.root,
            Kind::Live,
            "old-3",
            json!({"session_id":"old-3","claude_pid":8,"tmux_target":""}),
        );
        fire(
            st,
            "session-start",
            json!({"session_id":"new-1","cwd":"/w"}),
        );
        assert!(st.read(Kind::Live, "old-1").is_none());
        assert_eq!(get(st, "old-2")["status"], "dead");
        assert!(get(st, "old-3").get("status").is_none());

        // Outside tmux an empty target matches nothing.
        run(
            "session-start",
            &json!({"session_id":"new-2"}).to_string(),
            st,
            &Fake(None, 55),
        )
        .unwrap();
        assert!(get(st, "old-3").get("status").is_none());
    }

    #[test]
    fn legacy_record_is_updated_and_moved() {
        let t = scratch("hook-legacy");
        let st = &t.1;
        let legacy = st.legacy.clone().unwrap();
        put(
            &legacy,
            Kind::Live,
            "l-1",
            json!({"session_id":"l-1","tool_calls":4,"description":"d"}),
        );
        fire(
            st,
            "pre-tool-use",
            json!({"session_id":"l-1","tool_name":"Edit","transcript_path":"/p"}),
        );
        let r = st.read(Kind::Live, "l-1").unwrap();
        assert_eq!(
            (r["tool_calls"].as_u64(), r["description"].as_str()),
            (Some(5), Some("d"))
        );
        assert_eq!(r["transcript_path"], "/p");
        assert!(st.root.join("sessions/l-1.json").exists());
        assert!(!legacy.join("sessions/l-1.json").exists());
    }

    #[test]
    fn no_record_and_bad_input() {
        let t = scratch("hook-none");
        let st = &t.1;
        fire(st, "stop", json!({"session_id":"ghost"}));
        assert!(
            st.read(Kind::Live, "ghost").is_none(),
            "updates never create records"
        );
        assert!(run("stop", "not json", st, &Fake(None, 1)).is_err());
        assert!(run("stop", r#"{"session_id":"../etc"}"#, st, &Fake(None, 1)).is_err());
        assert!(run("bogus", r#"{"session_id":"a"}"#, st, &Fake(None, 1)).is_err());
    }

    #[test]
    fn notification_mapping() {
        assert_eq!(notification_status("idle_prompt", ""), Some("needs_input"));
        assert_eq!(
            notification_status("permissionprompt", ""),
            Some("needs_permission")
        );
        assert_eq!(notification_status("auth_success", ""), Some("working"));
        assert_eq!(
            notification_status("", "Which option do you prefer?"),
            Some("needs_input")
        );
        assert_eq!(notification_status("", "Done."), None);
        // ccmon's nested shape still works.
        let t = scratch("hook-nested");
        fire(&t.1, "session-start", json!({"session_id":"n-1"}));
        fire(
            &t.1,
            "notification",
            json!({"session_id":"n-1","notification":{"type":"idleprompt","message":"m"}}),
        );
        assert_eq!(get(&t.1, "n-1")["status"], "needs_input");
        assert_eq!(get(&t.1, "n-1")["status_message"], "m");
    }
}
