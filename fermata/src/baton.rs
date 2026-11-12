//! The baton tracker seam: locate baton, resolve a context, call `tracker.sh`.
use crate::model::{Branch, Task};
use crate::util::{CmdOutput, run_cmd};
use anyhow::{Result, anyhow, bail};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TRACKER_TIMEOUT: Duration = Duration::from_secs(20);

/// Locate baton: harness env → dev clone → install record → newest cached.
pub fn locate_baton() -> Option<PathBuf> {
    let ok = |p: &Path| p.join("scripts").is_dir();
    if let Ok(r) = std::env::var("CLAUDE_PLUGIN_ROOT") {
        let p = PathBuf::from(r);
        if ok(&p) {
            return Some(p);
        }
    }
    let home = dirs::home_dir()?;
    let dev = home.join("code/maestro/baton");
    if ok(&dev) {
        return Some(dev);
    }
    let bp = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".claude"))
        .join("plugins");
    if let Some(p) = from_install_record(&bp.join("installed_plugins.json"))
        && ok(&p)
    {
        return Some(p);
    }
    newest_cached(&bp.join("cache/maestro/baton")).filter(|p| ok(p))
}

fn from_install_record(path: &Path) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let arr = v.get("plugins")?.get("baton@maestro")?.as_array()?;
    let pick = arr
        .iter()
        .find(|e| e.get("scope").and_then(|s| s.as_str()) == Some("user"))
        .or_else(|| arr.first())?;
    Some(PathBuf::from(pick.get("installPath")?.as_str()?))
}

fn version_key(s: &str) -> Vec<u64> {
    s.split(['.', '-'])
        .map(|p| p.parse::<u64>().unwrap_or(0))
        .collect()
}

fn newest_cached(dir: &Path) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    v.sort_by_key(|p| version_key(&p.file_name().unwrap_or_default().to_string_lossy()));
    v.pop()
}

/// Resolved context JSON via `resolve-context.sh` with `BATON_CONTEXT=<name>`.
pub fn resolve_context(baton: &Path, name: &str) -> Result<String> {
    let script = baton.join("scripts/resolve-context.sh");
    let out = run_cmd(
        "bash",
        &[script.to_string_lossy().into_owned()],
        Some(&std::env::temp_dir()),
        &[("BATON_CONTEXT", name)],
        None,
        TRACKER_TIMEOUT,
    )?;
    if !out.ok() {
        bail!("resolve-context {name}: {}", out.stderr.trim());
    }
    serde_json::from_str::<serde_json::Value>(&out.stdout)
        .map_err(|e| anyhow!("resolve-context {name}: bad JSON: {e}"))?;
    Ok(out.stdout)
}

/// The tracker verbs fermata uses. `note` is the only write.
pub trait Tracker: Send + Sync {
    fn list(&self, status: &str) -> Result<Vec<Task>>;
    fn get(&self, id: &str) -> Result<Task>;
    fn list_branches(&self, id: &str) -> Result<Vec<Branch>>;
    fn note(&self, id: &str, text: &str) -> Result<()>;
}

/// `tracker.sh --context - <verb>` with the context JSON on stdin.
pub struct ShellTracker {
    pub script: PathBuf,
    pub ctx_json: String,
}

impl ShellTracker {
    pub fn new(baton: &Path, ctx_json: String) -> Self {
        Self {
            script: baton.join("scripts/tracker.sh"),
            ctx_json,
        }
    }

    fn call(&self, verb_args: &[&str]) -> Result<CmdOutput> {
        let mut args = vec![
            self.script.to_string_lossy().into_owned(),
            "--context".into(),
            "-".into(),
        ];
        args.extend(verb_args.iter().map(|s| s.to_string()));
        let out = run_cmd(
            "bash",
            &args,
            Some(&std::env::temp_dir()),
            &[],
            Some(self.ctx_json.as_bytes()),
            TRACKER_TIMEOUT,
        )?;
        match out.status {
            Some(0) => Ok(out),
            Some(3) => bail!("not found: {}", verb_args.join(" ")),
            Some(4) => bail!("unsupported verb: {}", verb_args[0]),
            _ => bail!("tracker {}: {}", verb_args.join(" "), out.stderr.trim()),
        }
    }
}

impl Tracker for ShellTracker {
    fn list(&self, status: &str) -> Result<Vec<Task>> {
        let out = self.call(&["list", "--status", status])?;
        Ok(serde_json::from_str(&out.stdout)?)
    }

    fn get(&self, id: &str) -> Result<Task> {
        let out = self.call(&["get", id])?;
        Ok(serde_json::from_str(&out.stdout)?)
    }

    fn list_branches(&self, id: &str) -> Result<Vec<Branch>> {
        let out = self.call(&["list-branches", id])?;
        if out.stdout.trim().is_empty() {
            return Ok(vec![]);
        }
        Ok(serde_json::from_str(&out.stdout)?)
    }

    fn note(&self, id: &str, text: &str) -> Result<()> {
        self.call(&["note", id, text]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_sort_numerically() {
        let d = std::env::temp_dir().join(format!("fermata-vtest-{}", std::process::id()));
        for v in ["0.9.0", "0.16.2", "0.10.1"] {
            std::fs::create_dir_all(d.join(v)).unwrap();
        }
        assert_eq!(newest_cached(&d).unwrap().file_name().unwrap(), "0.16.2");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn install_record_prefers_user_scope() {
        let d = std::env::temp_dir().join(format!("fermata-irec-{}.json", std::process::id()));
        std::fs::write(
            &d,
            r#"{"plugins":{"baton@maestro":[{"scope":"project","installPath":"/p"},{"scope":"user","installPath":"/u"}]}}"#,
        )
        .unwrap();
        assert_eq!(from_install_record(&d), Some(PathBuf::from("/u")));
        std::fs::remove_file(&d).unwrap();
    }
}
