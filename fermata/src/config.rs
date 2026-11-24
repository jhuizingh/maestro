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
    /// Seconds `info` may run; read through `info_timeout()`, which applies the default and cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

pub const INFO_TIMEOUT_DEFAULT: u64 = 5;
pub const INFO_TIMEOUT_MAX: u64 = 30;

impl TicketCfg {
    pub fn info_timeout(&self) -> std::time::Duration {
        let s = self.timeout.unwrap_or(INFO_TIMEOUT_DEFAULT);
        std::time::Duration::from_secs(s.clamp(1, INFO_TIMEOUT_MAX))
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workers: Option<WorkersCfg>,
}

/// `workers:` in `fermata.yaml`: how background workers are linked to tasks.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct WorkersCfg {
    /// Warn when a home session starts an Agent whose description names no known task id.
    #[serde(default)]
    pub warn_unlinked: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Context {
    pub name: String,
    pub workspace: PathBuf,
    pub tracker_dir: Option<PathBuf>,
    pub github_owner: Option<String>,
    /// `home`, when set (tilde-expanded); with `workspace`, the context's own directories.
    pub home: Option<PathBuf>,
    /// `member_repos` as written: paths or globs, `~` unexpanded.
    pub member_repos: Vec<String>,
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
    let home = s(&y["home"]).map(|h| expand_tilde(&h));
    let member_repos = y["member_repos"]
        .as_sequence()
        .map(|v| v.iter().filter_map(s).collect())
        .unwrap_or_default();
    let mut ctx = Context {
        name,
        workspace: workspace.to_path_buf(),
        tracker_dir,
        github_owner,
        home,
        member_repos,
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

/// The context a directory belongs to, by baton's rule in `resolve-context.sh` (rungs 2 and 3;
/// it owns the rule, this mirrors it): first a context's own workspace or `home`, then its
/// `member_repos` (globs allowed), each also covering its `-worktrees` sibling. Ties go to the
/// first registered. No default context: a path in none of them belongs to none.
pub fn context_of_path<'a>(contexts: &'a [Context], path: &Path) -> Option<&'a Context> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let under = |p: &Path| {
        let sib = PathBuf::from(format!("{}-worktrees", p.display()));
        path.starts_with(p) || path.starts_with(&sib)
    };
    let own = contexts
        .iter()
        .find(|c| under(&c.workspace) || c.home.as_deref().is_some_and(under));
    own.or_else(|| {
        contexts.iter().find(|c| {
            c.member_repos.iter().any(|m| {
                let pat = expand_tilde(m)
                    .to_string_lossy()
                    .trim_end_matches('/')
                    .to_string();
                path.ancestors().any(|a| {
                    let a = a.to_string_lossy();
                    glob(&pat, &a) || a.strip_suffix("-worktrees").is_some_and(|b| glob(&pat, b))
                })
            })
        })
    })
}

/// Shell-style glob over a whole path: `*` and `?` never match `/`.
pub fn glob(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') => (0..=s.len())
                .take_while(|&i| i == 0 || s[i - 1] != '/')
                .any(|i| go(&p[1..], &s[i..])),
            Some('?') => s.first().is_some_and(|&c| c != '/') && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(&p, &s)
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

/// A key as one YAML value: quoted when it would read as something else. JSON is YAML.
fn scalar(s: &str) -> String {
    match serde_yaml::to_string(s) {
        Ok(y) if y.trim_end().lines().count() == 1 => y.trim_end().to_string(),
        _ => serde_json::Value::from(s).to_string(),
    }
}

/// Render a priorities block in the adapter's YAML style.
fn render_priorities(p: &[Priority]) -> String {
    let mut s = String::from("priorities:\n");
    if p.is_empty() {
        s = "priorities: []\n".into();
    }
    for e in p {
        if let Some(t) = &e.ticket {
            s.push_str(&format!("  - ticket: {}\n", scalar(t)));
        } else if let Some(ep) = &e.epic {
            s.push_str(&format!("  - epic: {}\n", scalar(ep)));
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
    // Followed, so that a linked file is replaced and not its link.
    let (path, text) = match std::fs::canonicalize(&path) {
        Ok(p) => {
            let text = std::fs::read_to_string(&p)?;
            (p, text)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (path, String::new()),
        Err(e) => return Err(e.into()),
    };
    let tmp = path.with_extension(format!("yaml.tmp{}", std::process::id()));
    std::fs::write(&tmp, rewrite_priorities_text(&text, p))?;
    if let Ok(m) = std::fs::metadata(&path) {
        std::fs::set_permissions(&tmp, m.permissions())?;
    }
    std::fs::rename(&tmp, &path)?;
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

    #[test]
    fn saving_priorities_keeps_the_file_it_is_in() {
        use std::os::unix::fs::PermissionsExt;
        let sc = crate::store::tests::scratch("save-priorities");
        let ws = sc.0.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let mut c = crate::app::tests::ctx("c");
        c.workspace = ws.clone();
        let (link, real) = (ws.join("fermata.yaml"), sc.0.join("real.yaml"));
        std::fs::write(&real, "adapter: x\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let p = [
            "ABC-9",
            "yes",
            "a: b",
            "x\n  - ticket: evil",
            "# c",
            "'q",
            "\u{85}",
        ]
        .map(|t| Priority {
            ticket: Some(t.into()),
            ..Default::default()
        });
        save_priorities(&c, &p).unwrap();
        assert!(link.is_symlink());
        assert_eq!(real.metadata().unwrap().permissions().mode() & 0o777, 0o640);
        let text = std::fs::read_to_string(&real).unwrap();
        assert!(text.contains("adapter: x") && text.contains("  - ticket: ABC-9\n"));
        let v: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let back: Vec<&str> = v["priorities"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|e| e["ticket"].as_str().unwrap())
            .collect();
        let want: Vec<&str> = p.iter().map(|p| p.ticket.as_deref().unwrap()).collect();
        assert_eq!(back, want);
        // Unreadable is not empty: nothing may be rewritten from it.
        std::fs::write(&real, b"\xff\xfe").unwrap();
        assert!(save_priorities(&c, &p).is_err());
        assert_eq!(std::fs::read(&real).unwrap(), b"\xff\xfe");
    }

    const SAMPLE: &str = "ticket:\n  key_from: label\n  pattern: '^jira-([A-Z]+-[0-9]+)$'\n  info: ['./x.sh', '{key}']\npriorities:                # pinned\n  - epic: ABC-100\n  - ticket: ABC-1\n\n# trailing\npr:\n  provider: github\n";

    #[test]
    fn parse_adapter() {
        let a: Adapter = serde_yaml::from_str(SAMPLE).unwrap();
        assert_eq!(a.priorities.len(), 2);
        let t = a.ticket.unwrap();
        assert_eq!(t.ttl, 300);
        assert_eq!(t.info_timeout().as_secs(), INFO_TIMEOUT_DEFAULT);
        let t: TicketCfg = serde_yaml::from_str("pattern: x\ntimeout: 600\n").unwrap();
        assert_eq!(t.info_timeout().as_secs(), INFO_TIMEOUT_MAX);
        let t: TicketCfg = serde_yaml::from_str("pattern: x\ntimeout: 0\n").unwrap();
        assert_eq!(t.info_timeout().as_secs(), 1);
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

    #[test]
    fn context_of_path_follows_batons_rungs() {
        let c = |name: &str, ws: &str, members: &[&str]| Context {
            name: name.into(),
            workspace: ws.into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: members.iter().map(|m| m.to_string()).collect(),
            ctx_json: String::new(),
            adapter: None,
            adapter_error: None,
        };
        // "personal" globs over "/code/fam-ws"'s directory, but a workspace wins for its own.
        let cs = [
            c("personal", "/code/me-ws", &["/code/fam-*", "/code/tool"]),
            c("family", "/code/fam-ws", &[]),
        ];
        let of = |p: &str| context_of_path(&cs, Path::new(p)).map(|c| c.name.as_str());
        assert_eq!(of("/code/me-ws"), Some("personal"));
        assert_eq!(of("/code/fam-ws/sub"), Some("family"));
        assert_eq!(of("/code/fam-ws-worktrees/x"), Some("family"));
        assert_eq!(of("/code/fam-recipes/src"), Some("personal"));
        assert_eq!(of("/code/fam-recipes-worktrees/t-1"), Some("personal"));
        assert_eq!(of("/code/tool-worktrees/t-2"), Some("personal"));
        assert_eq!(of("/code/toolbox"), None);
        assert_eq!(of("/code/fam/x"), None); // `*` needs the "-" before it
        assert_eq!(of(""), None);
        assert!(glob("/a/*", "/a/b") && !glob("/a/*", "/a/b/c") && glob("/a/b?", "/a/bc"));
    }
}
