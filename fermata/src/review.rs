//! Review waiting on you: asked of baton's `pr-review.sh rank`, the one place that rule lives.
//! Cached 60s per task, like PRs.
use crate::model::Task;
use crate::util::run_cmd;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
pub const LABEL: &str = "pr-review";

/// `rank` output for one task: `Some(why)` when its review is waiting on you.
pub fn parse_rank(stdout: &str, id: &str) -> Result<Option<String>> {
    let v: Value = serde_json::from_str(stdout).map_err(|e| anyhow!("rank JSON: {e}"))?;
    let t = v
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|t| t.get("id").and_then(Value::as_str) == Some(id))
        })
        .ok_or_else(|| anyhow!("rank: {id} missing from output"))?;
    let s = |k: &str| t.get(k).and_then(Value::as_str);
    Ok((s("review_pending") == Some("yes"))
        .then(|| s("review_why").unwrap_or("review waiting").to_string()))
}

/// `pr-review.sh rank` over a one-task array.
pub fn fetch(baton: &Path, task: &Task) -> Result<Option<String>> {
    let script = baton.join("scripts/pr-review.sh");
    let input = serde_json::to_vec(&[task])?;
    let out = run_cmd(
        "bash",
        &[script.to_string_lossy().into_owned(), "rank".into()],
        Some(&std::env::temp_dir()),
        &[],
        Some(&input),
        Duration::from_secs(20),
    )?;
    if !out.ok() {
        bail!("pr-review rank: {}", out.stderr.trim());
    }
    parse_rank(&out.stdout, &task.id)
}

#[derive(Default)]
pub struct ReviewCache {
    map: Mutex<HashMap<String, (Instant, Option<String>)>>,
}

impl ReviewCache {
    /// Only `pr-review` tasks are asked; a failed lookup reads as not waiting.
    pub fn get(&self, baton: Option<&Path>, task: &Task, force: bool) -> Option<String> {
        if !task.has_label(LABEL) {
            return None;
        }
        let k = format!("{}|{}", task.id, task.status);
        if !force
            && let Some((t, v)) = self.map.lock().unwrap().get(&k)
            && t.elapsed() < TTL
        {
            return v.clone();
        }
        let v = match baton.map(|b| fetch(b, task)) {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                crate::log::warn(format!("review {}: {e}", task.id));
                None
            }
            None => None,
        };
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
    fn parses_rank_output() {
        let out = r#"[{"id":"a","review_pending":"yes","review_why":"review requested"},
                      {"id":"b","review_pending":"no","review_why":"review submitted (APPROVED)"},
                      {"id":"c","review_pending":"no"}]"#;
        assert_eq!(
            parse_rank(out, "a").unwrap().as_deref(),
            Some("review requested")
        );
        assert_eq!(parse_rank(out, "b").unwrap(), None);
        assert_eq!(parse_rank(out, "c").unwrap(), None);
        assert!(parse_rank(out, "z").is_err());
        assert!(parse_rank("nope", "a").is_err());
    }

    #[test]
    fn non_review_tasks_are_never_asked() {
        let c = ReviewCache::default();
        let t = Task {
            id: "x".into(),
            ..Default::default()
        };
        // A bogus baton path would fail loudly if it were called.
        assert_eq!(c.get(Some(Path::new("/nonexistent")), &t, true), None);
        assert!(c.map.lock().unwrap().is_empty());
    }

    /// The real script, on a task whose PR cannot be read: an open leaf still counts as waiting.
    #[test]
    fn real_rank_script_contract() {
        let baton = Path::new(env!("CARGO_MANIFEST_DIR")).join("../baton");
        let t = Task {
            id: "r".into(),
            status: "in_progress".into(),
            labels: vec![LABEL.into()],
            description: "Review (no URL recorded)".into(),
            ..Default::default()
        };
        assert_eq!(
            fetch(&baton, &t).unwrap().as_deref(),
            Some("leaf open, no PR URL")
        );
        let closed = Task {
            status: "closed".into(),
            ..t
        };
        assert_eq!(fetch(&baton, &closed).unwrap(), None);
    }
}
