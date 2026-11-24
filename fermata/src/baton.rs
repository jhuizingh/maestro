//! The baton tracker seam: locate baton, resolve a context, call `tracker.sh`.
use crate::model::{Branch, Decision, DecisionStatus, Task};
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

/// The tmux session names baton gives the task in `worktree` (current, then legacy), from
/// `task-identity.sh --worktree`. Empty when it is not a baton worktree.
pub fn session_names(baton: &Path, worktree: &str) -> Vec<String> {
    let script = baton.join("scripts/task-identity.sh");
    let args = [
        script.to_string_lossy().into_owned(),
        "--worktree".into(),
        worktree.into(),
        "--format".into(),
        "json".into(),
    ];
    let out = run_cmd(
        "bash",
        &args,
        Some(&std::env::temp_dir()),
        &[],
        None,
        TRACKER_TIMEOUT,
    );
    match out {
        Ok(o) if o.ok() => parse_session_names(&o.stdout),
        _ => vec![],
    }
}

fn parse_session_names(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec![];
    };
    ["session_name", "session_name_legacy"]
        .iter()
        .filter_map(|k| v.get(k)?.as_str().filter(|s| !s.is_empty()))
        .map(String::from)
        .collect()
}

/// The tracker verbs fermata uses. `note` and `reply` are the only writes.
pub trait Tracker: Send + Sync {
    fn list(&self, status: &str) -> Result<Vec<Task>>;
    fn get(&self, id: &str) -> Result<Task>;
    fn list_branches(&self, id: &str) -> Result<Vec<Branch>>;
    fn note(&self, id: &str, text: &str) -> Result<()>;
    /// A task's unresolved decisions. A backend without them errors, and readers fall back.
    fn decisions(&self, _id: &str) -> Result<Vec<Decision>> {
        bail!("unsupported verb: decisions")
    }
    /// The same, resolved ones included.
    fn decisions_all(&self, _id: &str) -> Result<Vec<Decision>> {
        bail!("unsupported verb: decisions")
    }
    /// Your input on one decision: `kind` is `answer` or `question`.
    fn reply(&self, _id: &str, _did: &str, _kind: &str, _text: &str) -> Result<()> {
        bail!("unsupported verb: reply")
    }
    /// The name `claim` assigns to here; `None` when the backend cannot say.
    fn whoami(&self) -> Result<Option<String>> {
        Ok(None)
    }
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

    fn whoami(&self) -> Result<Option<String>> {
        let out = self.call(&["whoami"])?;
        Ok(parse_whoami(&out.stdout))
    }

    fn decisions(&self, id: &str) -> Result<Vec<Decision>> {
        let out = self.call(&["decisions", id])?;
        parse_decisions(&out.stdout)
    }
    fn decisions_all(&self, id: &str) -> Result<Vec<Decision>> {
        let out = self.call(&["decisions", id, "--all"])?;
        parse_decisions(&out.stdout)
    }

    fn reply(&self, id: &str, did: &str, kind: &str, text: &str) -> Result<()> {
        let args = ["reply", id, did, "--kind", kind, "--text", text];
        self.call(&[&args[..], &["--by", "user", "--via", "fermata"]].concat())
            .map(|_| ())
    }
}

/// One decision as `tracker.sh decisions` prints it (baton's references/tracker.md).
#[derive(serde::Deserialize)]
struct WireDecision {
    id: String,
    question: String,
    #[serde(default)]
    options: Vec<String>,
    recommendation: Option<String>,
    status: DecisionStatus,
    asked_at: Option<String>,
    answer: Option<WireText>,
    answered_at: Option<String>,
    pending_question: Option<WireText>,
    #[serde(default)]
    replies: Vec<crate::model::Reply>,
    asked_by: Option<String>,
    resolved_at: Option<String>,
    outcome: Option<String>,
}

#[derive(serde::Deserialize)]
struct WireText {
    text: String,
}

/// `decisions` output as fermata's decisions; a numeric recommendation names its option.
fn parse_decisions(json: &str) -> Result<Vec<Decision>> {
    if json.trim().is_empty() {
        return Ok(vec![]);
    }
    let wire: Vec<WireDecision> = serde_json::from_str(json)?;
    Ok(wire
        .into_iter()
        .map(|w| {
            let rec = w.recommendation.filter(|r| !r.trim().is_empty()).map(|r| {
                match r.trim().parse::<usize>().ok().and_then(|n| {
                    w.options
                        .get(n.wrapping_sub(1))
                        .map(|o| format!("{n}) {o}"))
                }) {
                    Some(x) => x,
                    None => r,
                }
            });
            Decision {
                id: Some(w.id),
                text: w.question,
                options: w.options,
                recommendation: rec,
                status: w.status,
                asked_at: w.asked_at,
                answer: w.answer.map(|a| a.text),
                answered_at: w.answered_at,
                pending_question: w.pending_question.map(|q| q.text),
                label_missing: false,
                replies: w.replies,
                asked_by: w.asked_by,
                resolved_at: w.resolved_at,
                outcome: w.outcome,
            }
        })
        .collect())
}

/// `{"user": …}` from `tracker.sh whoami`; a blank or missing user is no answer.
fn parse_whoami(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let u = v.get("user")?.as_str()?.trim();
    (!u.is_empty()).then(|| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_current_then_legacy() {
        let j =
            r#"{"session_name":"slug-x-1","session_name_legacy":"baton-x-1-slug","leaf":"x-1"}"#;
        assert_eq!(parse_session_names(j), vec!["slug-x-1", "baton-x-1-slug"]);
        assert_eq!(
            parse_session_names(r#"{"session_name":""}"#),
            Vec::<String>::new()
        );
        assert!(parse_session_names("not json").is_empty());
    }

    #[test]
    fn decisions_parse_the_seam_shape() {
        let j = r#"[{"id":"d1a2b3c","question":"Merge?","options":["yes","no"],
            "recommendation":"2","asked_at":"2026-11-23T10:00:00Z","asked_by":"orchestrator",
            "status":"open","answer":null,"answered_at":null,
            "pending_question":{"text":"why not?","at":"x","by":"user"},
            "replies":[{"kind":"question","text":"why not?","at":"x","by":"user","via":null}]},
          {"id":"d0000001","question":"Name?","options":[],"recommendation":"short",
            "asked_at":null,"status":"answered","answer":{"text":"bob","at":"y","by":"user"},
            "answered_at":"2026-11-23T11:00:00Z","pending_question":null}]"#;
        let d = parse_decisions(j).unwrap();
        assert_eq!(d[0].id.as_deref(), Some("d1a2b3c"));
        assert_eq!(d[0].recommendation.as_deref(), Some("2) no"));
        assert_eq!(d[0].pending_question.as_deref(), Some("why not?"));
        assert!(d[0].is_open());
        assert_eq!(d[0].replies[0].kind, "question");
        assert_eq!(d[0].asked_by.as_deref(), Some("orchestrator"));
        assert_eq!(d[1].status, DecisionStatus::Answered);
        assert_eq!(d[1].answer.as_deref(), Some("bob"));
        assert_eq!(d[1].recommendation.as_deref(), Some("short"));
        assert!(parse_decisions(" \n").unwrap().is_empty());
    }

    #[test]
    fn whoami_reads_the_user() {
        let j = r#"{"user":" Pat Doe ","source":"git user.name"}"#;
        assert_eq!(parse_whoami(j).as_deref(), Some("Pat Doe"));
        assert_eq!(parse_whoami(r#"{"user":""}"#), None);
        assert_eq!(parse_whoami("nope"), None);
    }

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
