//! Per-context adapter: ticket key/url/info and progress files. The core knows no ticket system.
use crate::config::{Priority, ProgressCfg, TicketCfg};
use crate::fetch::{FailClass, Failure, Fetch, KeyedCache, Lookup, Policy, Req};
use crate::model::{Progress, TicketError, TicketErrorKind, TicketInfo};
use crate::util::{TimedOut, expand_tilde, run_cmd};
use regex::Regex;
use std::path::Path;
use std::time::Duration;

pub fn ticket_key(cfg: &TicketCfg, labels: &[String]) -> Option<String> {
    if cfg.key_from != "label" {
        return None;
    }
    let re = Regex::new(&cfg.pattern).ok()?;
    labels.iter().find_map(|l| {
        let c = re.captures(l)?;
        Some(c.get(1).unwrap_or(c.get(0)?).as_str().to_string())
    })
}

pub fn fill(template: &str, key: &str) -> String {
    template.replace("{key}", key)
}

pub fn ticket_url(cfg: &TicketCfg, key: &str) -> Option<String> {
    cfg.url.as_deref().map(|u| fill(u, key))
}

/// Run the adapter's `info` command from the workspace dir and parse TicketInfo, or say why not.
#[cfg(test)]
pub fn run_info(workspace: &Path, cfg: &TicketCfg, key: &str) -> Result<TicketInfo, TicketError> {
    exec_info(workspace, cfg, key).0
}

/// As `run_info`, plus detail for the log line: the exit status and a stderr summary.
fn exec_info(
    workspace: &Path,
    cfg: &TicketCfg,
    key: &str,
) -> (Result<TicketInfo, TicketError>, String) {
    let Some((prog, rest)) = cfg.info.split_first() else {
        return (
            Err(TicketError::new(TicketErrorKind::Error, "no info command")),
            String::new(),
        );
    };
    let args: Vec<String> = rest.iter().map(|a| fill(a, key)).collect();
    let prog = fill(prog, key);
    let timeout = cfg.info_timeout();
    let budget = timeout.as_secs().to_string();
    let env = [("FERMATA_INFO_TIMEOUT", budget.as_str())];
    let out = match run_cmd(&prog, &args, Some(workspace), &env, None, timeout) {
        Ok(o) => o,
        Err(e) => {
            let err = match e.downcast_ref::<TimedOut>() {
                Some(t) => TicketError {
                    kind: TicketErrorKind::Timeout,
                    message: None,
                    timeout_secs: Some(t.secs),
                },
                None => TicketError::new(TicketErrorKind::Error, e.to_string()),
            };
            return (Err(err), String::new());
        }
    };
    let stderr = out.stderr.trim();
    let r = parse_info(&out.stdout, out.ok(), stderr);
    let mut detail = match (&r, out.status) {
        (Err(_), Some(c)) => format!(" (exit {c})"),
        (Err(_), None) => " (killed by a signal)".into(),
        (Ok(_), _) => String::new(),
    };
    if !stderr.is_empty() {
        detail.push_str(&format!(" stderr: {}", stderr_summary(stderr)));
    }
    (r, detail)
}

/// The first non-empty stderr line, truncated, with a count of the rest.
fn stderr_summary(stderr: &str) -> String {
    let mut lines = stderr.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = crate::util::truncate(lines.next().unwrap_or(""), 200);
    match lines.count() {
        0 => first,
        n => format!("{first} (+{n} more line{})", if n == 1 { "" } else { "s" }),
    }
}

/// Read `info` output: an error object wins, then the exit status, then TicketInfo.
pub fn parse_info(stdout: &str, exit_ok: bool, stderr: &str) -> Result<TicketInfo, TicketError> {
    let v: Option<serde_json::Value> = serde_json::from_str(stdout.trim()).ok();
    if let Some(e) = v
        .as_ref()
        .and_then(|v| v.get("error"))
        .filter(|e| e.is_object())
    {
        let kind = TicketErrorKind::from_adapter(e.get("kind").and_then(|k| k.as_str()));
        let msg = e.get("message").and_then(|m| m.as_str()).unwrap_or("");
        return Err(TicketError::new(kind, msg));
    }
    if !exit_ok {
        return Err(TicketError::new(TicketErrorKind::Error, stderr));
    }
    let Some(v) = v else {
        return Err(TicketError::new(TicketErrorKind::Error, "invalid JSON"));
    };
    serde_json::from_value(v)
        .map_err(|e| TicketError::new(TicketErrorKind::Error, format!("invalid TicketInfo: {e}")))
}

impl Failure for TicketError {
    fn class(&self) -> FailClass {
        match self.kind {
            TicketErrorKind::NotFound => FailClass::NotFound,
            _ => FailClass::Transient,
        }
    }
}

/// Info output per workspace and key, by the rules in `fetch.rs`, for the adapter's `ttl`.
pub struct TicketCache {
    pub cache: KeyedCache<TicketInfo, TicketError>,
    pub health: Option<std::sync::Arc<crate::health::Health>>,
}

impl Default for TicketCache {
    fn default() -> Self {
        Self {
            cache: KeyedCache::new("ticket info", "ticket2"),
            health: None,
        }
    }
}

impl TicketCache {
    /// `ctx` names the context, for health.
    pub fn get(
        &self,
        ctx: &str,
        workspace: &Path,
        cfg: &TicketCfg,
        key: &str,
        fetch: impl Into<Fetch>,
    ) -> Lookup<TicketInfo, TicketError> {
        let scope = workspace.display().to_string();
        let label = workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let req = Req {
            scope: &scope,
            label: &label,
            key,
            policy: Policy::from_ttl(Duration::from_secs(cfg.ttl)),
            fetch: fetch.into(),
        };
        self.cache.get(&req, || {
            let out = exec_info(workspace, cfg, key);
            if let Some(h) = &self.health {
                use crate::health::Source;
                match &out.0 {
                    // A not-found ticket is an answer: the source works.
                    Ok(_) => h.ok(ctx, Source::Tickets, key),
                    Err(e) if e.kind == TicketErrorKind::NotFound => {
                        h.ok(ctx, Source::Tickets, key)
                    }
                    Err(e) => {
                        let kind = serde_json::to_value(e.kind).ok();
                        let kind = kind.as_ref().and_then(|k| k.as_str()).unwrap_or("error");
                        h.fail(ctx, Source::Tickets, key, kind, &e.to_string());
                    }
                }
            }
            out
        })
    }
}

/// Count `- [x]` vs `- [ ]` lines; keep done items and the next few open ones.
pub fn parse_checklist(text: &str) -> (usize, usize, Vec<String>, Vec<String>) {
    let (mut done, mut total) = (0, 0);
    let (mut dl, mut nl) = (vec![], vec![]);
    for l in text.lines() {
        let t = l.trim_start();
        let body = |p: &str| t[p.len()..].trim().to_string();
        if t.starts_with("- [x]") || t.starts_with("- [X]") {
            done += 1;
            total += 1;
            dl.push(body("- [x]"));
        } else if t.starts_with("- [ ]") {
            total += 1;
            nl.push(body("- [ ]"));
        }
    }
    (done, total, dl, nl)
}

pub fn read_progress(cfg: Option<&ProgressCfg>, key: Option<&str>) -> Progress {
    let mut p = Progress::default();
    let (Some(cfg), Some(key)) = (cfg, key) else {
        return p;
    };
    if let Some(sf) = &cfg.status
        && let Ok(t) = std::fs::read_to_string(expand_tilde(&fill(sf, key)))
    {
        p.status_line = t
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string);
    }
    if let Some(cf) = &cfg.checklist
        && let Ok(t) = std::fs::read_to_string(expand_tilde(&fill(cf, key)))
    {
        let (d, n, dl, nl) = parse_checklist(&t);
        if n > 0 {
            p.checklist = Some((d, n));
        }
        p.done = dl.into_iter().rev().take(5).collect();
        p.next = nl.into_iter().take(5).collect();
    }
    p
}

/// Pin rank for a task: index of the first priority matching its ticket or epic.
pub fn pin_rank(prios: &[Priority], ticket: Option<&str>, epic: Option<&str>) -> Option<usize> {
    prios.iter().position(|p| {
        (p.ticket.is_some() && p.ticket.as_deref() == ticket)
            || (p.epic.is_some() && (p.epic.as_deref() == epic || p.epic.as_deref() == ticket))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    fn cfg() -> TicketCfg {
        TicketCfg {
            key_from: "label".into(),
            pattern: "^jira-([A-Z]+-[0-9]+)$".into(),
            url: Some("https://j/browse/{key}".into()),
            info: vec![],
            ttl: 300,
            timeout: None,
        }
    }

    #[test]
    fn key_from_label() {
        let l = vec!["repo-x".into(), "jira-ABC-12".into()];
        assert_eq!(ticket_key(&cfg(), &l).as_deref(), Some("ABC-12"));
        assert_eq!(ticket_key(&cfg(), &["x".into()]), None);
        assert_eq!(ticket_url(&cfg(), "A-1").unwrap(), "https://j/browse/A-1");
    }

    #[test]
    fn checklist() {
        let (d, n, dl, nl) = parse_checklist("- [x] a\n  - [ ] b\n- [X] c\ntext\n- [ ] d");
        assert_eq!((d, n), (2, 4));
        assert_eq!(dl, vec!["a", "c"]);
        assert_eq!(nl, vec!["b", "d"]);
    }

    #[test]
    fn pins() {
        let p = vec![
            Priority {
                epic: Some("E-1".into()),
                ticket: None,
            },
            Priority {
                ticket: Some("T-2".into()),
                epic: None,
            },
        ];
        assert_eq!(pin_rank(&p, Some("T-9"), Some("E-1")), Some(0));
        assert_eq!(pin_rank(&p, Some("T-2"), None), Some(1));
        assert_eq!(pin_rank(&p, Some("E-1"), None), Some(0));
        assert_eq!(pin_rank(&p, Some("T-3"), None), None);
    }

    #[test]
    fn info_errors_are_graceful() {
        let mut c = cfg();
        c.info = vec!["false".into()];
        let h = std::sync::Arc::new(crate::health::Health::default());
        let cache = TicketCache {
            health: Some(h.clone()),
            ..Default::default()
        };
        let r = cache.get("w", Path::new("/"), &c, "X-1", false);
        assert_eq!(r.error.unwrap().kind, TicketErrorKind::Error);
        let row = h.rows().remove(0);
        assert!(row.degraded);
        assert_eq!(row.health.last_error.unwrap().kind, "error");
        c.info = vec!["echo".into(), "{\"key\":\"{key}\",\"title\":\"T\"}".into()];
        let r = cache
            .get("w", Path::new("/"), &c, "X-1", true)
            .value
            .unwrap();
        assert_eq!(r.title.as_deref(), Some("T"));
        assert!(!h.rows()[0].degraded);
        c.info = vec!["echo".into(), "not json".into()];
        assert!(run_info(Path::new("/"), &c, "X-1").is_err());
    }

    fn sh(script: &str) -> TicketCfg {
        let mut c = cfg();
        c.info = vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            "sh".into(),
            "{key}".into(),
        ];
        c
    }

    #[test]
    fn error_object_kinds() {
        let k = |out: &str| {
            let c = sh(&format!("echo '{out}'; exit 1"));
            run_info(Path::new("/"), &c, "X-1").unwrap_err()
        };
        let e = k(r#"{"error":{"kind":"not_found","message":"no X-1"}}"#);
        assert_eq!(
            (e.kind, e.to_string().as_str()),
            (TicketErrorKind::NotFound, "ticket not found: no X-1")
        );
        let e = k(r#"{"error":{"kind":"unavailable"}}"#);
        assert_eq!(
            (e.kind, e.label().as_str()),
            (TicketErrorKind::Unavailable, "ticket source unavailable")
        );
        assert_eq!(
            k(r#"{"error":{"kind":"bogus"}}"#).kind,
            TicketErrorKind::Error
        );
        assert_eq!(k(r#"{"error":{}}"#).kind, TicketErrorKind::Error);
        // An error object wins even with exit 0.
        let c = sh(r#"echo '{"error":{"kind":"not_found"}}'"#);
        assert_eq!(
            run_info(Path::new("/"), &c, "X-1").unwrap_err().kind,
            TicketErrorKind::NotFound
        );
    }

    #[test]
    fn plain_failures_stay_backward_compatible() {
        // Exit 2 alone is not "not found": a shell exits 2 on a syntax error.
        let c = sh("echo 'jira: connection reset' >&2; echo 'second line' >&2; exit 2");
        let e = run_info(Path::new("/"), &c, "X-1").unwrap_err();
        assert_eq!(e.kind, TicketErrorKind::Error);
        assert_eq!(e.to_string(), "adapter error: jira: connection reset");
        // A TicketInfo that merely has a non-object `error` field is still a TicketInfo.
        let c = sh(r#"echo '{"key":"X-1","error":"ignored"}'"#);
        assert_eq!(run_info(Path::new("/"), &c, "X-1").unwrap().key, "X-1");
    }

    #[test]
    fn timeout_and_budget() {
        let mut c = sh(r#"echo "{\"key\":\"$1\",\"title\":\"$FERMATA_INFO_TIMEOUT\"}""#);
        c.timeout = Some(3);
        assert_eq!(
            run_info(Path::new("/"), &c, "X-1")
                .unwrap()
                .title
                .as_deref(),
            Some("3")
        );
        let mut c = sh("exec sleep 5");
        c.timeout = Some(1);
        let e = run_info(Path::new("/"), &c, "X-1").unwrap_err();
        assert_eq!(e.kind, TicketErrorKind::Timeout);
        assert_eq!(e.to_string(), "adapter timed out after 1s");
    }

    /// The shipped example adapter, end to end through `run_info`, against a fake `gh`.
    #[test]
    fn example_adapter_contract() {
        let root = std::env::temp_dir().join(format!("fermata-adapter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let gh = bin.join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\ncase \"$FAKE_GH\" in\n\
             ok) echo '{\"key\":\"o/r#1\",\"title\":\"T\"}' ;;\n\
             missing) echo 'GraphQL: Could not resolve to an issue' >&2; exit 1 ;;\n\
             offline) echo 'error connecting to api.github.com' >&2; exit 1 ;;\n\
             hang) exec sleep 30 ;;\n\
             esac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/ticket-info.sh");
        let path = format!("PATH={}:/usr/bin:/bin", bin.display());
        let notes = root.join("notes");
        let run = |mode: &str, timeout: u64| {
            let mut c = cfg();
            c.timeout = Some(timeout);
            c.info = vec![
                "env".into(),
                path.clone(),
                format!("FAKE_GH={mode}"),
                format!("TICKET_INFO_DIR={}", notes.display()),
                "sh".into(),
                script.into(),
                "{key}".into(),
            ];
            run_info(&root, &c, "o/r#1")
        };
        assert_eq!(run("ok", 5).unwrap().title.as_deref(), Some("T"));
        assert_eq!(
            run("missing", 5).unwrap_err().kind,
            TicketErrorKind::NotFound
        );
        let e = run("offline", 5).unwrap_err();
        assert_eq!(e.kind, TicketErrorKind::Unavailable);
        assert!(e.to_string().contains("error connecting"), "{e}");
        // The adapter bounds itself below fermata's budget, so its own reason wins the race.
        let t = Instant::now();
        let e = run("hang", 3).unwrap_err();
        assert_eq!(e.kind, TicketErrorKind::Unavailable, "{e}");
        assert!(t.elapsed() < Duration::from_secs(3));
        // A notes directory without the file is "no data", not a failure of its own.
        std::fs::create_dir_all(notes.join("o/r")).unwrap();
        assert_eq!(
            run("offline", 5).unwrap_err().kind,
            TicketErrorKind::Unavailable
        );
        std::fs::write(
            notes.join("o/r/1.json"),
            r#"{"key":"o/r#1","title":"cached"}"#,
        )
        .unwrap();
        assert_eq!(run("offline", 5).unwrap().title.as_deref(), Some("cached"));
        // The fallback never masks a source that said the ticket does not exist.
        assert_eq!(
            run("missing", 5).unwrap_err().kind,
            TicketErrorKind::NotFound
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The acceptance example: 30 tickets, the source down for 10 minutes. Panels keep the last
    /// known values marked stale, the log gets a handful of lines, and recovery is quiet.
    #[test]
    fn outage_serves_stale_and_logs_little() {
        let root = std::env::temp_dir().join(format!("fermata-outage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let down = root.join("down");
        let script = root.join("info.sh");
        std::fs::write(
            &script,
            "if [ -e \"$DOWN\" ]; then\n\
             echo '{\"error\":{\"kind\":\"unavailable\",\"message\":\"source down\"}}'\n\
             echo 'connect: refused' >&2; exit 1\nfi\n\
             echo \"{\\\"key\\\":\\\"$1\\\",\\\"title\\\":\\\"T\\\"}\"\n",
        )
        .unwrap();
        let mut c = cfg();
        c.info = vec![
            "env".into(),
            format!("DOWN={}", down.display()),
            "sh".into(),
            script.display().to_string(),
            "{key}".into(),
        ];
        let cache = TicketCache::default();
        let keys: Vec<String> = (1..=30).map(|i| format!("X-{i}")).collect();
        let pass = |force: bool| {
            std::thread::scope(|sc| {
                let hs: Vec<_> = keys
                    .iter()
                    .map(|k| sc.spawn(|| cache.get("w", &root, &c, k, force)))
                    .collect();
                hs.into_iter()
                    .map(|h| h.join().unwrap())
                    .collect::<Vec<_>>()
            })
        };
        assert!(
            pass(false)
                .iter()
                .all(|r| r.value.is_some() && r.stale.is_none())
        );
        std::fs::write(&down, "").unwrap();
        // The source fails just as the entries expire; then ten minutes of refreshes, a minute
        // apart.
        for i in 0..10 {
            let ms = if i == 0 {
                c.ttl as i64 * 1000 + 1
            } else {
                60_000
            };
            for k in &keys {
                cache.cache.age(&root.display().to_string(), k, ms);
            }
            for r in pass(false) {
                assert_eq!(r.value.unwrap().title.as_deref(), Some("T"));
                let st = r.stale.expect("stale while down");
                assert!(st.error.contains("source down"), "{}", st.error);
            }
        }
        let logged = cache.cache.logged.load(Ordering::Relaxed);
        assert!(logged <= 3, "{logged} log lines for the outage");
        // Back up: the next expiry refreshes everything; one recovery line at most.
        std::fs::remove_file(&down).unwrap();
        for k in &keys {
            cache.cache.age(&root.display().to_string(), k, 10 * 60_000);
        }
        assert!(
            pass(false)
                .iter()
                .all(|r| r.value.is_some() && r.stale.is_none())
        );
        assert!(cache.cache.logged.load(Ordering::Relaxed) <= logged + 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
