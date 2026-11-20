//! Review waiting on you: asked of baton's `pr-review.sh rank`, the one place that rule lives.
//! One rank call per context per refresh, fed fermata's own PR cache (`--pr-data`) and a login
//! resolved once, so a warm refresh makes no GitHub calls at all.
use crate::model::Task;
use crate::util::run_cmd;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
/// A failed or unknown answer is asked again sooner, but not on the very next refresh.
const FAIL_TTL: Duration = Duration::from_secs(20);
/// How long a failed login lookup is remembered before `gh api user` is tried again.
const LOGIN_RETRY: Duration = Duration::from_secs(60);
pub const LABEL: &str = "pr-review";

/// What `rank` said about one `pr-review` task.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Waiting(String),
    /// The PR (or the whole lookup) could not be read: may be waiting, never "not waiting".
    Unknown(String),
    NotWaiting,
}

/// The first PR URL in a task's description, then notes: the same rule as rank's `pr_url`.
pub fn pr_url(t: &Task) -> Option<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"https?://[^\s/]+/[^\s/]+/[^\s/]+/pull/[0-9]+").expect("pr url regex")
    });
    let text = format!("{}\n{}", t.description, t.notes);
    re.find(&text).map(|m| m.as_str().to_string())
}

/// `rank` output: the verdict for every `pr-review` task in it, by id.
pub fn parse_rank(stdout: &str) -> Result<HashMap<String, Verdict>> {
    let v: Value = serde_json::from_str(stdout).map_err(|e| anyhow!("rank JSON: {e}"))?;
    let arr = v.as_array().ok_or_else(|| anyhow!("rank: not an array"))?;
    let mut out = HashMap::new();
    for t in arr {
        let s = |k: &str| t.get(k).and_then(Value::as_str);
        let Some(id) = s("id") else { continue };
        let why = s("review_why").unwrap_or("").to_string();
        let v = match s("review_pending") {
            Some("yes") if why.is_empty() => Verdict::Waiting("review waiting".into()),
            Some("yes") => Verdict::Waiting(why),
            Some("unknown") => Verdict::Unknown(if why.is_empty() {
                "review status unknown".into()
            } else {
                why
            }),
            _ => Verdict::NotWaiting,
        };
        out.insert(id.to_string(), v);
    }
    Ok(out)
}

/// One `pr-review.sh rank` over `tasks`, with `pr_data` standing in for gh.
pub fn rank(
    baton: &Path,
    tasks: &[&Task],
    login: Option<&str>,
    pr_data: &Value,
) -> Result<HashMap<String, Verdict>> {
    let script = baton.join("scripts/pr-review.sh");
    let input = serde_json::to_vec(tasks)?;
    // A per-call file: concurrent contexts each rank at once.
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let data = std::env::temp_dir().join(format!(
        "fermata-prdata-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&data, serde_json::to_vec(pr_data)?)?;
    let mut args = vec![
        script.to_string_lossy().into_owned(),
        "rank".into(),
        "--pr-data".into(),
        data.to_string_lossy().into_owned(),
    ];
    if let Some(l) = login {
        args.extend(["--login".into(), l.into()]);
    }
    let out = run_cmd(
        "bash",
        &args,
        Some(&std::env::temp_dir()),
        &[],
        Some(&input),
        Duration::from_secs(20),
    );
    let _ = std::fs::remove_file(&data);
    let out = out?;
    if !out.ok() {
        bail!("pr-review rank: {}", out.stderr.trim());
    }
    parse_rank(&out.stdout)
}

/// The GitHub login, via `gh api user`.
fn gh_login() -> Result<String> {
    let args: Vec<String> = ["api", "user", "--jq", ".login"].map(Into::into).into();
    let out = run_cmd("gh", &args, None, &[], None, Duration::from_secs(10))?;
    let l = out.stdout.trim();
    if !out.ok() || l.is_empty() {
        bail!("gh api user: {}", out.stderr.trim());
    }
    Ok(l.to_string())
}

type Entry = (Instant, Verdict);

#[derive(Default)]
pub struct ReviewCache {
    /// Keyed `id|status`, so a closed leaf is asked again at once.
    map: Mutex<HashMap<String, Entry>>,
    /// The login once known; a failure is retried after `LOGIN_RETRY`.
    login: Mutex<Option<(Instant, Option<String>)>>,
    /// `rank` processes started, for tests and the log.
    pub rank_calls: AtomicUsize,
}

fn key(t: &Task) -> String {
    format!("{}|{}", t.id, t.status)
}

fn fresh(e: &Entry) -> bool {
    let ttl = match e.1 {
        Verdict::Unknown(_) => FAIL_TTL,
        _ => TTL,
    };
    e.0.elapsed() < ttl
}

impl ReviewCache {
    /// The last verdict for `task` (any age); None for non-review tasks or before the first rank.
    pub fn lookup(&self, task: &Task) -> Option<Verdict> {
        if !task.has_label(LABEL) {
            return None;
        }
        self.map
            .lock()
            .unwrap()
            .get(&key(task))
            .map(|e| e.1.clone())
    }

    pub fn set_login(&self, login: Option<String>) {
        *self.login.lock().unwrap() = Some((Instant::now(), login));
    }

    fn login(&self) -> Option<String> {
        if let Some((t, l)) = self.login.lock().unwrap().as_ref()
            && (l.is_some() || t.elapsed() < LOGIN_RETRY)
        {
            return l.clone();
        }
        let l = gh_login()
            .map_err(|e| crate::log::warn(format!("review: login unknown: {e}")))
            .ok();
        self.set_login(l.clone());
        l
    }

    /// Rank every `pr-review` task in `tasks` in one call, unless all are fresh. `pr` reads one
    /// PR's `{state, reviewRequests, latestReviews}` (fermata's PR cache); a failure there makes
    /// only that task unknown. A failed rank call makes them all unknown, logged once.
    /// Returns the rank call's outcome, or None when no call was made.
    pub fn refresh(
        &self,
        baton: Option<&Path>,
        tasks: &[Task],
        force: bool,
        pr: &(dyn Fn(&str) -> Result<Value, String> + Sync),
    ) -> Option<Result<(), String>> {
        let review: Vec<&Task> = tasks.iter().filter(|t| t.has_label(LABEL)).collect();
        if review.is_empty() {
            return None;
        }
        if !force {
            let m = self.map.lock().unwrap();
            if review.iter().all(|t| m.get(&key(t)).is_some_and(fresh)) {
                return None;
            }
        }
        let mut urls: Vec<String> = review
            .iter()
            .filter(|t| t.status != "closed")
            .filter_map(|t| pr_url(t))
            .collect();
        urls.sort();
        urls.dedup();
        let reads: Vec<(String, Result<Value, String>)> = std::thread::scope(|sc| {
            let hs: Vec<_> = urls
                .iter()
                .map(|u| sc.spawn(move || (u.clone(), pr(u))))
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        let any_read = reads.iter().any(|(_, r)| r.is_ok());
        let mut data = Map::new();
        for (u, r) in reads {
            let v = r.unwrap_or_else(|e| serde_json::json!({ "error": e }));
            data.insert(u, v);
        }
        // No PR could be read: the login would decide nothing, so don't ask for it.
        let login = any_read.then(|| self.login()).flatten();
        self.rank_calls.fetch_add(1, Ordering::Relaxed);
        let verdicts = match baton {
            Some(b) => rank(b, &review, login.as_deref(), &Value::Object(data)),
            None => Err(anyhow!("baton not found")),
        };
        let now = Instant::now();
        let mut m = self.map.lock().unwrap();
        match verdicts {
            Ok(v) => {
                let mut unknown = vec![];
                for t in &review {
                    let vd = v.get(&t.id).cloned().unwrap_or_else(|| {
                        Verdict::Unknown(format!("{} missing from rank output", t.id))
                    });
                    if let Verdict::Unknown(why) = &vd {
                        unknown.push(format!("{} ({why})", t.id));
                    }
                    m.insert(key(t), (now, vd));
                }
                if !unknown.is_empty() {
                    crate::log::warn(format!("review status unknown: {}", unknown.join(", ")));
                }
                Some(Ok(()))
            }
            Err(e) => {
                let why = format!("lookup failed: {e}");
                crate::log::warn(format!("review rank, {} task(s): {why}", review.len()));
                for t in &review {
                    m.insert(key(t), (now, Verdict::Unknown(why.clone())));
                }
                Some(Err(e.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn baton() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../baton")
    }

    fn rt(id: &str, pr: u32) -> Task {
        Task {
            id: id.into(),
            status: "in_progress".into(),
            labels: vec![LABEL.into()],
            description: format!("Review https://github.com/o/p/pull/{pr} (a → main)"),
            ..Default::default()
        }
    }

    fn open_pr(requested: &[&str]) -> Value {
        let rr: Vec<Value> = requested
            .iter()
            .map(|l| serde_json::json!({ "login": l }))
            .collect();
        serde_json::json!({ "state": "OPEN", "reviewRequests": rr, "latestReviews": [] })
    }

    #[test]
    fn parses_rank_output() {
        let out = r#"[{"id":"a","review_pending":"yes","review_why":"review requested"},
                      {"id":"b","review_pending":"no","review_why":"review submitted (APPROVED)"},
                      {"id":"u","review_pending":"unknown","review_why":"PR unreadable (x)"},
                      {"id":"c","review_pending":"no"}]"#;
        let m = parse_rank(out).unwrap();
        assert_eq!(m["a"], Verdict::Waiting("review requested".into()));
        assert_eq!(m["b"], Verdict::NotWaiting);
        assert_eq!(m["u"], Verdict::Unknown("PR unreadable (x)".into()));
        assert_eq!(m["c"], Verdict::NotWaiting);
        assert!(parse_rank("nope").is_err());
    }

    #[test]
    fn pr_url_matches_rank() {
        let mut t = rt("a", 12);
        t.description = "Review https://github.com/o/p/pull/12/files".into();
        assert_eq!(
            pr_url(&t).as_deref(),
            Some("https://github.com/o/p/pull/12")
        );
        t.description = "none".into();
        t.notes = "see http://ghe.example/o/p/pull/3".into();
        assert_eq!(pr_url(&t).as_deref(), Some("http://ghe.example/o/p/pull/3"));
        t.notes = String::new();
        assert_eq!(pr_url(&t), None);
    }

    #[test]
    fn non_review_tasks_are_never_asked() {
        let c = ReviewCache::default();
        let t = Task {
            id: "x".into(),
            ..Default::default()
        };
        // A bogus baton path would fail loudly if it were called.
        c.refresh(
            Some(Path::new("/nonexistent")),
            std::slice::from_ref(&t),
            true,
            &|_| panic!("no PR read for a non-review task"),
        );
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 0);
        assert_eq!(c.lookup(&t), None);
    }

    /// 12 review tasks, PRs served from cache and the login known: one rank process, no gh.
    #[test]
    fn one_rank_call_for_many_tasks_and_none_while_fresh() {
        let c = ReviewCache::default();
        c.set_login(Some("me".into()));
        let tasks: Vec<Task> = (1..=12).map(|i| rt(&format!("r{i}"), i)).collect();
        let reads = AtomicUsize::new(0);
        let pr = |u: &str| {
            reads.fetch_add(1, Ordering::Relaxed);
            Ok(if u.ends_with("/3") {
                open_pr(&[])
            } else {
                open_pr(&["me"])
            })
        };
        c.refresh(Some(&baton()), &tasks, false, &pr);
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 12);
        assert_eq!(
            c.lookup(&tasks[0]),
            Some(Verdict::Waiting("review requested".into()))
        );
        assert_eq!(
            c.lookup(&tasks[2]),
            Some(Verdict::Waiting("no review submitted yet".into()))
        );
        // Fresh: the next refresh asks nothing at all.
        c.refresh(Some(&baton()), &tasks, false, &pr);
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 12);
    }

    /// GitHub unreachable: every review is unknown (never not-waiting), and the failure is
    /// remembered rather than retried on the next refresh.
    #[test]
    fn unreachable_github_reads_as_unknown() {
        let c = ReviewCache::default();
        let tasks = vec![rt("a", 1), rt("b", 2)];
        let pr = |_: &str| Err("error connecting to api.github.com".to_string());
        c.refresh(Some(&baton()), &tasks, false, &pr);
        for t in &tasks {
            assert_eq!(
                c.lookup(t),
                Some(Verdict::Unknown(
                    "PR unreadable (error connecting to api.github.com)".into()
                ))
            );
        }
        // The login was never asked for (that would be a gh call), and nothing re-fires at once.
        assert!(c.login.lock().unwrap().is_none());
        c.refresh(Some(&baton()), &tasks, false, &pr);
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 1);
    }

    /// One deleted PR: only its task is unknown, the rest rank normally.
    #[test]
    fn one_unreadable_pr_spoils_only_its_task() {
        let c = ReviewCache::default();
        c.set_login(Some("me".into()));
        let tasks = vec![rt("ok", 1), rt("gone", 2)];
        c.refresh(Some(&baton()), &tasks, false, &|u: &str| {
            if u.ends_with("/2") {
                Err("Could not resolve to a PullRequest".into())
            } else {
                Ok(open_pr(&["me"]))
            }
        });
        assert_eq!(
            c.lookup(&tasks[0]),
            Some(Verdict::Waiting("review requested".into()))
        );
        assert_eq!(
            c.lookup(&tasks[1]),
            Some(Verdict::Unknown(
                "PR unreadable (Could not resolve to a PullRequest)".into()
            ))
        );
    }

    /// The whole rank call failing makes every review unknown, never not-waiting.
    #[test]
    fn failed_rank_call_reads_as_unknown() {
        let c = ReviewCache::default();
        c.set_login(Some("me".into()));
        let t = rt("a", 1);
        c.refresh(
            Some(Path::new("/nonexistent")),
            std::slice::from_ref(&t),
            false,
            &|_| Ok(open_pr(&[])),
        );
        assert!(
            matches!(c.lookup(&t), Some(Verdict::Unknown(w)) if w.starts_with("lookup failed"))
        );
    }

    /// The real script: an open leaf with no URL waits; a closed one does not.
    #[test]
    fn real_rank_script_contract() {
        let t = Task {
            id: "r".into(),
            status: "in_progress".into(),
            labels: vec![LABEL.into()],
            description: "Review (no URL recorded)".into(),
            ..Default::default()
        };
        let closed = Task {
            id: "s".into(),
            status: "closed".into(),
            ..t.clone()
        };
        let m = rank(&baton(), &[&t, &closed], None, &serde_json::json!({})).unwrap();
        assert_eq!(m["r"], Verdict::Waiting("leaf open, no PR URL".into()));
        assert_eq!(m["s"], Verdict::NotWaiting);
    }
}
