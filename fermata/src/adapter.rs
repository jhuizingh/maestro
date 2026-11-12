//! Per-context adapter: ticket key/url/info and progress files. The core knows no ticket system.
use crate::config::{Priority, ProgressCfg, TicketCfg};
use crate::log;
use crate::model::{Progress, TicketInfo};
use crate::util::{expand_tilde, run_cmd};
use anyhow::{Result, anyhow, bail};
use regex::Regex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const INFO_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Run the adapter's `info` command from the workspace dir and parse TicketInfo.
pub fn run_info(workspace: &Path, cfg: &TicketCfg, key: &str) -> Result<TicketInfo> {
    let Some((prog, rest)) = cfg.info.split_first() else {
        bail!("no info command");
    };
    let args: Vec<String> = rest.iter().map(|a| fill(a, key)).collect();
    let prog = fill(prog, key);
    let out = run_cmd(&prog, &args, Some(workspace), &[], None, INFO_TIMEOUT)?;
    if !out.ok() {
        log::warn(format!("adapter info {key}: {}", out.stderr.trim()));
        bail!("exit {:?}", out.status);
    }
    let info: TicketInfo =
        serde_json::from_str(out.stdout.trim()).map_err(|e| anyhow!("invalid JSON: {e}"))?;
    if !out.stderr.trim().is_empty() {
        log::warn(format!("adapter info {key} stderr: {}", out.stderr.trim()));
    }
    Ok(info)
}

/// Info output cached per key for the adapter's `ttl`.
type Cached<T> = (Instant, Result<T, String>);

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
    ) -> Result<TicketInfo, String> {
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
        let v = run_info(workspace, cfg, key).map_err(|e| {
            log::warn(format!("adapter error for {key}: {e}"));
            format!("adapter error: {e}")
        });
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
        assert!(r.unwrap_err().starts_with("adapter error"));
        c.info = vec!["echo".into(), "{\"key\":\"{key}\",\"title\":\"T\"}".into()];
        let r = cache.get(Path::new("/"), &c, "X-1", true).unwrap();
        assert_eq!(r.title.as_deref(), Some("T"));
        c.info = vec!["echo".into(), "not json".into()];
        assert!(run_info(Path::new("/"), &c, "X-1").is_err());
    }
}
