//! Load and join: contexts → tasks → branches, ticket, PR, progress, sessions, workers.
use crate::adapter::{self, TicketCache};
use crate::baton::{self, ShellTracker, Tracker};
use crate::cache::DiskCache;
use crate::config::{self, Context};
use crate::fetch::{Fetch, Lookup};
use crate::health::{self, Health, Source};
use crate::log;
use crate::model::Branch;
use crate::model::{Badge, JoinedTask, PrInfo, Session, Stale, Task};
use crate::notes::task_decisions;
use crate::pr::PrCache;
use crate::review::{ReviewCache, Verdict};
use crate::sessions::{self, WorkerMeta};
use crate::signature::signature;
use anyhow::Result;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type TrackerFactory = dyn Fn(&Context) -> Arc<dyn Tracker> + Send + Sync;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Snapshot {
    pub contexts: Vec<Context>,
    pub tasks: Vec<JoinedTask>,
    pub sessions: Vec<Session>,
    /// Who you are per context (`tracker.sh whoami`); a context missing here is unknown.
    pub me: HashMap<String, String>,
    pub errors: Vec<String>,
    pub loaded_at: String,
    /// Each external source's health as of the end of this load.
    pub health: Vec<health::Row>,
    /// Contexts whose task list is the last good one, served after a failed `list`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tracker_stale: BTreeMap<String, Stale>,
    /// Stateless backlog per context: active tasks not shown as rows (see `in_flight`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub backlog: BTreeMap<String, usize>,
    /// When to retry the soonest failed `list` (seconds); None when nothing failed.
    #[serde(skip)]
    pub retry_in_s: Option<u64>,
}

/// Parked tasks' `list-branches` reads in flight at once; each holds the tracker's lock.
const PARKED_READS: usize = 4;

/// Wait before retrying a failed `list`: 15 s, doubling per consecutive failure, at most 5 min.
pub fn list_retry_s(streak: u32) -> u64 {
    (15u64 << streak.saturating_sub(1).min(5)).min(300)
}

pub struct Loader {
    pub baton: Option<PathBuf>,
    pub tickets: TicketCache,
    pub prs: PrCache,
    pub reviews: ReviewCache,
    ctx_json: Mutex<HashMap<PathBuf, String>>,
    factory: Box<TrackerFactory>,
    pub fetch_prs: bool,
    pub disk: Option<DiskCache>,
    /// Tracker write signature per context, as of the last tracker read.
    pub sigs: Mutex<HashMap<String, String>>,
    pub health: Arc<Health>,
    /// Ask tmux for its panes on each load, for health. Off in tests.
    pub probe_tmux: bool,
    /// Each context's last good tracker state and when it was read, served after a failed list.
    last_good: Mutex<HashMap<String, (TrackerState, chrono::DateTime<chrono::Utc>)>>,
    /// Consecutive failed `list` reads per context, for the retry backoff.
    fails: Mutex<HashMap<String, u32>>,
    /// Last good registry entries per context and task, loaded lazily from disk.
    registry: Mutex<HashMap<String, HashMap<String, Vec<Branch>>>>,
}

/// In-progress tasks, cached on disk per tracker write signature. Their worktrees come from
/// the directory stores (`worktrees.rs`), not the tracker.
#[derive(Serialize, serde::Deserialize, Default, Clone)]
struct TrackerState {
    tasks: Vec<Task>,
}

/// The active tasks shown as rows: in progress, a declared state, an open decision label, or
/// the adapter's ticket label. The rest are counted as backlog.
fn in_flight(c: &Context, tasks: &[Task]) -> (Vec<Task>, usize) {
    let tcfg = c.adapter.as_ref().and_then(|a| a.ticket.as_ref());
    let (shown, rest): (Vec<&Task>, Vec<&Task>) = tasks.iter().partition(|t| {
        t.status == "in_progress"
            || t.state.is_some()
            || t.has_label("needs-decision")
            || tcfg.is_some_and(|cfg| adapter::ticket_key(cfg, &t.labels).is_some())
    });
    (shown.into_iter().cloned().collect(), rest.len())
}

/// What the cached tracker state holds, in its disk key: an older list (`in_progress` only)
/// is then refetched rather than served under the same write signature.
const LISTED: &str = "active";

/// Every non-closed task; a backend that refuses `active` is asked for `in_progress`.
fn list_active(c: &Context, tr: &dyn Tracker) -> Result<Vec<Task>> {
    tr.list("active").or_else(|e| {
        let r = tr.list("in_progress");
        if r.is_ok() {
            log::warn(format!(
                "{}: tracker refused list --status active, showing in_progress only: {e:#}",
                c.name
            ));
        }
        r
    })
}

/// Registry entries first, then local worktrees, with a review's PR number adopting its URL.
fn branches_for(t: &Task, registry: Vec<Branch>, local: Option<&Vec<Branch>>) -> Vec<Branch> {
    let mut b = crate::worktrees::merge(registry, local.cloned().unwrap_or_default());
    crate::worktrees::adopt_pr_url(&mut b, crate::review::pr_url(t).as_deref());
    b
}

/// Parked tasks whose registry entries are read: `keep-task-open`, and not somebody else's
/// (assigned to `me`, unassigned, an unknown `me`, or posing a decision).
fn parked_ids(tasks: &[Task], me: Option<&str>) -> Vec<String> {
    let norm = |s: &str| s.trim().to_lowercase();
    tasks
        .iter()
        .filter(|t| t.has_label("keep-task-open"))
        .filter(|t| match (me, t.assignee.as_deref()) {
            (Some(m), Some(a)) => norm(a) == norm(m) || t.has_label("needs-decision"),
            _ => true,
        })
        .map(|t| t.id.clone())
        .collect()
}

fn mtime(p: &std::path::Path) -> i64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(|t| chrono::DateTime::<chrono::Utc>::from(t).timestamp_millis())
        .unwrap_or(0)
}

impl Loader {
    pub fn new() -> Self {
        let baton = baton::locate_baton();
        let b2 = baton.clone();
        let mut l = Self::with_factory(
            baton,
            Box::new(move |c: &Context| -> Arc<dyn Tracker> {
                let b = b2.clone().unwrap_or_default();
                Arc::new(ShellTracker::new(&b, c.ctx_json.clone()))
            }),
        );
        let d = DiskCache::new(DiskCache::default_dir());
        l.tickets.cache.disk = Some(d.clone());
        l.prs.cache.disk = Some(d.clone());
        l.disk = Some(d);
        l.probe_tmux = true;
        l
    }

    pub fn with_factory(baton: Option<PathBuf>, factory: Box<TrackerFactory>) -> Self {
        let health = Arc::new(Health::default());
        let tickets = TicketCache {
            health: Some(health.clone()),
            ..Default::default()
        };
        let prs = PrCache {
            health: Some(health.clone()),
            ..Default::default()
        };
        Self {
            baton,
            tickets,
            prs,
            reviews: ReviewCache::default(),
            ctx_json: Mutex::new(HashMap::new()),
            factory,
            fetch_prs: true,
            disk: None,
            sigs: Mutex::new(HashMap::new()),
            health,
            probe_tmux: false,
            last_good: Mutex::new(HashMap::new()),
            fails: Mutex::new(HashMap::new()),
            registry: Mutex::new(HashMap::new()),
        }
    }

    pub fn tracker(&self, c: &Context) -> Arc<dyn Tracker> {
        (self.factory)(c)
    }

    pub fn contexts(&self, errors: &mut Vec<String>) -> Vec<Context> {
        let ws = match config::workspaces() {
            Ok(w) => w,
            Err(e) => {
                errors.push(format!("registry: {e}"));
                return vec![];
            }
        };
        let results: Vec<Result<Context>> = std::thread::scope(|sc| {
            let hs: Vec<_> = ws
                .iter()
                .map(|w| sc.spawn(move || self.context_for(w)))
                .collect();
            hs.into_iter()
                .map(|h| h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("panic"))))
                .collect()
        });
        let mut out = vec![];
        for (w, r) in ws.iter().zip(results) {
            match r {
                Ok(c) => {
                    if let Some(e) = &c.adapter_error {
                        errors.push(e.clone());
                    }
                    self.health.expect(&c.name, Source::Tracker);
                    if c.adapter
                        .as_ref()
                        .and_then(|a| a.ticket.as_ref())
                        .is_some_and(|t| !t.info.is_empty())
                    {
                        self.health.expect(&c.name, Source::Tickets);
                    }
                    out.push(c);
                }
                Err(e) => errors.push(format!("{}: {e}", w.display())),
            }
        }
        out
    }

    fn context_for(&self, w: &std::path::Path) -> Result<Context> {
        let mut c = config::load_context(w)?;
        let cached = self.ctx_json.lock().unwrap().get(w).cloned();
        if let Some(j) = cached {
            c.ctx_json = j;
            return Ok(c);
        }
        let key = format!(
            "{}|{}|{}",
            self.baton
                .as_deref()
                .unwrap_or(std::path::Path::new(""))
                .display(),
            mtime(&w.join("context.yaml")),
            mtime(&config::registry_path())
        );
        let dname = format!("ctx-{}", c.name);
        let disk = self
            .disk
            .as_ref()
            .and_then(|d| d.get::<String>(&dname, &key, None));
        let j = match disk {
            Some(j) => j,
            None => {
                let j = self
                    .baton
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("baton not found"))
                    .and_then(|b| baton::resolve_context(b, &c.name));
                self.health
                    .record(&c.name, Source::Tracker, "resolve-context", &j);
                match j {
                    Ok(j) => {
                        if let Some(d) = &self.disk {
                            d.put(&dname, &key, &j);
                        }
                        j
                    }
                    Err(e) => {
                        log::warn(format!("resolve-context {}: {e}; using yaml", c.name));
                        config::yaml_to_ctx_json(w).unwrap_or_default()
                    }
                }
            }
        };
        self.ctx_json
            .lock()
            .unwrap()
            .insert(w.to_path_buf(), j.clone());
        c.ctx_json = j;
        Ok(c)
    }

    /// Current write signatures of every context's tracker dir.
    pub fn current_sigs(contexts: &[Context]) -> HashMap<String, String> {
        contexts
            .iter()
            .filter_map(|c| Some((c.name.clone(), signature(c.tracker_dir.as_deref()?)?)))
            .collect()
    }

    /// In-progress tasks; served from disk while the signature is unchanged.
    #[cfg(test)]
    fn tracker_state(
        &self,
        c: &Context,
        tr: &dyn Tracker,
        force: bool,
    ) -> Result<(TrackerState, Option<Stale>)> {
        self.tracker_state_with(c, tr, force, &|_| {})
    }

    /// The last good tracker state and when it was read: in memory, else on disk at any age.
    fn last_good_state(
        &self,
        c: &Context,
    ) -> Option<(TrackerState, chrono::DateTime<chrono::Utc>)> {
        if let Some(g) = self.last_good.lock().unwrap().get(&c.name) {
            return Some(g.clone());
        }
        let d = self.disk.as_ref()?;
        let dname = format!("tracker-{}", c.name);
        Some((d.get_any(&dname)?, d.saved_at(&dname)?))
    }

    /// As `tracker_state`; on a cache miss `warm` runs on the previous (stale) state while the
    /// tracker is read, so ticket and PR lookups overlap the slow tracker calls. A failed read
    /// serves the last good state with why it is stale; it errors only when there was none.
    fn tracker_state_with(
        &self,
        c: &Context,
        tr: &dyn Tracker,
        force: bool,
        warm: &(dyn Fn(&TrackerState) + Sync),
    ) -> Result<(TrackerState, Option<Stale>)> {
        let sig = c.tracker_dir.as_deref().and_then(signature);
        let dname = format!("tracker-{}", c.name);
        if let (Some(sig), Some(d), false) = (&sig, &self.disk, force)
            && let Some(st) = d.get::<TrackerState>(&dname, &format!("{LISTED}:{sig}"), None)
        {
            if let Some(at) = d.saved_at(&dname) {
                self.health.cached(&c.name, Source::Tracker, at);
            }
            self.sigs
                .lock()
                .unwrap()
                .insert(c.name.clone(), sig.clone());
            self.fails.lock().unwrap().remove(&c.name);
            return Ok((st, None));
        }
        let prev = self.last_good_state(c);
        // Warming overlaps the tracker call; the scope ends once both are done.
        let tasks = std::thread::scope(|sc| {
            if let Some((st, _)) = &prev {
                sc.spawn(move || warm(st));
            }
            let tasks = list_active(c, tr);
            self.health.record(&c.name, Source::Tracker, "list", &tasks);
            tasks
        });
        // Even a failure takes the signature, so only a write or the retry timer reloads.
        if let Some(sig) = &sig {
            self.sigs
                .lock()
                .unwrap()
                .insert(c.name.clone(), sig.clone());
        }
        let tasks = match tasks {
            Ok(t) => t,
            Err(e) => {
                *self
                    .fails
                    .lock()
                    .unwrap()
                    .entry(c.name.clone())
                    .or_default() += 1;
                let Some((st, at)) = prev else { return Err(e) };
                log::warn(format!(
                    "{}: tracker list failed; serving tasks as of {at}: {e}",
                    c.name
                ));
                self.health.cached(&c.name, Source::Tracker, at);
                let stale = Stale {
                    as_of: at.to_rfc3339(),
                    error: format!("{e:#}"),
                };
                return Ok((st, Some(stale)));
            }
        };
        let mut tasks = tasks;
        self.read_decisions(c, tr, &mut tasks, prev.as_ref().map(|p| &p.0));
        let st = TrackerState { tasks };
        if let (Some(sig), Some(d)) = (sig, &self.disk) {
            d.put(&dname, &format!("{LISTED}:{sig}"), &st);
        }
        self.fails.lock().unwrap().remove(&c.name);
        self.last_good
            .lock()
            .unwrap()
            .insert(c.name.clone(), (st.clone(), chrono::Utc::now()));
        Ok((st, None))
    }

    /// Read the decisions of tasks labelled `needs-decision`, side by side. A failed read keeps
    /// the task's last good ones from `prev`, else leaves them unread (`None`).
    fn read_decisions(
        &self,
        c: &Context,
        tr: &dyn Tracker,
        tasks: &mut [Task],
        prev: Option<&TrackerState>,
    ) {
        std::thread::scope(|sc| {
            let hs: Vec<_> = tasks
                .iter_mut()
                .filter(|t| t.has_label("needs-decision"))
                .map(|t| {
                    let id = t.id.clone();
                    (t, sc.spawn(move || tr.decisions(&id)))
                })
                .collect();
            for (t, h) in hs {
                let r = h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("panic")));
                // A backend without decisions is not degraded: the legacy notes still read.
                if !r
                    .as_ref()
                    .is_err_and(|e| e.to_string().starts_with("unsupported verb"))
                {
                    self.health
                        .record(&c.name, Source::Tracker, "decisions", &r);
                }
                t.decisions = match r {
                    Ok(d) => Some(d),
                    Err(e) => {
                        log::warn(format!("{}: decisions {}: {e}", c.name, t.id));
                        prev.and_then(|p| p.tasks.iter().find(|o| o.id == t.id))
                            .and_then(|o| o.decisions.clone())
                    }
                };
            }
        });
    }

    /// Run `f` on the context's last good registry entries, loading them from disk first.
    fn with_registry<R>(
        &self,
        ctx: &str,
        f: impl FnOnce(&mut HashMap<String, Vec<Branch>>) -> R,
    ) -> R {
        let mut all = self.registry.lock().unwrap();
        let m = all.entry(ctx.to_string()).or_insert_with(|| {
            self.disk
                .as_ref()
                .and_then(|d| d.get_any(&format!("registry-{ctx}")))
                .unwrap_or_default()
        });
        f(m)
    }

    /// Keep successful registry reads as the last good ones; `live` (when given) prunes tasks
    /// that are no longer in progress.
    fn keep_registry(&self, ctx: &str, ok: &HashMap<String, Vec<Branch>>, live: Option<&[Task]>) {
        let snapshot = self.with_registry(ctx, |m| {
            m.extend(ok.iter().map(|(k, v)| (k.clone(), v.clone())));
            if let Some(tasks) = live {
                m.retain(|id, _| tasks.iter().any(|t| &t.id == id));
            }
            m.clone()
        });
        if let Some(d) = &self.disk {
            d.put(&format!("registry-{ctx}"), "any", &snapshot);
        }
    }

    /// A task's last good registry entries, or none if it was never read.
    fn prev_registry(&self, ctx: &str, id: &str) -> Vec<Branch> {
        self.with_registry(ctx, |m| m.get(id).cloned().unwrap_or_default())
    }

    /// One task's registry entries; a failed read keeps the last good ones.
    fn registry_of(&self, c: &Context, tr: &dyn Tracker, id: &str) -> Vec<Branch> {
        let reg = tr.list_branches(id);
        self.health
            .record(&c.name, Source::Tracker, "list-branches", &reg);
        match reg {
            Ok(b) => {
                self.keep_registry(&c.name, &HashMap::from([(id.to_string(), b.clone())]), None);
                b
            }
            Err(e) => {
                log::warn(format!(
                    "list-branches {id}: {e}; keeping the last good entries"
                ));
                self.prev_registry(&c.name, id)
            }
        }
    }

    /// Registry entries (`list-branches`) of the parked tasks you see, for `waiting_on`/`until`;
    /// one call per such task, cached per tracker signature and `me`. Never one per task.
    fn parked_registry(
        &self,
        c: &Context,
        tr: &dyn Tracker,
        tasks: &[Task],
        me: Option<&str>,
        force: bool,
    ) -> HashMap<String, Vec<Branch>> {
        let ids = parked_ids(tasks, me);
        if ids.is_empty() {
            return HashMap::new();
        }
        let sig = c.tracker_dir.as_deref().and_then(signature);
        let dname = format!("parked-{}", c.name);
        let key = sig.map(|s| format!("{s}|{}|{}", me.unwrap_or(""), ids.join(",")));
        if let (Some(k), Some(d), false) = (&key, &self.disk, force)
            && let Some(m) = d.get::<HashMap<String, Vec<Branch>>>(&dname, k, None)
        {
            return m;
        }
        let (ok, failed) = (Mutex::new(HashMap::new()), Mutex::new(vec![]));
        let next = std::sync::atomic::AtomicUsize::new(0);
        // A small pool: each call holds the tracker's lock, so more at once only queue.
        std::thread::scope(|sc| {
            for _ in 0..PARKED_READS.min(ids.len()) {
                sc.spawn(|| {
                    while let Some(id) =
                        ids.get(next.fetch_add(1, std::sync::atomic::Ordering::SeqCst))
                    {
                        match tr.list_branches(id) {
                            Ok(b) => drop(ok.lock().unwrap().insert(id.clone(), b)),
                            Err(e) => {
                                log::warn(format!("list-branches {id}: {e}"));
                                failed
                                    .lock()
                                    .unwrap()
                                    .push((id.clone(), format!("{id}: {e}")));
                            }
                        }
                    }
                });
            }
        });
        let ok = ok.into_inner().unwrap();
        self.keep_registry(&c.name, &ok, Some(tasks));
        let failed = failed.into_inner().unwrap();
        // A failed read keeps the task's last good entries rather than none.
        let mut m = ok;
        for (id, _) in &failed {
            m.insert(id.clone(), self.prev_registry(&c.name, id));
        }
        let failed: Vec<String> = failed.into_iter().map(|(_, e)| e).collect();
        // One outcome per read, so a finished task cannot leave a failure behind.
        let r = match failed.first() {
            Some(e) => Err(format!("{} of {} failed; {e}", failed.len(), ids.len())),
            None => Ok(()),
        };
        self.health
            .record(&c.name, Source::Tracker, "list-branches", &r);
        if let (Some(k), Some(d), true) = (&key, &self.disk, failed.is_empty()) {
            d.put(&dname, k, &m);
        }
        m
    }

    /// Who `claim` assigns to in this context. Cached on disk for an hour (an unknown answer
    /// too, so a backend without `whoami` is not asked on every load); `force` bypasses it.
    fn me_for(&self, c: &Context, tr: &dyn Tracker, force: bool) -> Option<String> {
        let dname = format!("me-{}", c.name);
        if !force
            && let Some(d) = &self.disk
            && let Some(me) = d.get::<Option<String>>(&dname, "whoami", Some(3600))
        {
            return me;
        }
        let me = tr.whoami();
        self.health.record(&c.name, Source::Tracker, "whoami", &me);
        let me = me.unwrap_or_else(|e| {
            log::warn(format!("{}: whoami: {e}", c.name));
            None
        });
        if let Some(d) = &self.disk {
            d.put(&dname, "whoami", &me);
        }
        me
    }

    pub fn forget_contexts(&self) {
        self.ctx_json.lock().unwrap().clear();
    }

    /// Full load. `force` bypasses the tracker, ticket and PR caches.
    pub async fn load(self: &Arc<Self>, force: bool) -> Snapshot {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.load_blocking(force))
            .await
            .unwrap_or_default()
    }

    pub fn load_blocking(&self, force: bool) -> Snapshot {
        let mut errors = vec![];
        let mut me = HashMap::new();
        let contexts = self.contexts(&mut errors);
        let (live, workers) = std::thread::scope(|sc| {
            let w = sc.spawn(|| sessions::scan_workers(&sessions::projects_dir()));
            if self.probe_tmux {
                sc.spawn(|| {
                    let t = crate::panes::real_table(std::time::Duration::from_secs(5));
                    self.health.record("", Source::Tmux, "list-panes", &t);
                });
            }
            let probe = sessions::probe_sessions();
            self.health.record("", Source::Sessions, "sessions", &probe);
            (sessions::live_sessions(), w.join().unwrap_or_default())
        });
        if self.fetch_prs {
            self.health.expect("", Source::Gh);
        }
        let mut tasks: Vec<JoinedTask> = vec![];
        let mut tracker_stale = BTreeMap::new();
        let mut backlog = BTreeMap::new();
        std::thread::scope(|sc| {
            let per_ctx: Vec<_> = contexts
                .iter()
                .map(|c| {
                    let (live, workers) = (&live, &workers);
                    sc.spawn(
                        move || -> (Result<Vec<JoinedTask>>, Option<String>, Option<Stale>, usize) {
                            let tr = self.tracker(c);
                            let local = crate::worktrees::local_branches(c);
                            let local = &local;
                            let warm = |old: &TrackerState| {
                                std::thread::scope(|w| {
                                    for t in &old.tasks {
                                        let b = branches_for(t, vec![], local.get(&t.id));
                                        w.spawn(move || {
                                            self.join(c, t.clone(), b, live, workers, force)
                                        });
                                    }
                                });
                            };
                            let (st, me) = std::thread::scope(|w| {
                                let me = w.spawn(|| self.me_for(c, &*tr, force));
                                let st = self.tracker_state_with(c, &*tr, force, &warm);
                                (st, me.join().unwrap_or(None))
                            });
                            // A failed tracker read keeps who you are, so the header stays truthful.
                            let (st, stale) = match st {
                                Ok(st) => st,
                                Err(e) => return (Err(e), me, None, 0),
                            };
                            let (shown, backlog) = in_flight(c, &st.tasks);
                            let st = TrackerState { tasks: shown };
                            let mut reg =
                                self.parked_registry(c, &*tr, &st.tasks, me.as_deref(), force);
                            // One rank call for the context runs beside the joins.
                            let mut joined: Vec<JoinedTask> = std::thread::scope(|sc2| {
                                let r = sc2.spawn(|| self.rank_reviews(&c.name, &st.tasks, force));
                                let hs: Vec<_> = st
                                    .tasks
                                    .iter()
                                    .map(|t| {
                                        let r = reg.remove(&t.id).unwrap_or_default();
                                        let b = branches_for(t, r, local.get(&t.id));
                                        sc2.spawn(move || {
                                            self.join(c, t.clone(), b, live, workers, force)
                                        })
                                    })
                                    .collect();
                                let _ = r.join();
                                hs.into_iter().filter_map(|h| h.join().ok()).collect()
                            });
                            for j in &mut joined {
                                self.apply_review(j);
                            }
                            (Ok(joined), me, stale, backlog)
                        },
                    )
                })
                .collect();
            for (c, h) in contexts.iter().zip(per_ctx) {
                let Ok((r, who, st, backlog_n)) = h.join() else {
                    errors.push(format!("{}: loader panicked", c.name));
                    continue;
                };
                if let Some(who) = who {
                    me.insert(c.name.clone(), who);
                }
                if let Some(st) = st {
                    tracker_stale.insert(c.name.clone(), st);
                }
                if backlog_n > 0 {
                    backlog.insert(c.name.clone(), backlog_n);
                }
                match r {
                    Ok(v) => tasks.extend(v),
                    Err(e) => errors.push(format!("{}: tracker: {e}", c.name)),
                }
            }
        });
        sort_tasks(&mut tasks);
        for e in &errors {
            log::warn(e);
        }
        let retry_in_s = {
            let f = self.fails.lock().unwrap();
            contexts
                .iter()
                .filter_map(|c| f.get(&c.name).map(|n| list_retry_s(*n)))
                .min()
        };
        Snapshot {
            contexts,
            tasks,
            sessions: live,
            me,
            errors,
            loaded_at: chrono::Utc::now().to_rfc3339(),
            health: self.health.rows(),
            tracker_stale,
            backlog,
            retry_in_s,
        }
    }

    /// The cache-only first paint: each context's last good tasks, registry entries and who you
    /// are, joined with whatever ticket, PR and review data is cached. No tracker, adapter or gh
    /// call; contexts with nothing cached are left out.
    pub fn preview_blocking(&self) -> Snapshot {
        let mut errors = vec![];
        let contexts = self.contexts(&mut errors);
        let live = sessions::live_sessions();
        let workers = sessions::scan_workers(&sessions::projects_dir());
        Snapshot {
            errors,
            ..self.preview_of(contexts, live, &workers)
        }
    }

    fn preview_of(
        &self,
        contexts: Vec<Context>,
        live: Vec<Session>,
        workers: &[WorkerMeta],
    ) -> Snapshot {
        let mut me = HashMap::new();
        let mut tasks = vec![];
        let mut backlog = BTreeMap::new();
        std::thread::scope(|sc| {
            let hs: Vec<_> = contexts
                .iter()
                .map(|c| {
                    let live = &live;
                    sc.spawn(move || {
                        let (st, _) = self.last_good_state(c)?;
                        let (shown, backlog) = in_flight(c, &st.tasks);
                        let st = TrackerState { tasks: shown };
                        let who: Option<String> = self
                            .disk
                            .as_ref()
                            .and_then(|d| d.get_any(&format!("me-{}", c.name)))
                            .flatten();
                        let local = crate::worktrees::local_branches(c);
                        let parked = parked_ids(&st.tasks, who.as_deref());
                        let joined: Vec<JoinedTask> = st
                            .tasks
                            .iter()
                            .map(|t| {
                                let r = if parked.contains(&t.id) {
                                    self.prev_registry(&c.name, &t.id)
                                } else {
                                    vec![]
                                };
                                let b = branches_for(t, r, local.get(&t.id));
                                let mut j = self.join(c, t.clone(), b, live, workers, Fetch::Never);
                                self.apply_review(&mut j);
                                j
                            })
                            .collect();
                        Some((c.name.clone(), joined, who, backlog))
                    })
                })
                .collect();
            for (name, joined, who, n) in hs.into_iter().filter_map(|h| h.join().ok().flatten()) {
                tasks.extend(joined);
                if n > 0 {
                    backlog.insert(name.clone(), n);
                }
                if let Some(w) = who {
                    me.insert(name, w);
                }
            }
        });
        sort_tasks(&mut tasks);
        Snapshot {
            contexts,
            tasks,
            sessions: live,
            me,
            loaded_at: String::new(),
            health: self.health.rows(),
            backlog,
            ..Default::default()
        }
    }

    pub async fn preview(self: &Arc<Self>) -> Snapshot {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.preview_blocking())
            .await
            .unwrap_or_default()
    }

    /// Re-read one task (`tracker get`) with its registry entries (`list-branches`) and re-join
    /// it; `force` also bypasses the ticket and PR caches.
    pub fn refresh_one(
        &self,
        c: &Context,
        id: &str,
        live: &[Session],
        force: bool,
    ) -> Result<JoinedTask> {
        let tr = self.tracker(c);
        let mut t = tr.get(id)?;
        let prev = self.last_good_state(c).map(|p| p.0);
        self.read_decisions(c, &*tr, std::slice::from_mut(&mut t), prev.as_ref());
        let reg = self.registry_of(c, &*tr, id);
        let local = crate::worktrees::local_branches(c);
        let branches = branches_for(&t, reg, local.get(id));
        let workers = sessions::scan_workers(&sessions::projects_dir());
        self.rank_reviews(&c.name, std::slice::from_ref(&t), force);
        let mut j = self.join(c, t, branches, live, &workers, Fetch::from(force));
        self.apply_review(&mut j);
        Ok(j)
    }

    /// Rank a context's review tasks in one `pr-review.sh rank` call, its PR data read through
    /// the PR cache (a URL is one cache entry, shared with the task's own PR lookup).
    pub fn rank_reviews(&self, scope: &str, tasks: &[Task], force: bool) {
        if !self.fetch_prs {
            return;
        }
        let pr = |url: &str| -> Result<(serde_json::Value, Option<Stale>), String> {
            let l = self.prs.get(None, url, force);
            match l.value {
                Some(p) => p
                    .review
                    .map(|r| (r, l.stale))
                    .ok_or_else(|| "no review data cached".to_string()),
                None => Err(l.error.unwrap_or_else(|| "no PR data".into())),
            }
        };
        if let Some(r) = self
            .reviews
            .refresh(self.baton.as_deref(), tasks, force, &pr)
        {
            self.health.record(scope, Source::Review, "rank", &r);
        }
    }

    /// Set a joined task's review fields from the last rank, and its badge with them.
    pub fn apply_review(&self, j: &mut JoinedTask) {
        let (verdict, stale) = self.reviews.lookup(&j.task).unzip();
        j.review_stale = stale.flatten();
        (j.review_waiting, j.review_unknown) = match verdict {
            Some(Verdict::Waiting(w)) => (Some(w), None),
            Some(Verdict::Unknown(w)) => (None, Some(w)),
            _ => (None, None),
        };
        j.badge = badge(j);
    }

    pub fn join(
        &self,
        c: &Context,
        task: Task,
        branches: Vec<Branch>,
        live: &[Session],
        workers: &[WorkerMeta],
        fetch: impl Into<Fetch>,
    ) -> JoinedTask {
        let force = fetch.into();
        let ad = c.adapter.as_ref();
        let tcfg = ad.and_then(|a| a.ticket.as_ref());
        let ticket_key = tcfg.and_then(|t| adapter::ticket_key(t, &task.labels));
        let ticket_url = match (tcfg, &ticket_key) {
            (Some(t), Some(k)) => adapter::ticket_url(t, k),
            _ => None,
        };
        type TicketLookup = Lookup<crate::model::TicketInfo, crate::model::TicketError>;
        let fetch_ticket = || -> TicketLookup {
            let (Some(t), Some(k)) = (tcfg, &ticket_key) else {
                return Lookup::none();
            };
            if t.info.is_empty() {
                return Lookup::none();
            }
            let mut l = self.tickets.get(&c.name, &c.workspace, t, k, force);
            if let Some(info) = l.value.as_mut() {
                if info.url.is_none() {
                    info.url = ticket_url.clone();
                }
                if let Some(ep) = info.epic.as_mut()
                    && ep.url.is_none()
                {
                    ep.url = adapter::ticket_url(t, &ep.key);
                }
            }
            l
        };
        // Ticket adapter and gh are independent: run them side by side. Review state is set
        // afterwards from the context's one rank call (`apply_review`).
        let (tl, pl) = std::thread::scope(|sc| {
            let h = sc.spawn(|| {
                if self.fetch_prs {
                    self.pr_for(c, &branches, force)
                } else {
                    Lookup::none()
                }
            });
            let t = fetch_ticket();
            (t, h.join().unwrap_or_else(|_| Lookup::none()))
        });
        let (ticket, ticket_error, ticket_stale) = (tl.value, tl.error, tl.stale);
        let (pr, pr_error, pr_stale) = (pl.value, pl.error, pl.stale);
        let progress =
            adapter::read_progress(ad.and_then(|a| a.progress.as_ref()), ticket_key.as_deref());
        let decisions = task_decisions(&task);
        let wts: Vec<&str> = branches
            .iter()
            .map(|b| b.worktree.as_str())
            .filter(|w| !w.is_empty())
            .collect();
        let session = sessions::session_for(live, &wts).cloned();
        let workers = sessions::workers_for(workers, &task.id, ticket_key.as_deref(), &wts);
        let epic_key = ticket
            .as_ref()
            .and_then(|t| t.epic.as_ref())
            .map(|e| e.key.clone());
        let pin_rank = ad.and_then(|a| {
            adapter::pin_rank(&a.priorities, ticket_key.as_deref(), epic_key.as_deref())
        });
        let mut changes: Vec<String> = vec![];
        changes.extend(task.updated_at.clone());
        changes.extend(branches.iter().filter_map(|b| b.updated.clone()));
        changes.extend(session.as_ref().and_then(|s| s.last_activity.clone()));
        changes.extend(workers.iter().filter_map(|w| w.modified.clone()));
        let last_change = changes
            .into_iter()
            .filter_map(|s| crate::util::parse_ts(&s))
            .max()
            .map(|d| d.to_rfc3339());
        let mut j = JoinedTask {
            context: c.name.clone(),
            task,
            branches,
            ticket_key,
            ticket_url,
            ticket,
            ticket_error,
            ticket_stale,
            pr,
            pr_error,
            pr_stale,
            progress,
            decisions,
            session,
            workers,
            badge: Badge::Unknown,
            pin_rank,
            last_change,
            review_waiting: None,
            review_unknown: None,
            review_stale: None,
        };
        crate::prompts::apply(&mut j);
        j.badge = badge(&j);
        j
    }

    fn pr_for(
        &self,
        c: &Context,
        branches: &[crate::model::Branch],
        force: Fetch,
    ) -> Lookup<PrInfo, String> {
        let slug_of = |repo: &str| {
            c.github_owner
                .as_ref()
                .filter(|_| !repo.is_empty())
                .map(|o| format!("{o}/{repo}"))
        };
        for b in branches.iter().rev() {
            if let Some(sel) = b.pr_url.clone().or_else(|| b.pr.clone()) {
                let slug = slug_of(&b.repo);
                return self.prs.get(slug.as_deref(), &sel, force);
            }
        }
        // No PR recorded yet: ask gh by branch name; any failure (usually "no pull requests")
        // is not shown, but a last good PR is, marked stale.
        for b in branches.iter().rev().filter(|b| b.status == "open") {
            if let Some(slug) = slug_of(&b.repo)
                && !b.branch.is_empty()
            {
                let l = self.prs.get(Some(&slug), &b.branch, force);
                if l.value.is_some() {
                    return Lookup { error: None, ..l };
                }
            }
        }
        Lookup::none()
    }
}

impl Default for Loader {
    fn default() -> Self {
        Self::new()
    }
}

pub fn badge(j: &JoinedTask) -> Badge {
    badge_on(j, chrono::Local::now().date_naive())
}

/// A `working` task with no change for this long and nobody on it is stalled.
pub const STALLED_AFTER_H: i64 = 4;

/// The badge as of `today`, which decides whether a parked task is overdue.
pub fn badge_on(j: &JoinedTask, today: chrono::NaiveDate) -> Badge {
    badge_at(j, today, chrono::Utc::now())
}

/// Declared `waiting-on-review` whose PR has merged, closed or been approved.
fn review_state_stale(j: &JoinedTask) -> bool {
    j.pr.as_ref().is_some_and(|p| {
        p.state == "MERGED"
            || p.state == "CLOSED"
            || p.review_decision.as_deref() == Some("APPROVED")
    })
}

/// Declared `working`, but no unfinished worker, no live session and no recent change.
fn stalled(j: &JoinedTask, now: chrono::DateTime<chrono::Utc>) -> bool {
    if j.workers.iter().any(|w| !w.finished) || j.session.is_some() {
        return false;
    }
    let at = j.task.state.as_ref().and_then(|s| s.at.as_deref());
    let newest = [j.last_change.as_deref(), at]
        .into_iter()
        .flatten()
        .filter_map(crate::util::parse_ts)
        .max();
    newest.is_none_or(|t| now - t > chrono::Duration::hours(STALLED_AFTER_H))
}

/// As `badge_on`, with `now` deciding whether a `working` task is stalled.
pub fn badge_at(
    j: &JoinedTask,
    today: chrono::NaiveDate,
    now: chrono::DateTime<chrono::Utc>,
) -> Badge {
    let open_pr = j.pr.as_ref().filter(|p| p.state == "OPEN");
    let declared = j.task.state.as_ref().map(|s| s.state.as_str());
    if j.open_decisions().next().is_some()
        || j.session.as_ref().is_some_and(|s| s.needs_input())
        || declared == Some("waiting-on-me")
    {
        Badge::NeedsYou
    } else if j.review_waiting.is_some() {
        Badge::ReviewRequested
    } else if j.review_unknown.is_some() {
        Badge::ReviewUnknown
    } else if j.overdue_on(today) {
        Badge::Overdue
    } else if open_pr.is_some_and(|p| p.checks.fail > 0) {
        Badge::CiFailing
    } else if declared == Some("waiting-on-review") && review_state_stale(j) {
        Badge::StateStale
    } else if declared == Some("working") && stalled(j, now) {
        Badge::Stalled
    } else if open_pr.is_some_and(|p| {
        !p.is_draft
            && p.review_decision.as_deref() == Some("APPROVED")
            && p.checks.fail == 0
            && p.checks.pending == 0
    }) {
        Badge::ReadyToMerge
    } else if open_pr.is_some_and(|p| p.checks.pending > 0) {
        Badge::WaitingOnCi
    } else if j.workers.iter().any(|w| !w.finished) {
        Badge::WorkerRunning
    } else if j.session.is_some() {
        Badge::Interactive
    } else if let Some(s) = j
        .task
        .state
        .as_ref()
        .filter(|s| s.badge() != Badge::Unknown)
    {
        s.badge()
    } else if j.task.has_label("ready-for-worktree-delete") || j.kept_open() {
        Badge::Parked
    } else {
        Badge::Unknown
    }
}

/// Sort tier: needs-you > review requested > review unknown > overdue > CI failing > state
/// stale > stalled > ready to merge > waiting on CI > working > unknown > the declared waits.
pub fn badge_rank(b: Badge) -> u8 {
    match b {
        Badge::NeedsYou => 0,
        Badge::ReviewRequested => 1,
        Badge::ReviewUnknown => 2,
        Badge::Overdue => 3,
        Badge::CiFailing => 4,
        Badge::ReadyToMerge => 7,
        Badge::WaitingOnCi => 8,
        Badge::StateStale => 5,
        Badge::Stalled => 6,
        Badge::WorkerRunning | Badge::Interactive | Badge::Working => 9,
        Badge::Unknown => 10,
        Badge::InReview => 11,
        Badge::Blocked => 12,
        Badge::WaitingUntil => 13,
        Badge::Paused => 14,
        Badge::DoneCleanup | Badge::Parked => 15,
    }
}

/// A review waiting on you or of unknown state (it may be waiting, so it keeps that place),
/// needs-you in a pinned stream, other needs-you, overdue (pinned or
/// not), rest of the pinned streams, then everything else. Within a group: badge tier, then pin
/// order, then most recent change.
pub fn sort_tasks(v: &mut [JoinedTask]) {
    v.sort_by_key(|j| {
        let group = match (
            j.review_waiting.is_some() || j.review_unknown.is_some(),
            j.needs_you(),
            j.badge == Badge::Overdue,
            j.pin_rank.is_some(),
        ) {
            (true, ..) => 0,
            (false, true, _, true) => 1,
            (false, true, _, false) => 2,
            (false, false, true, _) => 3,
            (false, false, false, true) => 4,
            (false, false, false, false) => 5,
        };
        (
            group,
            badge_rank(j.badge),
            j.pin_rank.unwrap_or(usize::MAX),
            Reverse(j.last_change.clone()),
        )
    });
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::model::{Branch, Checks, Worker};

    pub fn jt(id: &str) -> JoinedTask {
        JoinedTask {
            context: "c".into(),
            task: Task {
                id: id.into(),
                ..Default::default()
            },
            branches: vec![],
            ticket_key: None,
            ticket_url: None,
            ticket: None,
            ticket_error: None,
            ticket_stale: None,
            pr: None,
            pr_error: None,
            pr_stale: None,
            progress: Default::default(),
            decisions: vec![],
            session: None,
            workers: vec![],
            badge: Badge::Unknown,
            pin_rank: None,
            last_change: None,
            review_waiting: None,
            review_unknown: None,
            review_stale: None,
        }
    }

    #[test]
    fn sort_order() {
        let mut a = jt("rest-old");
        a.last_change = Some("2026-01-01T00:00:00+00:00".into());
        let mut b = jt("rest-new");
        b.last_change = Some("2026-02-01T00:00:00+00:00".into());
        let mut c = jt("pinned1");
        c.pin_rank = Some(1);
        let mut d = jt("needs");
        d.badge = Badge::NeedsYou;
        let mut e = jt("needs-pinned1");
        e.badge = Badge::NeedsYou;
        e.pin_rank = Some(1);
        let mut f = jt("needs-pinned0");
        f.badge = Badge::NeedsYou;
        f.pin_rank = Some(0);
        let mut g = jt("overdue");
        g.badge = Badge::Overdue;
        let mut r = jt("review-old");
        r.review_waiting = Some("review requested".into());
        r.badge = Badge::ReviewRequested;
        r.last_change = Some("2026-01-01T00:00:00+00:00".into());
        let mut rn = jt("review-new");
        rn.review_waiting = Some("no review submitted yet".into());
        rn.badge = Badge::ReviewRequested;
        rn.last_change = Some("2026-02-01T00:00:00+00:00".into());
        // A review leaf whose review is submitted is just another task.
        let mut done = jt("review-submitted");
        done.task.labels = vec!["pr-review".into()];
        // A review whose state could not be read keeps its place beside the waiting ones.
        let mut u = jt("review-unknown");
        u.review_unknown = Some("PR unreadable (offline)".into());
        u.badge = Badge::ReviewUnknown;
        u.last_change = Some("2026-03-01T00:00:00+00:00".into());
        let mut v = vec![a, b, c, d, u, e, f, g, r, done, rn];
        sort_tasks(&mut v);
        let ids: Vec<_> = v.iter().map(|j| j.task.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "review-new",
                "review-old",
                "review-unknown",
                "needs-pinned0",
                "needs-pinned1",
                "needs",
                "overdue",
                "pinned1",
                "rest-new",
                "rest-old",
                "review-submitted"
            ]
        );
    }

    #[test]
    fn badges() {
        let mut j = jt("x");
        assert_eq!(badge(&j), Badge::Unknown);
        j.task.labels = vec!["keep-task-open".into()];
        assert_eq!(badge(&j), Badge::Parked);
        j.workers.push(Worker {
            description: "x: y".into(),
            transcript: String::new(),
            finished: false,
            modified: None,
        });
        assert_eq!(badge(&j), Badge::WorkerRunning);
        j.pr = Some(PrInfo {
            state: "OPEN".into(),
            checks: Checks {
                pending: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(badge(&j), Badge::WaitingOnCi);
        j.pr = Some(PrInfo {
            state: "OPEN".into(),
            review_decision: Some("APPROVED".into()),
            checks: Checks {
                pass: 2,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(badge(&j), Badge::ReadyToMerge);
        j.pr.as_mut().unwrap().checks.fail = 1;
        assert_eq!(badge(&j), Badge::CiFailing);
        j.review_waiting = Some("review requested".into());
        assert_eq!(badge(&j), Badge::ReviewRequested);
        j.decisions = vec![Default::default()];
        assert_eq!(badge(&j), Badge::NeedsYou);
        // Needs-you on a waiting review keeps the badge, but the review group still sorts first.
        let mut n = jt("plain-needs");
        n.badge = Badge::NeedsYou;
        j.badge = badge(&j);
        let mut v = vec![n, j];
        sort_tasks(&mut v);
        assert_eq!(v[0].task.id, "x");
    }

    #[test]
    fn overdue_parked_task() {
        let day = |s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let today = day("2026-05-10");
        let parked = |until: Option<&str>| Branch {
            keep_task_open: Some("yes".into()),
            waiting_on: Some("the vendor fix".into()),
            until: until.map(Into::into),
            ..Default::default()
        };
        let mut j = jt("x");
        j.branches = vec![parked(Some("2026-05-20"))];
        assert_eq!(badge_on(&j, today), Badge::Parked);
        assert_eq!(
            j.waiting_text().as_deref(),
            Some("waiting on the vendor fix until 2026-05-20")
        );
        j.branches = vec![parked(Some("2026-05-10"))];
        assert_eq!(
            badge_on(&j, today),
            Badge::Parked,
            "due today is not overdue"
        );
        j.branches = vec![parked(Some("soon"))];
        assert_eq!(
            badge_on(&j, today),
            Badge::Parked,
            "a bad date is never overdue"
        );
        j.branches = vec![parked(None)];
        assert_eq!(
            j.waiting_text().as_deref(),
            Some("waiting on the vendor fix")
        );
        // An older entry's date is superseded by the newest one that recorded anything.
        j.branches = vec![parked(Some("2026-05-01")), parked(Some("2026-06-01"))];
        assert_eq!(badge_on(&j, today), Badge::Parked);
        j.branches.reverse();
        assert_eq!(badge_on(&j, today), Badge::Overdue);
        // Overdue outranks CI and a live session, but not needs-you.
        j.session = Some(Default::default());
        assert_eq!(badge_on(&j, today), Badge::Overdue);
        j.decisions = vec![Default::default()];
        assert_eq!(badge_on(&j, today), Badge::NeedsYou);
        // Not kept open: a past date alone means nothing.
        let mut k = jt("y");
        k.branches = vec![Branch {
            until: Some("2026-05-01".into()),
            ..Default::default()
        }];
        assert_eq!(badge_on(&k, today), Badge::Unknown);
        k.task.labels = vec!["keep-task-open".into()];
        assert_eq!(badge_on(&k, today), Badge::Overdue);
    }

    fn declared(j: &mut JoinedTask, state: &str, until: Option<&str>) {
        j.task.state = Some(crate::model::TaskState {
            state: state.into(),
            until: until.map(Into::into),
            ..Default::default()
        });
    }

    #[test]
    fn declared_state_badges_and_cross_checks() {
        let day = |s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let today = day("2026-12-02");
        let now = crate::util::parse_ts("2026-12-02T12:00:00Z").unwrap();
        let at = |j: &JoinedTask| badge_at(j, today, now);
        let mut j = jt("x");
        assert_eq!(
            at(&j),
            Badge::Unknown,
            "nothing declared is unknown, never idle"
        );
        // Each declared state shows as itself when no world fact outranks it.
        j.last_change = Some("2026-12-02T11:00:00Z".into());
        for (st, b) in [
            ("waiting-on-me", Badge::NeedsYou),
            ("working", Badge::Working),
            ("waiting-on-review", Badge::InReview),
            ("paused", Badge::Paused),
            ("done-pending-cleanup", Badge::DoneCleanup),
            ("no-such-state", Badge::Unknown),
        ] {
            declared(&mut j, st, None);
            assert_eq!(at(&j), b, "{st}");
        }
        // blocked stays visible until its date, then turns overdue the day after.
        declared(&mut j, "blocked", Some("2026-12-02"));
        assert_eq!(at(&j), Badge::Blocked);
        declared(&mut j, "blocked", Some("2026-12-01"));
        assert_eq!(at(&j), Badge::Overdue);
        declared(&mut j, "waiting-until", Some("2026-12-01"));
        assert_eq!(at(&j), Badge::Overdue);
        declared(&mut j, "waiting-until", Some("2026-12-09"));
        assert_eq!(at(&j), Badge::WaitingUntil);
        // working with nobody on it and no change for 4 h is stalled; paused never is.
        j.last_change = Some("2026-12-02T07:00:00Z".into());
        declared(&mut j, "working", None);
        assert_eq!(at(&j), Badge::Stalled);
        j.task.state.as_mut().unwrap().at = Some("2026-12-02T10:00:00Z".into());
        assert_eq!(at(&j), Badge::Working, "a fresh declaration is a change");
        j.task.state.as_mut().unwrap().at = None;
        j.session = Some(Default::default());
        assert_eq!(
            at(&j),
            Badge::Interactive,
            "a live session is somebody on it"
        );
        j.session = None;
        j.last_change = None;
        assert_eq!(at(&j), Badge::Stalled, "no readable change time");
        declared(&mut j, "paused", None);
        assert_eq!(at(&j), Badge::Paused);
        // waiting-on-review whose PR merged or was approved is stale; CI failing outranks it.
        declared(&mut j, "waiting-on-review", None);
        j.pr = Some(PrInfo {
            state: "MERGED".into(),
            ..Default::default()
        });
        assert_eq!(at(&j), Badge::StateStale);
        j.pr = Some(PrInfo {
            state: "OPEN".into(),
            checks: Checks {
                fail: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(at(&j), Badge::CiFailing);
        j.pr.as_mut().unwrap().checks.fail = 0;
        assert_eq!(at(&j), Badge::InReview);
        j.pr.as_mut().unwrap().review_decision = Some("APPROVED".into());
        assert_eq!(at(&j), Badge::StateStale);
    }

    fn plain_ctx() -> Context {
        Context {
            name: "c".into(),
            workspace: "/ws".into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        }
    }

    #[test]
    fn in_flight_shows_declared_and_counts_stateless_backlog() {
        let ctx = plain_ctx();
        let t = |id: &str, status: &str| Task {
            id: id.into(),
            status: status.into(),
            ..Default::default()
        };
        let mut blocked = t("b", "blocked");
        blocked.state = Some(crate::model::TaskState {
            state: "blocked".into(),
            on: Some("Security sign-off".into()),
            ..Default::default()
        });
        let mut decision = t("d", "open");
        decision.labels = vec!["needs-decision".into()];
        let tasks = vec![
            t("w", "in_progress"),
            blocked,
            decision,
            t("o1", "open"),
            t("o2", "blocked"),
        ];
        let (shown, backlog) = in_flight(&ctx, &tasks);
        let ids: Vec<&str> = shown.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["w", "b", "d"]);
        assert_eq!(backlog, 2);
    }

    #[test]
    fn active_is_listed_and_an_older_backend_falls_back_to_in_progress() {
        struct Picky {
            refuse_active: bool,
            asked: Mutex<Vec<String>>,
        }
        impl Tracker for Picky {
            fn list(&self, s: &str) -> Result<Vec<Task>> {
                self.asked.lock().unwrap().push(s.into());
                if s == "active" && self.refuse_active {
                    anyhow::bail!("unknown status active");
                }
                Ok(vec![])
            }
            fn get(&self, _id: &str) -> Result<Task> {
                anyhow::bail!("unused")
            }
            fn list_branches(&self, _id: &str) -> Result<Vec<Branch>> {
                Ok(vec![])
            }
            fn note(&self, _id: &str, _t: &str) -> Result<()> {
                Ok(())
            }
        }
        let ctx = plain_ctx();
        for (refuse, want) in [
            (false, vec!["active"]),
            (true, vec!["active", "in_progress"]),
        ] {
            let p = Picky {
                refuse_active: refuse,
                asked: Mutex::new(vec![]),
            };
            assert!(list_active(&ctx, &p).is_ok());
            assert_eq!(*p.asked.lock().unwrap(), want);
        }
    }

    /// In-memory tracker: records notes instead of writing anywhere.
    #[derive(Default)]
    pub struct FakeTracker {
        pub tasks: Vec<Task>,
        pub branches: HashMap<String, Vec<Branch>>,
        pub notes: Mutex<Vec<(String, String)>>,
        pub me: Option<String>,
        pub decisions: HashMap<String, Vec<crate::model::Decision>>,
        pub decision_reads: Mutex<Vec<String>>,
        pub fail_decisions: Mutex<bool>,
        pub replies: Mutex<Vec<[String; 4]>>,
    }

    impl Tracker for FakeTracker {
        fn list(&self, _status: &str) -> Result<Vec<Task>> {
            Ok(self.tasks.clone())
        }
        fn get(&self, id: &str) -> Result<Task> {
            self.tasks
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }
        fn list_branches(&self, id: &str) -> Result<Vec<Branch>> {
            Ok(self.branches.get(id).cloned().unwrap_or_default())
        }
        fn note(&self, id: &str, text: &str) -> Result<()> {
            self.notes.lock().unwrap().push((id.into(), text.into()));
            Ok(())
        }
        fn whoami(&self) -> Result<Option<String>> {
            Ok(self.me.clone())
        }
        fn decisions(&self, id: &str) -> Result<Vec<crate::model::Decision>> {
            self.decision_reads.lock().unwrap().push(id.into());
            if *self.fail_decisions.lock().unwrap() {
                anyhow::bail!("tracker decisions: boom");
            }
            Ok(self.decisions.get(id).cloned().unwrap_or_default())
        }
        fn decisions_all(&self, id: &str) -> Result<Vec<crate::model::Decision>> {
            self.decisions(id)
        }
        fn reply(&self, id: &str, did: &str, kind: &str, text: &str) -> Result<()> {
            let r = [id, did, kind, text].map(String::from);
            self.replies.lock().unwrap().push(r);
            Ok(())
        }
    }

    #[test]
    fn join_with_fake() {
        let mut fake = FakeTracker::default();
        let t = Task {
            id: "ctx-1".into(),
            labels: vec!["jira-ABC-1".into(), "needs-decision".into()],
            notes: "DECISION (2026-11-12): ship? Rec: yes".into(),
            updated_at: Some("2026-11-12T00:00:00Z".into()),
            ..Default::default()
        };
        fake.tasks.push(t.clone());
        fake.branches.insert(
            "ctx-1".into(),
            vec![Branch {
                worktree: "/wt/a".into(),
                ..Default::default()
            }],
        );
        let ctx = Context {
            name: "c".into(),
            workspace: "/".into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: Some(
                serde_yaml::from_str(
                    "ticket:\n  pattern: '^jira-(.+)$'\n  url: 'https://j/{key}'\npriorities:\n  - ticket: ABC-1\n",
                )
                .unwrap(),
            ),
            adapter_error: None,
        };
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.fetch_prs = false;
        let live = vec![Session {
            worktree: "/wt/a".into(),
            status: "idle".into(),
            ..Default::default()
        }];
        let b = fake.list_branches("ctx-1").unwrap();
        let j = loader.join(&ctx, t, b, &live, &[], false);
        assert_eq!(j.ticket_key.as_deref(), Some("ABC-1"));
        assert_eq!(j.ticket_url.as_deref(), Some("https://j/ABC-1"));
        assert_eq!(j.pin_rank, Some(0));
        assert_eq!(j.badge, Badge::NeedsYou);
        assert!(j.session.is_some());
        assert_eq!(j.decisions[0].recommendation.as_deref(), Some("yes"));
    }

    /// 12 review tasks, a warm PR cache and a known login: one rank process, zero gh calls,
    /// and the verdicts land on the joined tasks with their badges.
    #[test]
    fn reviews_rank_once_from_the_pr_cache() {
        let baton = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../baton");
        let loader =
            Loader::with_factory(Some(baton), Box::new(|_| Arc::new(FakeTracker::default())));
        loader.reviews.set_login(Some("me".into()));
        let tasks: Vec<Task> = (1..=12)
            .map(|i| Task {
                id: format!("r{i}"),
                status: "in_progress".into(),
                labels: vec!["pr-review".into()],
                description: format!("Review https://github.com/o/p/pull/{i}"),
                ..Default::default()
            })
            .collect();
        for i in 1..=12 {
            let requested = if i == 5 {
                vec![]
            } else {
                vec![serde_json::json!({"login": "me"})]
            };
            let reviews = if i == 5 {
                vec![serde_json::json!({"author": {"login": "me"}, "state": "APPROVED"})]
            } else {
                vec![]
            };
            loader.prs.seed(
                Some("o/p"),
                &format!("https://github.com/o/p/pull/{i}"),
                PrInfo {
                    state: "OPEN".into(),
                    review: Some(serde_json::json!({
                        "state": "OPEN", "reviewRequests": requested, "latestReviews": reviews
                    })),
                    ..Default::default()
                },
            );
        }
        loader.rank_reviews("work", &tasks, false);
        let rows = loader.health.rows();
        let row = rows.iter().find(|r| r.source == Source::Review).unwrap();
        assert_eq!((row.scope.as_str(), row.degraded), ("work", false));
        assert!(row.health.last_ok.is_some());
        assert_eq!(
            loader
                .reviews
                .rank_calls
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            loader
                .prs
                .cache
                .fetches
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        let mut j = jt("r1");
        j.task = tasks[0].clone();
        loader.apply_review(&mut j);
        assert_eq!(j.review_waiting.as_deref(), Some("review requested"));
        assert_eq!(j.badge, Badge::ReviewRequested);
        j.task = tasks[4].clone();
        loader.apply_review(&mut j);
        assert_eq!(
            (j.review_waiting.clone(), j.review_unknown.clone()),
            (None, None)
        );
        assert_eq!(j.badge, Badge::Unknown);
    }

    #[test]
    fn answered_decision_leaves_and_new_question_returns() {
        let ctx = Context {
            name: "c".into(),
            workspace: "/".into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.fetch_prs = false;
        let task = |notes: &str| Task {
            id: "ctx-2".into(),
            labels: vec!["needs-decision".into()],
            notes: notes.into(),
            ..Default::default()
        };
        let asked = "DECISION (2026-11-12): ship? Rec: yes";
        let answered = format!("{asked}\nANSWER (fermata, 2026-11-12T10:00Z): yes");
        let j = loader.join(&ctx, task(&answered), vec![], &[], &[], false);
        assert!(j.decisions.is_empty(), "answered: out of the decision view");
        assert_ne!(j.badge, Badge::NeedsYou);
        let again = format!("{answered}\nDECISION (2026-11-13): and the docs?");
        let j = loader.join(&ctx, task(&again), vec![], &[], &[], false);
        assert!(j.decisions[0].text.starts_with("and the docs?"));
        assert_eq!(j.badge, Badge::NeedsYou);
        // The label alone, never asked or answered, still counts.
        let j = loader.join(&ctx, task(""), vec![], &[], &[], false);
        assert_eq!(j.decisions.len(), 1);
        // No label: a trailing DECISION still shows, marked; one followed by any note does not.
        let bare = |notes: &str| Task {
            labels: vec![],
            ..task(notes)
        };
        let j = loader.join(&ctx, bare(&again), vec![], &[], &[], false);
        assert!(j.decisions[0].label_missing);
        assert_eq!(j.badge, Badge::NeedsYou);
        let moved = format!("{again}\nNOTE: later");
        assert!(
            loader
                .join(&ctx, bare(&moved), vec![], &[], &[], false)
                .decisions
                .is_empty()
        );
        assert!(
            !loader
                .join(&ctx, task(&again), vec![], &[], &[], false)
                .decisions[0]
                .label_missing
        );
    }

    /// Tracker decisions: read only for labelled tasks, several per task, and an answered one
    /// waits on the orchestrator without needing you.
    #[test]
    fn tracker_decisions_load_and_badge() {
        let ctx = Context {
            name: "c".into(),
            workspace: "/".into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.fetch_prs = false;
        let d = |id: &str, st| crate::model::Decision {
            id: Some(id.into()),
            text: format!("q {id}"),
            status: st,
            ..Default::default()
        };
        use crate::model::DecisionStatus::*;
        let mut tr = FakeTracker::default();
        for id in ["a", "b"] {
            tr.tasks.push(Task {
                id: id.into(),
                labels: vec!["needs-decision".into()],
                notes: "ANSWER (fermata, 2026-11-12T10:00Z): status update".into(),
                ..Default::default()
            });
        }
        tr.tasks.push(Task {
            id: "plain".into(),
            ..Default::default()
        });
        tr.decisions
            .insert("a".into(), vec![d("d1", Open), d("d2", Answered)]);
        tr.decisions.insert("b".into(), vec![d("d3", Answered)]);
        let (st, _) = loader.tracker_state(&ctx, &tr, false).unwrap();
        let mut reads = tr.decision_reads.lock().unwrap().clone();
        reads.sort();
        assert_eq!(reads, ["a", "b"], "only labelled tasks are read");
        let j = |i: usize| loader.join(&ctx, st.tasks[i].clone(), vec![], &[], &[], false);
        let (a, b) = (j(0), j(1));
        assert_eq!(a.decisions.len(), 2);
        assert_eq!(a.badge, Badge::NeedsYou);
        assert_eq!(b.decisions.len(), 1, "answered still shows");
        assert_ne!(b.badge, Badge::NeedsYou, "…but waits on the orchestrator");
        // A failed read keeps the last good decisions.
        *tr.fail_decisions.lock().unwrap() = true;
        let (st, _) = loader.tracker_state(&ctx, &tr, true).unwrap();
        assert_eq!(st.tasks[0].decisions.as_ref().map(Vec::len), Some(2));
    }

    /// Counts list and list-branches calls so the tracker's cost can be observed.
    #[derive(Default)]
    struct CountingTracker {
        lists: std::sync::atomic::AtomicUsize,
        branch_reads: Mutex<Vec<String>>,
        tasks: Vec<Task>,
    }

    impl Tracker for CountingTracker {
        fn list(&self, _s: &str) -> Result<Vec<Task>> {
            self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !self.tasks.is_empty() {
                return Ok(self.tasks.clone());
            }
            Ok(vec![Task {
                id: "ctx-9".into(),
                ..Default::default()
            }])
        }
        fn get(&self, _id: &str) -> Result<Task> {
            anyhow::bail!("unused")
        }
        fn list_branches(&self, id: &str) -> Result<Vec<Branch>> {
            self.branch_reads.lock().unwrap().push(id.into());
            Ok(vec![Branch {
                branch: "b".into(),
                keep_task_open: Some("yes".into()),
                until: Some("2026-01-01".into()),
                ..Default::default()
            }])
        }
        fn note(&self, _id: &str, _t: &str) -> Result<()> {
            anyhow::bail!("unused")
        }
    }

    #[test]
    fn whoami_is_cached_and_force_asks_again() {
        let root = std::env::temp_dir().join(format!("fermata-me-{}", std::process::id()));
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.disk = Some(DiskCache::new(root.join("cache")));
        let ctx = Context {
            name: "c".into(),
            workspace: root.clone(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let pat = FakeTracker {
            me: Some("Pat".into()),
            ..Default::default()
        };
        assert_eq!(loader.me_for(&ctx, &pat, false).as_deref(), Some("Pat"));
        let nobody = FakeTracker::default();
        assert_eq!(
            loader.me_for(&ctx, &nobody, false).as_deref(),
            Some("Pat"),
            "served from cache"
        );
        assert_eq!(loader.me_for(&ctx, &nobody, true), None, "force asks again");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn tracker_state_cached_by_signature() {
        let root = std::env::temp_dir().join(format!("fermata-ts-{}", std::process::id()));
        let tdir = root.join(".beads");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("manifest"), "root-a").unwrap();
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.disk = Some(DiskCache::new(root.join("cache")));
        let ctx = Context {
            name: "c".into(),
            workspace: root.clone(),
            tracker_dir: Some(tdir.clone()),
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let tr = CountingTracker::default();
        let n = || tr.lists.load(std::sync::atomic::Ordering::SeqCst);
        let (st, _) = loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!((st.tasks.len(), n()), (1, 1));
        assert!(
            tr.branch_reads.lock().unwrap().is_empty(),
            "no list-branches"
        );
        loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!(n(), 1, "unchanged signature is served from cache");
        std::fs::write(tdir.join("manifest"), "root-b").unwrap();
        loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!(n(), 2, "a write invalidates");
        loader.tracker_state(&ctx, &tr, true).unwrap();
        assert_eq!(n(), 3, "force bypasses");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 80 in-progress tasks cost one `list`; `list-branches` only for the parked ones you see,
    /// and those are cached with the tracker state.
    #[test]
    fn a_refresh_costs_one_list_not_one_call_per_task() {
        let root = std::env::temp_dir().join(format!("fermata-o1-{}", std::process::id()));
        let tdir = root.join(".beads");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("manifest"), "root-a").unwrap();
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.disk = Some(DiskCache::new(root.join("cache")));
        let ctx = Context {
            name: "c".into(),
            workspace: root.clone(),
            tracker_dir: Some(tdir.clone()),
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let mut tasks: Vec<Task> = (0..80)
            .map(|i| Task {
                id: format!("ctx-{i}"),
                assignee: Some(if i % 2 == 0 { "Me" } else { "Other" }.into()),
                ..Default::default()
            })
            .collect();
        for i in [0, 1, 2, 3] {
            tasks[i].labels = vec!["keep-task-open".into()];
        }
        tasks[3].labels.push("needs-decision".into());
        tasks[5].assignee = None;
        tasks[5].labels = vec!["keep-task-open".into()];
        let tr = CountingTracker {
            tasks,
            ..Default::default()
        };
        let (st, _) = loader.tracker_state(&ctx, &tr, false).unwrap();
        let reg = loader.parked_registry(&ctx, &tr, &st.tasks, Some("me"), false);
        let mut read = tr.branch_reads.lock().unwrap().clone();
        read.sort();
        // ctx-1 is somebody else's parked task: the mine filter hides it, so it is not read.
        assert_eq!(read, ["ctx-0", "ctx-2", "ctx-3", "ctx-5"]);
        assert_eq!(tr.lists.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reg["ctx-0"][0].until.as_deref(), Some("2026-01-01"));
        loader.parked_registry(&ctx, &tr, &st.tasks, Some("me"), false);
        assert_eq!(
            tr.branch_reads.lock().unwrap().len(),
            4,
            "cached per signature"
        );
        // Who you are unknown: every parked task may be yours.
        let all = loader.parked_registry(&ctx, &tr, &st.tasks, None, false);
        assert_eq!(all.len(), 5);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The loader joins local worktrees from fake carrier dirs: worktree, branch and a review
    /// carrier's PR, with no tracker read beyond `list`.
    #[test]
    fn local_worktrees_link_without_the_registry() {
        let root = std::env::temp_dir().join(format!("fermata-lw-{}", std::process::id()));
        let repo = root.join("code/app");
        let gd = repo.join(".git/worktrees/ctx-1-x");
        let wt = root.join("code/app-worktrees/ctx-1-x");
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(gd.join("gitdir"), format!("{}/.git", wt.display())).unwrap();
        std::fs::write(gd.join("HEAD"), "ref: refs/heads/ctx-1-x\n").unwrap();
        std::fs::write(gd.join("baton-identity"), "leaf=ctx-1\nkind=review\npr=7\n").unwrap();
        let ctx = Context {
            name: "c".into(),
            workspace: root.clone(),
            tracker_dir: None,
            github_owner: Some("o".into()),
            home: None,
            member_repos: vec![repo.to_string_lossy().into_owned()],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let t = Task {
            id: "ctx-1".into(),
            description: "review https://h/o/app/pull/7".into(),
            ..Default::default()
        };
        let local = crate::worktrees::local_branches(&ctx);
        let b = branches_for(&t, vec![], local.get("ctx-1"));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].worktree, wt.to_string_lossy());
        assert_eq!(
            (b[0].repo.as_str(), b[0].branch.as_str()),
            ("app", "ctx-1-x")
        );
        assert_eq!(b[0].pr_url.as_deref(), Some("https://h/o/app/pull/7"));
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.fetch_prs = false;
        let live = vec![Session {
            worktree: wt.to_string_lossy().into_owned(),
            status: "idle".into(),
            ..Default::default()
        }];
        let j = loader.join(&ctx, t, b, &live, &[], false);
        assert!(
            j.session.is_some(),
            "the session matches through the carrier's worktree"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A tracker whose reads can be made to fail (or time out) and recover.
    #[derive(Default)]
    struct FlakyTracker {
        tasks: Vec<Task>,
        branches: HashMap<String, Vec<Branch>>,
        /// When set, `list` and `list-branches` fail with this message.
        down: Mutex<Option<String>>,
    }

    impl FlakyTracker {
        fn set_down(&self, why: Option<&str>) {
            *self.down.lock().unwrap() = why.map(str::to_string);
        }
        fn check(&self) -> Result<()> {
            match self.down.lock().unwrap().as_deref() {
                Some(why) => anyhow::bail!("{why}"),
                None => Ok(()),
            }
        }
    }

    impl Tracker for FlakyTracker {
        fn list(&self, _s: &str) -> Result<Vec<Task>> {
            self.check()?;
            Ok(self.tasks.clone())
        }
        fn get(&self, id: &str) -> Result<Task> {
            self.check()?;
            self.tasks
                .iter()
                .find(|t| t.id == id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("not found"))
        }
        fn list_branches(&self, id: &str) -> Result<Vec<Branch>> {
            self.check()?;
            Ok(self.branches.get(id).cloned().unwrap_or_default())
        }
        fn note(&self, _id: &str, _t: &str) -> Result<()> {
            anyhow::bail!("unused")
        }
    }

    fn scratch(tag: &str) -> (std::path::PathBuf, Context) {
        let root = std::env::temp_dir().join(format!("fermata-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tdir = root.join(".beads");
        std::fs::create_dir_all(&tdir).unwrap();
        std::fs::write(tdir.join("manifest"), "root-a").unwrap();
        let ctx = Context {
            name: "c".into(),
            workspace: root.clone(),
            tracker_dir: Some(tdir),
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        (root, ctx)
    }

    fn ids(st: &TrackerState) -> Vec<&str> {
        st.tasks.iter().map(|t| t.id.as_str()).collect()
    }

    fn list_degraded(l: &Loader) -> bool {
        l.health
            .rows()
            .iter()
            .any(|r| r.source == Source::Tracker && r.degraded)
    }

    /// A failed or timed-out `list` serves the last good tasks, marked stale; a success clears it.
    #[test]
    fn a_failed_list_keeps_the_last_good_tasks() {
        let (root, ctx) = scratch("stale");
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.disk = Some(DiskCache::new(root.join("cache")));
        let mut tr = FlakyTracker {
            tasks: vec![Task {
                id: "c-1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let (st, stale) = loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!((ids(&st), stale), (vec!["c-1"], None));

        tr.set_down(Some("bash timed out after 20s"));
        let (st, stale) = loader.tracker_state(&ctx, &tr, true).unwrap();
        let stale = stale.expect("marked stale");
        assert_eq!(ids(&st), ["c-1"], "never blanked");
        assert!(stale.error.contains("timed out"));
        assert!(list_degraded(&loader));
        assert_eq!(loader.fails.lock().unwrap()["c"], 1);
        tr.set_down(Some("tracker list: lock held"));
        let (_, stale) = loader.tracker_state(&ctx, &tr, true).unwrap();
        assert!(stale.unwrap().error.contains("lock held"));
        assert_eq!(loader.fails.lock().unwrap()["c"], 2, "the streak grows");

        tr.set_down(None);
        tr.tasks.push(Task {
            id: "c-2".into(),
            ..Default::default()
        });
        let (st, stale) = loader.tracker_state(&ctx, &tr, true).unwrap();
        assert_eq!((ids(&st), stale), (vec!["c-1", "c-2"], None), "recovered");
        assert!(!loader.fails.lock().unwrap().contains_key("c"));
        assert!(!list_degraded(&loader));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A restart with the tracker down serves the disk copy; with no success ever, it errors.
    #[test]
    fn a_cold_start_falls_back_to_disk_and_errors_only_without_one() {
        let (root, ctx) = scratch("stale-cold");
        let disk = DiskCache::new(root.join("cache"));
        let fresh = || {
            let mut l = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
            l.disk = Some(disk.clone());
            l
        };
        let tr = FlakyTracker {
            tasks: vec![Task {
                id: "c-1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        tr.set_down(Some("down"));
        assert!(
            fresh().tracker_state(&ctx, &tr, false).is_err(),
            "nothing to serve"
        );
        tr.set_down(None);
        fresh().tracker_state(&ctx, &tr, false).unwrap();
        tr.set_down(Some("down"));
        std::fs::write(ctx.tracker_dir.as_ref().unwrap().join("manifest"), "root-b").unwrap();
        let (st, stale) = fresh().tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!(ids(&st), ["c-1"]);
        assert_eq!(stale.unwrap().error, "down");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn list_retry_backs_off_to_five_minutes() {
        let s: Vec<u64> = (1..=8).map(list_retry_s).collect();
        assert_eq!(s, [15, 30, 60, 120, 240, 300, 300, 300]);
    }

    /// A failed `list-branches` keeps the task's last good entries; one never read gets none.
    #[test]
    fn a_failed_registry_read_keeps_the_last_good_entries() {
        let (root, ctx) = scratch("reg");
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.disk = Some(DiskCache::new(root.join("cache")));
        let parked = |id: &str| Task {
            id: id.into(),
            labels: vec!["keep-task-open".into()],
            ..Default::default()
        };
        let entry = Branch {
            worktree: "/wt/p".into(),
            pr_url: Some("https://h/o/r/pull/1".into()),
            until: Some("2026-12-01".into()),
            ..Default::default()
        };
        let mut tr = FlakyTracker {
            tasks: vec![parked("c-1")],
            ..Default::default()
        };
        tr.branches.insert("c-1".into(), vec![entry.clone()]);
        let reg = loader.parked_registry(&ctx, &tr, &tr.tasks, None, false);
        assert_eq!(reg["c-1"], vec![entry.clone()]);

        tr.tasks.push(parked("c-2"));
        tr.set_down(Some("bash timed out after 20s"));
        let reg = loader.parked_registry(&ctx, &tr, &tr.tasks, None, true);
        assert_eq!(reg["c-1"], vec![entry.clone()], "kept, not blanked");
        assert!(reg["c-2"].is_empty(), "never read: nothing to keep");
        assert_eq!(loader.registry_of(&ctx, &tr, "c-1"), vec![entry.clone()]);

        // A new loader (a restart) finds the last good entries on disk.
        let mut again = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        again.disk = loader.disk.clone();
        assert_eq!(again.registry_of(&ctx, &tr, "c-1"), vec![entry]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Parked reads never run more than `PARKED_READS` at once.
    #[test]
    fn parked_reads_are_bounded() {
        struct Slow {
            now: std::sync::atomic::AtomicUsize,
            peak: std::sync::atomic::AtomicUsize,
        }
        impl Tracker for Slow {
            fn list(&self, _s: &str) -> Result<Vec<Task>> {
                anyhow::bail!("unused")
            }
            fn get(&self, _id: &str) -> Result<Task> {
                anyhow::bail!("unused")
            }
            fn list_branches(&self, _id: &str) -> Result<Vec<Branch>> {
                use std::sync::atomic::Ordering::SeqCst;
                let n = self.now.fetch_add(1, SeqCst) + 1;
                self.peak.fetch_max(n, SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(5));
                self.now.fetch_sub(1, SeqCst);
                Ok(vec![])
            }
            fn note(&self, _id: &str, _t: &str) -> Result<()> {
                anyhow::bail!("unused")
            }
        }
        let (root, ctx) = scratch("pool");
        let loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        let tasks: Vec<Task> = (0..20)
            .map(|i| Task {
                id: format!("c-{i}"),
                labels: vec!["keep-task-open".into()],
                ..Default::default()
            })
            .collect();
        let tr = Slow {
            now: Default::default(),
            peak: Default::default(),
        };
        let reg = loader.parked_registry(&ctx, &tr, &tasks, None, true);
        assert_eq!(reg.len(), 20);
        assert!(tr.peak.load(std::sync::atomic::Ordering::SeqCst) <= PARKED_READS);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// The first paint is built from caches alone: no tracker call, and contexts with nothing
    /// cached are left out.
    #[test]
    fn the_preview_reads_only_caches() {
        let (root, ctx) = scratch("preview");
        let disk = DiskCache::new(root.join("cache"));
        let mut loader = Loader::with_factory(None, Box::new(|_| Arc::new(FakeTracker::default())));
        loader.disk = Some(disk.clone());
        loader.fetch_prs = false;
        let entry = Branch {
            worktree: "/wt/p".into(),
            ..Default::default()
        };
        let mut tr = FlakyTracker {
            tasks: vec![Task {
                id: "c-1".into(),
                status: "in_progress".into(),
                labels: vec!["keep-task-open".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        tr.branches.insert("c-1".into(), vec![entry]);
        let (st, _) = loader.tracker_state(&ctx, &tr, false).unwrap();
        loader.parked_registry(&ctx, &tr, &st.tasks, None, false);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = calls.clone();
        let mut cold = Loader::with_factory(
            None,
            Box::new(move |_| {
                c2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Arc::new(FakeTracker::default())
            }),
        );
        cold.disk = Some(disk);
        let mut empty = ctx.clone();
        empty.name = "nothing-cached".into();
        let snap = cold.preview_of(vec![ctx, empty], vec![], &[]);
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].task.id, "c-1");
        assert_eq!(snap.tasks[0].branches[0].worktree, "/wt/p");
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no tracker"
        );
        assert!(snap.loaded_at.is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
