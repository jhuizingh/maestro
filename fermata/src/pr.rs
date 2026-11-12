//! PR state and checks via read-only `gh pr view`, cached 60s.
use crate::cache::DiskCache;
use crate::model::{Checks, PrInfo};
use crate::util::run_cmd;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
const FIELDS: &str = "number,state,isDraft,reviewDecision,statusCheckRollup,url";

/// Latest run per check (workflow + name, or status context) wins; superseded runs are
/// ignored. Failing = latest concluded FAILURE, TIMED_OUT or ACTION_REQUIRED.
pub fn parse_pr(v: &Value) -> PrInfo {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let mut latest_per: HashMap<String, (String, &Value)> = HashMap::new();
    for (i, c) in v
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let g = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
        let name = if g("__typename") == "StatusContext" {
            format!("ctx:{}", g("context"))
        } else {
            format!("run:{}/{}", g("workflowName"), g("name"))
        };
        let name = if name.ends_with(':') || name.ends_with("/") {
            format!("{name}#{i}")
        } else {
            name
        };
        let at = format!(
            "{}|{}",
            [g("startedAt"), g("createdAt")]
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or(""),
            g("completedAt")
        );
        match latest_per.get(&name) {
            Some((t, _)) if *t >= at => {}
            _ => {
                latest_per.insert(name, (at, c));
            }
        }
    }
    let mut runs: Vec<(String, &Value)> = latest_per.into_values().collect();
    runs.sort_by(|a, b| a.0.cmp(&b.0));
    let mut checks = Checks::default();
    for (_, c) in &runs {
        let g = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("");
        let url = [g("detailsUrl"), g("targetUrl")]
            .into_iter()
            .find(|u| !u.is_empty())
            .map(str::to_string);
        let (failed, pending) = if g("__typename") == "StatusContext" {
            let st = g("state");
            (
                matches!(st, "FAILURE" | "ERROR"),
                matches!(st, "PENDING" | "EXPECTED"),
            )
        } else {
            let pending = g("status") != "COMPLETED";
            let failed =
                !pending && matches!(g("conclusion"), "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED");
            (failed, pending)
        };
        if failed {
            checks.fail += 1;
            if checks.failing_url.is_none() {
                checks.failing_url = url.clone();
            }
        } else if pending {
            checks.pending += 1;
        } else {
            checks.pass += 1;
        }
        if url.is_some() {
            checks.latest_url = url;
        }
    }
    PrInfo {
        number: v
            .get("number")
            .map(|n| n.to_string().trim_matches('"').to_string())
            .unwrap_or_default(),
        url: s("url"),
        state: s("state").unwrap_or_default(),
        is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
        review_decision: s("reviewDecision").filter(|x| !x.is_empty()),
        checks,
    }
}

/// `gh pr view <sel> --repo <slug>`; `sel` is a number, URL, or branch name.
pub fn fetch(slug: Option<&str>, sel: &str) -> Result<PrInfo> {
    let mut args: Vec<String> = vec!["pr".into(), "view".into(), sel.into()];
    if let Some(s) = slug
        && !sel.starts_with("http")
    {
        args.extend(["--repo".into(), s.into()]);
    }
    args.extend(["--json".into(), FIELDS.into()]);
    let out = run_cmd("gh", &args, None, &[], None, Duration::from_secs(15))?;
    if !out.ok() {
        bail!("{}", out.stderr.trim());
    }
    let v: Value = serde_json::from_str(&out.stdout).map_err(|e| anyhow!("gh JSON: {e}"))?;
    Ok(parse_pr(&v))
}

type Cached = (Instant, Result<PrInfo, String>);

#[derive(Default)]
pub struct PrCache {
    map: Mutex<HashMap<String, Cached>>,
    pub disk: Option<DiskCache>,
}

impl PrCache {
    pub fn get(&self, slug: Option<&str>, sel: &str, force: bool) -> Result<PrInfo, String> {
        let k = format!("{}#{sel}", slug.unwrap_or(""));
        if !force
            && let Some((t, v)) = self.map.lock().unwrap().get(&k)
            && t.elapsed() < TTL
        {
            return v.clone();
        }
        let dname = format!("pr-{k}");
        if !force
            && let Some(d) = &self.disk
            && let Some(p) = d.get::<PrInfo>(&dname, &k, Some(TTL.as_secs()))
        {
            self.map
                .lock()
                .unwrap()
                .insert(k, (Instant::now(), Ok(p.clone())));
            return Ok(p);
        }
        let v = fetch(slug, sel).map_err(|e| e.to_string());
        if let (Some(d), Ok(p)) = (&self.disk, &v) {
            d.put(&dname, &k, p);
        }
        self.map
            .lock()
            .unwrap()
            .insert(k, (Instant::now(), v.clone()));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rollup() {
        let v: Value = serde_json::from_str(
            r#"{"number":7,"state":"OPEN","isDraft":true,"reviewDecision":"","url":"u",
            "statusCheckRollup":[
              {"__typename":"CheckRun","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"f","completedAt":"2026-01-01T00:00:01Z"},
              {"__typename":"CheckRun","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"s","completedAt":"2026-01-01T00:00:09Z"},
              {"__typename":"CheckRun","status":"IN_PROGRESS","conclusion":"","detailsUrl":"p"},
              {"__typename":"StatusContext","state":"SUCCESS","targetUrl":"t"}]}"#,
        )
        .unwrap();
        let p = parse_pr(&v);
        assert_eq!(p.number, "7");
        assert!(p.is_draft);
        assert_eq!(p.review_decision, None);
        assert_eq!((p.checks.fail, p.checks.pass, p.checks.pending), (1, 2, 1));
        assert_eq!(p.checks.failing_url.as_deref(), Some("f"));
    }

    #[test]
    fn superseded_cancelled_runs_are_ignored() {
        // Shape of PR #4603: concurrency-cancelled duplicates, latest of each skipped/success.
        let v: Value = serde_json::from_str(
            r#"{"number":4603,"state":"OPEN","statusCheckRollup":[
              {"__typename":"CheckRun","workflowName":"Charts","name":"Add PR comment","status":"COMPLETED","conclusion":"CANCELLED","startedAt":"2026-11-12T19:13:16Z"},
              {"__typename":"CheckRun","workflowName":"Charts","name":"Add PR comment","status":"COMPLETED","conclusion":"SKIPPED","startedAt":"2026-11-12T19:13:25Z"},
              {"__typename":"CheckRun","workflowName":"Charts","name":"Merge","status":"COMPLETED","conclusion":"CANCELLED","startedAt":"2026-11-12T19:13:16Z"},
              {"__typename":"CheckRun","workflowName":"Charts","name":"Merge","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-11-12T19:13:25Z"},
              {"__typename":"CheckRun","workflowName":"Lint","name":"lint","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-11-12T19:00:00Z"},
              {"__typename":"CheckRun","workflowName":"Lint","name":"lint","status":"COMPLETED","conclusion":"FAILURE","startedAt":"2026-11-12T19:10:00Z","detailsUrl":"lf"},
              {"__typename":"CheckRun","workflowName":"Plan","name":"plan","status":"COMPLETED","conclusion":"TIMED_OUT","startedAt":"2026-11-12T19:00:00Z"},
              {"__typename":"CheckRun","workflowName":"Plan","name":"plan","status":"COMPLETED","conclusion":"SUCCESS","startedAt":"2026-11-12T19:20:00Z"}]}"#,
        )
        .unwrap();
        let p = parse_pr(&v);
        assert_eq!((p.checks.fail, p.checks.pass, p.checks.pending), (1, 3, 0));
        assert_eq!(p.checks.failing_url.as_deref(), Some("lf"));
    }
}
