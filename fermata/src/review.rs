//! Review waiting on you: asked of baton's `pr-review.sh rank`, the one place that rule lives.
//! One rank call per context per refresh, fed fermata's own PR cache (`--pr-data`) and a login
//! resolved once, so a warm refresh makes no GitHub calls at all.
use crate::fetch::{FailClass, Policy};
use crate::log::Throttle;
use crate::model::{Stale, Task};
use crate::util::run_cmd;
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(60);
/// A failed or unknown answer is asked again sooner, but not on the very next refresh; doubling
/// on repeats up to `TTL`.
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

/// One PR's review data and whether it is stale, or why it could not be read.
pub type PrRead = Result<(Value, Option<Stale>), String>;

/// A known verdict (waiting or not), and the input it was ranked from if that was stale.
#[derive(Clone)]
struct Known {
    verdict: Verdict,
    at: Instant,
    as_of: String,
    stale: Option<Stale>,
}

#[derive(Clone)]
struct Failed {
    why: String,
    at: Instant,
    streak: u32,
}

/// The last known verdict and, when the latest answer was unknown, that.
#[derive(Clone, Default)]
struct Entry {
    known: Option<Known>,
    failed: Option<Failed>,
}

/// Rank's TTLs in `fetch.rs` terms: an unknown answer is a transient failure.
fn policy() -> Policy {
    Policy {
        ok: TTL,
        not_found: FAIL_TTL,
        transient: FAIL_TTL,
    }
}

/// One rank in flight per set of review tasks; `done` counts finished ranks.
#[derive(Default)]
struct Flight {
    lock: Mutex<()>,
    done: AtomicUsize,
}

#[derive(Default)]
pub struct ReviewCache {
    /// Keyed `id|status`, so a closed leaf is asked again at once.
    map: Mutex<HashMap<String, Entry>>,
    /// The login once known; a failure is retried after `LOGIN_RETRY`.
    login: Mutex<Option<(Instant, Option<String>)>>,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    log: Throttle,
    /// `rank` processes started and log lines written, for tests.
    pub rank_calls: AtomicUsize,
    pub logged: AtomicUsize,
}

fn key(t: &Task) -> String {
    format!("{}|{}", t.id, t.status)
}

fn fresh(e: &Entry) -> bool {
    match (&e.failed, &e.known) {
        (Some(f), _) => f.at.elapsed() < policy().fail_ttl(FailClass::Transient, f.streak),
        (None, Some(k)) => k.at.elapsed() < TTL,
        (None, None) => false,
    }
}

impl ReviewCache {
    fn emit(&self, line: Option<String>) {
        if let Some(l) = line {
            self.logged.fetch_add(1, Ordering::Relaxed);
            crate::log::warn(l);
        }
    }

    /// The verdict for `task` (any age) and, if it is a last known one served after a failed
    /// lookup or was ranked from stale PR data, why. None for non-review tasks or before the
    /// first rank; Unknown only when there was never a known verdict.
    pub fn lookup(&self, task: &Task) -> Option<(Verdict, Option<Stale>)> {
        if !task.has_label(LABEL) {
            return None;
        }
        let m = self.map.lock().unwrap();
        let e = m.get(&key(task))?;
        Some(match (&e.known, &e.failed) {
            (Some(k), Some(f)) => (
                k.verdict.clone(),
                Some(Stale {
                    as_of: k.as_of.clone(),
                    error: f.why.clone(),
                }),
            ),
            (Some(k), None) => (k.verdict.clone(), k.stale.clone()),
            (None, Some(f)) => (Verdict::Unknown(f.why.clone()), None),
            (None, None) => return None,
        })
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
        let l = match gh_login() {
            Ok(l) => {
                self.emit(self.log.ok("login"));
                Some(l)
            }
            Err(e) => {
                let first = format!("review: login unknown: {e}");
                self.emit(
                    self.log
                        .hit("login", "review: login unknown", "login", first),
                );
                None
            }
        };
        self.set_login(l.clone());
        l
    }

    fn flight(&self, review: &[&Task]) -> Arc<Flight> {
        let mut ids: Vec<&str> = review.iter().map(|t| t.id.as_str()).collect();
        ids.sort_unstable();
        self.flights
            .lock()
            .unwrap()
            .entry(ids.join(","))
            .or_default()
            .clone()
    }

    /// Rank every `pr-review` task in `tasks` in one call, unless all are fresh or a rank of the
    /// same tasks finishes while this one waits for it. `pr` reads one PR's `{state,
    /// reviewRequests, latestReviews}` (fermata's PR cache) and whether that is stale; a failure
    /// there makes only that task unknown. A failed rank call makes them all unknown. Either way
    /// a task with a known verdict keeps it, marked stale. Returns the rank call's outcome, or
    /// None when no call was made.
    pub fn refresh(
        &self,
        baton: Option<&Path>,
        tasks: &[Task],
        force: bool,
        pr: &(dyn Fn(&str) -> PrRead + Sync),
    ) -> Option<Result<(), String>> {
        let review: Vec<&Task> = tasks.iter().filter(|t| t.has_label(LABEL)).collect();
        if review.is_empty() {
            return None;
        }
        let flight = self.flight(&review);
        let seen = flight.done.load(Ordering::SeqCst);
        let _running = flight.lock.lock().unwrap();
        if flight.done.load(Ordering::SeqCst) != seen {
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
        let reads: Vec<(String, PrRead)> = std::thread::scope(|sc| {
            let hs: Vec<_> = urls
                .iter()
                .map(|u| sc.spawn(move || (u.clone(), pr(u))))
                .collect();
            hs.into_iter().filter_map(|h| h.join().ok()).collect()
        });
        let any_read = reads.iter().any(|(_, r)| r.is_ok());
        let mut data = Map::new();
        let mut stale_input: HashMap<String, Stale> = HashMap::new();
        for (u, r) in reads {
            let v = match r {
                Ok((v, st)) => {
                    if let Some(st) = st {
                        stale_input.insert(u.clone(), st);
                    }
                    v
                }
                Err(e) => serde_json::json!({ "error": e }),
            };
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
        let as_of = chrono::Utc::now().to_rfc3339();
        let outcome = verdicts.as_ref().map(|_| ()).map_err(|e| e.to_string());
        let mut lines = vec![];
        {
            let mut m = self.map.lock().unwrap();
            let mut record = |t: &Task, vd: Verdict| {
                let e = m.entry(key(t)).or_default();
                match vd {
                    Verdict::Unknown(why) => {
                        let streak = e.failed.as_ref().map_or(1, |f| f.streak + 1);
                        e.failed = Some(Failed {
                            why,
                            at: now,
                            streak,
                        });
                    }
                    vd => {
                        let stale = pr_url(t).and_then(|u| stale_input.get(&u).cloned());
                        e.known = Some(Known {
                            verdict: vd,
                            at: now,
                            as_of: stale.as_ref().map_or(as_of.clone(), |s| s.as_of.clone()),
                            stale,
                        });
                        e.failed = None;
                    }
                }
            };
            match verdicts {
                Ok(v) => {
                    for t in &review {
                        let vd = v.get(&t.id).cloned().unwrap_or_else(|| {
                            Verdict::Unknown(format!("{} missing from rank output", t.id))
                        });
                        match &vd {
                            Verdict::Unknown(why) => {
                                let w = why.replace(&t.id, "{id}");
                                let first = format!("review status unknown: {} ({why})", t.id);
                                let what = format!("review status unknown ({w})");
                                lines.push(self.log.hit(
                                    &format!("unknown|{w}"),
                                    &what,
                                    &t.id,
                                    first,
                                ));
                            }
                            _ => lines.push(self.log.ok(&t.id)),
                        }
                        record(t, vd);
                    }
                }
                Err(e) => {
                    let why = format!("lookup failed: {e}");
                    let first = format!("review rank, {} task(s): {why}", review.len());
                    let what = format!("review rank: {why}");
                    for t in &review {
                        lines.push(self.log.hit(
                            &format!("rank|{why}"),
                            &what,
                            &t.id,
                            first.clone(),
                        ));
                        record(t, Verdict::Unknown(why.clone()));
                    }
                }
            }
        }
        for l in lines {
            self.emit(l);
        }
        flight.done.fetch_add(1, Ordering::SeqCst);
        Some(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn verdict(c: &ReviewCache, t: &Task) -> Option<Verdict> {
        c.lookup(t).map(|(v, _)| v)
    }

    /// Pretend every entry was recorded `secs` earlier.
    fn age(c: &ReviewCache, secs: u64) {
        let d = Duration::from_secs(secs);
        for e in c.map.lock().unwrap().values_mut() {
            if let Some(k) = e.known.as_mut() {
                k.at -= d;
            }
            if let Some(f) = e.failed.as_mut() {
                f.at -= d;
            }
        }
    }

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
        assert_eq!(verdict(&c, &t), None);
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
            Ok((
                if u.ends_with("/3") {
                    open_pr(&[])
                } else {
                    open_pr(&["me"])
                },
                None,
            ))
        };
        c.refresh(Some(&baton()), &tasks, false, &pr);
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 1);
        assert_eq!(reads.load(Ordering::Relaxed), 12);
        assert_eq!(
            verdict(&c, &tasks[0]),
            Some(Verdict::Waiting("review requested".into()))
        );
        assert_eq!(
            verdict(&c, &tasks[2]),
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
                verdict(&c, t),
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
                Ok((open_pr(&["me"]), None))
            }
        });
        assert_eq!(
            verdict(&c, &tasks[0]),
            Some(Verdict::Waiting("review requested".into()))
        );
        assert_eq!(
            verdict(&c, &tasks[1]),
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
            &|_| Ok((open_pr(&[]), None)),
        );
        assert!(
            matches!(verdict(&c, &t), Some(Verdict::Unknown(w)) if w.starts_with("lookup failed"))
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

    /// A known verdict survives a later failed lookup, marked stale; unknown only when never known.
    #[test]
    fn failed_lookup_keeps_the_last_verdict_stale() {
        let c = ReviewCache::default();
        c.set_login(Some("me".into()));
        let t = rt("a", 1);
        let ts = std::slice::from_ref(&t);
        c.refresh(Some(&baton()), ts, false, &|_| Ok((open_pr(&["me"]), None)));
        assert_eq!(
            c.lookup(&t),
            Some((Verdict::Waiting("review requested".into()), None))
        );
        age(&c, 61);
        c.refresh(Some(Path::new("/nonexistent")), ts, false, &|_| {
            Ok((open_pr(&["me"]), None))
        });
        let (v, st) = c.lookup(&t).unwrap();
        assert_eq!(v, Verdict::Waiting("review requested".into()));
        assert!(st.unwrap().error.starts_with("lookup failed"));
        // Recovery clears it.
        age(&c, 61);
        c.refresh(Some(&baton()), ts, false, &|_| Ok((open_pr(&["me"]), None)));
        assert_eq!(c.lookup(&t).unwrap().1, None);
    }

    /// A verdict ranked from a stale PR read carries that staleness.
    #[test]
    fn stale_pr_data_marks_the_verdict_stale() {
        let c = ReviewCache::default();
        c.set_login(Some("me".into()));
        let t = rt("a", 1);
        let st = Stale {
            as_of: "2026-01-01T14:02:00Z".into(),
            error: "gh: offline".into(),
        };
        c.refresh(Some(&baton()), std::slice::from_ref(&t), false, &|_| {
            Ok((open_pr(&["me"]), Some(st.clone())))
        });
        assert_eq!(
            c.lookup(&t),
            Some((
                Verdict::Waiting("review requested".into()),
                Some(st.clone())
            ))
        );
    }

    /// Unknown answers back off: 20 s, then 40 s.
    #[test]
    fn unknown_backs_off() {
        let c = ReviewCache::default();
        let ts = vec![rt("a", 1)];
        let bad = |_: &str| Err::<(Value, Option<Stale>), _>("offline".to_string());
        c.refresh(Some(&baton()), &ts, false, &bad);
        age(&c, 21);
        c.refresh(Some(&baton()), &ts, false, &bad);
        age(&c, 21);
        c.refresh(Some(&baton()), &ts, false, &bad);
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 2);
        // Repeated failures log once.
        assert_eq!(c.logged.load(Ordering::Relaxed), 1);
    }

    /// Concurrent refreshes of the same review tasks share one rank.
    #[test]
    fn concurrent_refreshes_share_one_rank() {
        let c = ReviewCache::default();
        c.set_login(Some("me".into()));
        let ts: Vec<Task> = (1..=3).map(|i| rt(&format!("r{i}"), i)).collect();
        let pr = |_: &str| {
            std::thread::sleep(Duration::from_millis(150));
            Ok((open_pr(&["me"]), None))
        };
        std::thread::scope(|sc| {
            for i in 0..4 {
                let (c, ts, pr) = (&c, &ts, &pr);
                sc.spawn(move || c.refresh(Some(&baton()), ts, i == 0, pr));
            }
        });
        assert_eq!(c.rank_calls.load(Ordering::Relaxed), 1);
    }
}
