//! Contexts (baton registry → context.yaml) and per-context adapters (fermata.yaml).
use crate::util::expand_tilde;
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct TicketCfg {
    #[serde(default = "default_key_from")]
    pub key_from: String,
    pub pattern: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub info: Vec<String>,
    #[serde(default = "default_ttl")]
    pub ttl: u64,
}

fn default_key_from() -> String {
    "label".into()
}
fn default_ttl() -> u64 {
    300
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ProgressCfg {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub checklist: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct PrCfg {
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Priority {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Adapter {
    #[serde(default)]
    pub ticket: Option<TicketCfg>,
    #[serde(default)]
    pub progress: Option<ProgressCfg>,
    #[serde(default)]
    pub pr: Option<PrCfg>,
    #[serde(default)]
    pub priorities: Vec<Priority>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Context {
    pub name: String,
    pub workspace: PathBuf,
    pub tracker_dir: Option<PathBuf>,
    pub github_owner: Option<String>,
    #[serde(skip)]
    pub ctx_json: String,
    pub adapter: Option<Adapter>,
    pub adapter_error: Option<String>,
}

impl Context {
    pub fn adapter_path(&self) -> PathBuf {
        self.workspace.join("fermata.yaml")
    }
}

#[derive(Debug, Deserialize)]
struct Registry {
    #[serde(default)]
    workspaces: Vec<String>,
}

pub fn registry_path() -> PathBuf {
    std::env::var("BATON_REGISTRY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| expand_tilde("~/.config/baton/registry.yaml"))
}

pub fn workspaces() -> Result<Vec<PathBuf>> {
    if let Ok(w) = std::env::var("BATON_WORKSPACES") {
        return Ok(w
            .split(':')
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .collect());
    }
    let p = registry_path();
    let r: Registry = serde_yaml::from_str(
        &std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?,
    )?;
    Ok(r.workspaces.iter().map(|s| expand_tilde(s)).collect())
}

/// Parse one workspace's context.yaml + fermata.yaml. `ctx_json` is filled in by the caller.
pub fn load_context(workspace: &Path) -> Result<Context> {
    let cfg = workspace.join("context.yaml");
    let y: serde_yaml::Value = serde_yaml::from_str(
        &std::fs::read_to_string(&cfg).with_context(|| format!("read {}", cfg.display()))?,
    )?;
    let s = |v: &serde_yaml::Value| v.as_str().map(str::to_string);
    let name = s(&y["name"]).unwrap_or_else(|| {
        workspace
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    let tracker_dir = s(&y["task_tracking"]["dir"]).map(|d| expand_tilde(&d));
    let github_owner = s(&y["github"]["owner"]);
    let mut ctx = Context {
        name,
        workspace: workspace.to_path_buf(),
        tracker_dir,
        github_owner,
        ctx_json: String::new(),
        adapter: None,
        adapter_error: None,
    };
    let ap = ctx.adapter_path();
    if ap.exists() {
        match std::fs::read_to_string(&ap)
            .map_err(anyhow::Error::from)
            .and_then(|t| Ok(serde_yaml::from_str::<Adapter>(&t)?))
        {
            Ok(a) => ctx.adapter = Some(a),
            Err(e) => ctx.adapter_error = Some(format!("{}: {e}", ap.display())),
        }
    }
    Ok(ctx)
}

/// Fallback context JSON when resolve-context.sh is unavailable: the yaml plus `_workspace`.
pub fn yaml_to_ctx_json(workspace: &Path) -> Result<String> {
    let y: serde_yaml::Value =
        serde_yaml::from_str(&std::fs::read_to_string(workspace.join("context.yaml"))?)?;
    let mut j = serde_json::to_value(y)?;
    if let Some(o) = j.as_object_mut() {
        o.insert(
            "_workspace".into(),
            serde_json::Value::String(workspace.to_string_lossy().into_owned()),
        );
        o.insert("_match".into(), "env".into());
    }
    Ok(j.to_string())
}

/// Render a priorities block in the adapter's YAML style.
fn render_priorities(p: &[Priority]) -> String {
    let mut s = String::from("priorities:\n");
    if p.is_empty() {
        s = "priorities: []\n".into();
    }
    for e in p {
        if let Some(t) = &e.ticket {
            s.push_str(&format!("  - ticket: {t}\n"));
        } else if let Some(ep) = &e.epic {
            s.push_str(&format!("  - epic: {ep}\n"));
        }
    }
    s
}

/// Replace only the top-level `priorities:` block of an adapter file's text.
pub fn rewrite_priorities_text(text: &str, p: &[Priority]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let is_top = |l: &str| {
        !l.is_empty() && !l.starts_with(' ') && !l.starts_with('\t') && !l.starts_with('#')
    };
    let start = lines.iter().position(|l| l.starts_with("priorities:"));
    let block = render_priorities(p);
    let Some(start) = start else {
        let mut out = text.to_string();
        if !out.ends_with('\n') && !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&block);
        return out;
    };
    let mut end = start + 1;
    while end < lines.len() && !is_top(lines[end]) {
        end += 1;
    }
    // Keep trailing comments/blank lines that belong to the next block.
    while end > start + 1 && !lines[end - 1].trim_start().starts_with('-') {
        end -= 1;
    }
    let mut out = String::new();
    for l in &lines[..start] {
        out.push_str(l);
        out.push('\n');
    }
    out.push_str(&block);
    for l in &lines[end..] {
        out.push_str(l);
        out.push('\n');
    }
    out
}

pub fn save_priorities(ctx: &Context, p: &[Priority]) -> Result<()> {
    let path = ctx.adapter_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, rewrite_priorities_text(&text, p))?;
    Ok(())
}

/// Toggle a pin; returns the new list.
pub fn toggle_pin(list: &[Priority], pin: Priority) -> Vec<Priority> {
    let mut v = list.to_vec();
    if let Some(i) = v.iter().position(|x| *x == pin) {
        v.remove(i);
    } else {
        v.push(pin);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "ticket:\n  key_from: label\n  pattern: '^jira-([A-Z]+-[0-9]+)$'\n  info: ['./x.sh', '{key}']\npriorities:                # pinned\n  - epic: ABC-100\n  - ticket: ABC-1\n\n# trailing\npr:\n  provider: github\n";

    #[test]
    fn parse_adapter() {
        let a: Adapter = serde_yaml::from_str(SAMPLE).unwrap();
        assert_eq!(a.priorities.len(), 2);
        assert_eq!(a.ticket.unwrap().ttl, 300);
    }

    #[test]
    fn rewrite_only_priorities() {
        let p = vec![Priority {
            ticket: Some("ABC-9".into()),
            epic: None,
        }];
        let out = rewrite_priorities_text(SAMPLE, &p);
        assert!(out.contains("  - ticket: ABC-9\n"));
        assert!(!out.contains("ABC-100"));
        assert!(out.contains("# trailing\npr:\n  provider: github\n"));
        assert!(out.starts_with("ticket:\n  key_from: label\n"));
        let a: Adapter = serde_yaml::from_str(&out).unwrap();
        assert_eq!(a.priorities, p);
    }

    #[test]
    fn rewrite_appends_when_missing() {
        let out = rewrite_priorities_text("pr:\n  provider: github", &[]);
        assert!(out.ends_with("priorities: []\n"));
        let a: Adapter = serde_yaml::from_str(&out).unwrap();
        assert!(a.priorities.is_empty());
    }

    #[test]
    fn toggle() {
        let e = Priority {
            epic: Some("E-1".into()),
            ticket: None,
        };
        let v = toggle_pin(&[], e.clone());
        assert_eq!(v.len(), 1);
        assert!(toggle_pin(&v, e).is_empty());
    }
}
