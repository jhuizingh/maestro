//! Per-context adapter: ticket key/url/info and progress files. The core knows no ticket system.
use crate::config::{Priority, ProgressCfg, TicketCfg};
use crate::log;
use crate::model::{Progress, TicketError, TicketErrorKind, TicketInfo};
use crate::util::{TimedOut, expand_tilde, run_cmd};
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
pub fn run_info(workspace: &Path, cfg: &TicketCfg, key: &str) -> Result<TicketInfo, TicketError> {
    let Some((prog, rest)) = cfg.info.split_first() else {
        return Err(TicketError::new(TicketErrorKind::Error, "no info command"));
    };
    let args: Vec<String> = rest.iter().map(|a| fill(a, key)).collect();
    let prog = fill(prog, key);
    let timeout = cfg.info_timeout();
    let budget = timeout.as_secs().to_string();
    let env = [("FERMATA_INFO_TIMEOUT", budget.as_str())];
    let out = match run_cmd(&prog, &args, Some(workspace), &env, None, timeout) {
        Ok(o) => o,
        Err(e) => {
            log::warn(format!("adapter info {key}: {e}"));
            return Err(match e.downcast_ref::<TimedOut>() {
                Some(t) => TicketError {
                    kind: TicketErrorKind::Timeout,
                    message: None,
                    timeout_secs: Some(t.secs),
                },
                None => TicketError::new(TicketErrorKind::Error, e.to_string()),
            });
        }
    };
    let stderr = out.stderr.trim();
    let r = parse_info(&out.stdout, out.ok(), stderr);
    // One line per call: the outcome plus the adapter's own stderr, which carries the real cause.
    match (&r, stderr.is_empty()) {
        (Err(e), _) => log::warn(format!(
            "adapter info {key}: {e} (exit {:?}){}",
            out.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(" stderr: {stderr}")
            }
        )),
        (Ok(_), false) => log::warn(format!("adapter info {key} stderr: {stderr}")),
        (Ok(_), true) => {}
    }
    r
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

/// Info output cached per key for the adapter's `ttl`.
type Cached<T> = (Instant, Result<T, TicketError>);

#[derive(Default)]
pub struct TicketCache {
    map: Mutex<HashMap<String, Cached<TicketInfo>>>,
    pub disk: Option<crate::cache::DiskCache>,
}

impl TicketCache {
    pub fn get(
        &self,
        workspace: &Path,
        cfg: &TicketCfg,
        key: &str,
        force: bool,
    ) -> Result<TicketInfo, TicketError> {
        let ttl = Duration::from_secs(cfg.ttl);
        if !force
            && let Some((t, v)) = self.map.lock().unwrap().get(key)
            && t.elapsed() < ttl
        {
            return v.clone();
        }
        let dname = format!("ticket-{key}");
        let dkey = format!("{}|{key}", workspace.display());
        if !force
            && let Some(d) = &self.disk
            && let Some(info) = d.get::<TicketInfo>(&dname, &dkey, Some(cfg.ttl))
        {
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), (Instant::now(), Ok(info.clone())));
            return Ok(info);
        }
        let v = run_info(workspace, cfg, key);
        if let (Some(d), Ok(info)) = (&self.disk, &v) {
            d.put(&dname, &dkey, info);
        }
        self.map
            .lock()
            .unwrap()
            .insert(key.to_string(), (Instant::now(), v.clone()));
        v
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
        let cache = TicketCache::default();
        let r = cache.get(Path::new("/"), &c, "X-1", false);
        assert_eq!(r.unwrap_err().kind, TicketErrorKind::Error);
        c.info = vec!["echo".into(), "{\"key\":\"{key}\",\"title\":\"T\"}".into()];
        let r = cache.get(Path::new("/"), &c, "X-1", true).unwrap();
        assert_eq!(r.title.as_deref(), Some("T"));
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
}
