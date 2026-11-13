//! Load and join: contexts → tasks → branches, ticket, PR, progress, sessions, workers.
use crate::adapter::{self, TicketCache};
use crate::baton::{self, ShellTracker, Tracker};
use crate::cache::DiskCache;
use crate::config::{self, Context};
use crate::log;
use crate::model::Branch;
use crate::model::{Badge, Decision, JoinedTask, PrInfo, Session, Task};
use crate::notes::pending_decision;
use crate::pr::PrCache;
use crate::review::ReviewCache;
use crate::sessions::{self, WorkerMeta};
use crate::signature::signature;
use anyhow::Result;
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub type TrackerFactory = dyn Fn(&Context) -> Arc<dyn Tracker> + Send + Sync;

#[derive(Debug, Clone, Serialize, Default)]
pub struct Snapshot {
    pub contexts: Vec<Context>,
    pub tasks: Vec<JoinedTask>,
    pub sessions: Vec<Session>,
    pub errors: Vec<String>,
    pub loaded_at: String,
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
}

/// Tasks plus their branch registries, cached on disk per tracker write signature.
#[derive(Serialize, serde::Deserialize, Default, Clone)]
struct TrackerState {
    tasks: Vec<Task>,
    branches: HashMap<String, Vec<Branch>>,
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
        l.tickets.disk = Some(d.clone());
        l.prs.disk = Some(d.clone());
        l.disk = Some(d);
        l
    }

    pub fn with_factory(baton: Option<PathBuf>, factory: Box<TrackerFactory>) -> Self {
        Self {
            baton,
            tickets: TicketCache::default(),
            prs: PrCache::default(),
            reviews: ReviewCache::default(),
            ctx_json: Mutex::new(HashMap::new()),
            factory,
            fetch_prs: true,
            disk: None,
            sigs: Mutex::new(HashMap::new()),
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

    /// In-progress tasks and their branches; served from disk while the signature is unchanged.
    #[cfg(test)]
    fn tracker_state(&self, c: &Context, tr: &dyn Tracker, force: bool) -> Result<TrackerState> {
        self.tracker_state_with(c, tr, force, &|_| {})
    }

    /// As `tracker_state`; on a cache miss `warm` runs on the previous (stale) state while the
    /// tracker is read, so ticket and PR lookups overlap the slow tracker calls.
    fn tracker_state_with(
        &self,
        c: &Context,
        tr: &dyn Tracker,
        force: bool,
        warm: &(dyn Fn(&TrackerState) + Sync),
    ) -> Result<TrackerState> {
        let sig = c.tracker_dir.as_deref().and_then(signature);
        let dname = format!("tracker-{}", c.name);
        if let (Some(sig), Some(d), false) = (&sig, &self.disk, force)
            && let Some(st) = d.get::<TrackerState>(&dname, sig, None)
        {
            self.sigs
                .lock()
                .unwrap()
                .insert(c.name.clone(), sig.clone());
            return Ok(st);
        }
        let stale = self
            .disk
            .as_ref()
            .and_then(|d| d.get_any::<TrackerState>(&dname));
        // Warming overlaps both tracker calls; the scope ends once all are done.
        let (tasks, branches) = std::thread::scope(|sc| -> Result<_> {
            if let Some(st) = &stale {
                sc.spawn(move || warm(st));
            }
            let tasks = tr.list("in_progress")?;
            let branches: HashMap<String, Vec<Branch>> = std::thread::scope(|sc2| {
                let hs: Vec<_> = tasks
                    .iter()
                    .map(|t| {
                        sc2.spawn(move || {
                            let b = tr.list_branches(&t.id).unwrap_or_else(|e| {
                                log::warn(format!("list-branches {}: {e}", t.id));
                                vec![]
                            });
                            (t.id.clone(), b)
                        })
                    })
                    .collect();
                hs.into_iter().filter_map(|h| h.join().ok()).collect()
            });
            Ok((tasks, branches))
        })?;
        let st = TrackerState { tasks, branches };
        if let Some(sig) = sig {
            if let Some(d) = &self.disk {
                d.put(&dname, &sig, &st);
            }
            self.sigs.lock().unwrap().insert(c.name.clone(), sig);
        }
        Ok(st)
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
        let contexts = self.contexts(&mut errors);
        let (live, workers) = std::thread::scope(|sc| {
            let w = sc.spawn(|| sessions::scan_workers(&sessions::projects_dir()));
            (sessions::live_sessions(), w.join().unwrap_or_default())
        });
        let mut tasks: Vec<JoinedTask> = vec![];
        std::thread::scope(|sc| {
            let per_ctx: Vec<_> = contexts
                .iter()
                .map(|c| {
                    let (live, workers) = (&live, &workers);
                    sc.spawn(move || -> Result<Vec<JoinedTask>> {
                        let tr = self.tracker(c);
                        let warm = |old: &TrackerState| {
                            std::thread::scope(|w| {
                                for t in &old.tasks {
                                    let b = old.branches.get(&t.id).cloned().unwrap_or_default();
                                    w.spawn(move || {
                                        self.join(c, t.clone(), b, live, workers, force)
                                    });
                                }
                            });
                        };
                        let st = self.tracker_state_with(c, &*tr, force, &warm)?;
                        let joined = std::thread::scope(|sc2| {
                            let hs: Vec<_> = st
                                .tasks
                                .iter()
                                .map(|t| {
                                    let b = st.branches.get(&t.id).cloned().unwrap_or_default();
                                    sc2.spawn(move || {
                                        self.join(c, t.clone(), b, live, workers, force)
                                    })
                                })
                                .collect();
                            hs.into_iter().filter_map(|h| h.join().ok()).collect()
                        });
                        Ok(joined)
                    })
                })
                .collect();
            for (c, h) in contexts.iter().zip(per_ctx) {
                match h.join() {
                    Ok(Ok(v)) => tasks.extend(v),
                    Ok(Err(e)) => errors.push(format!("{}: tracker: {e}", c.name)),
                    Err(_) => errors.push(format!("{}: loader panicked", c.name)),
                }
            }
        });
        sort_tasks(&mut tasks);
        for e in &errors {
            log::warn(e);
        }
        Snapshot {
            contexts,
            tasks,
            sessions: live,
            errors,
            loaded_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Re-read one task (`tracker get`) and re-join it, bypassing caches.
    pub fn refresh_one(&self, c: &Context, id: &str, live: &[Session]) -> Result<JoinedTask> {
        let tr = self.tracker(c);
        let t = tr.get(id)?;
        let branches = tr.list_branches(id).unwrap_or_default();
        let workers = sessions::scan_workers(&sessions::projects_dir());
        Ok(self.join(c, t, branches, live, &workers, true))
    }

    pub fn join(
        &self,
        c: &Context,
        task: Task,
        branches: Vec<Branch>,
        live: &[Session],
        workers: &[WorkerMeta],
        force: bool,
    ) -> JoinedTask {
        let ad = c.adapter.as_ref();
        let tcfg = ad.and_then(|a| a.ticket.as_ref());
        let ticket_key = tcfg.and_then(|t| adapter::ticket_key(t, &task.labels));
        let ticket_url = match (tcfg, &ticket_key) {
            (Some(t), Some(k)) => adapter::ticket_url(t, k),
            _ => None,
        };
        let fetch_ticket = || -> (Option<crate::model::TicketInfo>, Option<String>) {
            let (Some(t), Some(k)) = (tcfg, &ticket_key) else {
                return (None, None);
            };
            if t.info.is_empty() {
                return (None, None);
            }
            match self.tickets.get(&c.workspace, t, k, force) {
                Ok(mut info) => {
                    if info.url.is_none() {
                        info.url = ticket_url.clone();
                    }
                    if let Some(ep) = info.epic.as_mut()
                        && ep.url.is_none()
                    {
                        ep.url = adapter::ticket_url(t, &ep.key);
                    }
                    (Some(info), None)
                }
                Err(e) => (None, Some(e)),
            }
        };
        // Ticket adapter, gh and the review rank are independent: run them side by side.
        let ((ticket, ticket_error), (pr, pr_error), review_waiting) = std::thread::scope(|sc| {
            let h = sc.spawn(|| {
                if self.fetch_prs {
                    self.pr_for(c, &branches, force)
                } else {
                    (None, None)
                }
            });
            let r = sc.spawn(|| {
                self.fetch_prs
                    .then(|| self.reviews.get(self.baton.as_deref(), &task, force))
                    .flatten()
            });
            let t = fetch_ticket();
            (
                t,
                h.join().unwrap_or((None, None)),
                r.join().unwrap_or(None),
            )
        });
        let progress =
            adapter::read_progress(ad.and_then(|a| a.progress.as_ref()), ticket_key.as_deref());
        // An ANSWER after the latest DECISION settles it until a new DECISION is posed.
        let decision = task
            .has_label("needs-decision")
            .then(|| pending_decision(&task.notes))
            .flatten()
            .map(|d| {
                d.unwrap_or_else(|| Decision {
                    text: "(labelled needs-decision; no DECISION note)".into(),
                    ..Default::default()
                })
            });
        let wts: Vec<&str> = branches
            .iter()
            .map(|b| b.worktree.as_str())
            .filter(|w| !w.is_empty())
            .collect();
        let session = sessions::session_for(live, &wts).cloned();
        let workers = sessions::workers_for(workers, &task.id, ticket_key.as_deref());
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
            pr,
            pr_error,
            progress,
            decision,
            session,
            workers,
            badge: Badge::Idle,
            pin_rank,
            last_change,
            review_waiting,
        };
        j.badge = badge(&j);
        j
    }

    fn pr_for(
        &self,
        c: &Context,
        branches: &[crate::model::Branch],
        force: bool,
    ) -> (Option<PrInfo>, Option<String>) {
        let slug_of = |repo: &str| {
            c.github_owner
                .as_ref()
                .filter(|_| !repo.is_empty())
                .map(|o| format!("{o}/{repo}"))
        };
        for b in branches.iter().rev() {
            if let Some(sel) = b.pr_url.clone().or_else(|| b.pr.clone()) {
                let slug = slug_of(&b.repo);
                return match self.prs.get(slug.as_deref(), &sel, force) {
                    Ok(p) => (Some(p), None),
                    Err(e) => (None, Some(e)),
                };
            }
        }
        // No PR recorded yet: ask gh by branch name; "no pull requests" is not an error.
        for b in branches.iter().rev().filter(|b| b.status == "open") {
            if let Some(slug) = slug_of(&b.repo)
                && !b.branch.is_empty()
                && let Ok(p) = self.prs.get(Some(&slug), &b.branch, force)
            {
                return (Some(p), None);
            }
        }
        (None, None)
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

/// The badge as of `today`, which decides whether a parked task is overdue.
pub fn badge_on(j: &JoinedTask, today: chrono::NaiveDate) -> Badge {
    let open_pr = j.pr.as_ref().filter(|p| p.state == "OPEN");
    if j.decision.is_some() || j.session.as_ref().is_some_and(|s| s.needs_input()) {
        Badge::NeedsYou
    } else if j.review_waiting.is_some() {
        Badge::ReviewRequested
    } else if j.overdue_on(today) {
        Badge::Overdue
    } else if open_pr.is_some_and(|p| p.checks.fail > 0) {
        Badge::CiFailing
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
    } else if j.task.has_label("ready-for-worktree-delete") || j.kept_open() {
        Badge::Parked
    } else {
        Badge::Idle
    }
}

/// Sort tier: needs-you > review requested > overdue > CI failing > ready to merge >
/// waiting on CI > working > parked > idle.
pub fn badge_rank(b: Badge) -> u8 {
    match b {
        Badge::NeedsYou => 0,
        Badge::ReviewRequested => 1,
        Badge::Overdue => 2,
        Badge::CiFailing => 3,
        Badge::ReadyToMerge => 4,
        Badge::WaitingOnCi => 5,
        Badge::WorkerRunning | Badge::Interactive => 6,
        Badge::Parked => 7,
        Badge::Idle => 8,
    }
}

/// A review waiting on you, needs-you in a pinned stream, other needs-you, overdue (pinned or
/// not), rest of the pinned streams, then everything else. Within a group: badge tier, then pin
/// order, then most recent change.
pub fn sort_tasks(v: &mut [JoinedTask]) {
    v.sort_by_key(|j| {
        let group = match (
            j.review_waiting.is_some(),
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
            pr: None,
            pr_error: None,
            progress: Default::default(),
            decision: None,
            session: None,
            workers: vec![],
            badge: Badge::Idle,
            pin_rank: None,
            last_change: None,
            review_waiting: None,
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
        let mut v = vec![a, b, c, d, e, f, g, r, done, rn];
        sort_tasks(&mut v);
        let ids: Vec<_> = v.iter().map(|j| j.task.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "review-new",
                "review-old",
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
        assert_eq!(badge(&j), Badge::Idle);
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
        j.decision = Some(Default::default());
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
        j.decision = Some(Default::default());
        assert_eq!(badge_on(&j, today), Badge::NeedsYou);
        // Not kept open: a past date alone means nothing.
        let mut k = jt("y");
        k.branches = vec![Branch {
            until: Some("2026-05-01".into()),
            ..Default::default()
        }];
        assert_eq!(badge_on(&k, today), Badge::Idle);
        k.task.labels = vec!["keep-task-open".into()];
        assert_eq!(badge_on(&k, today), Badge::Overdue);
    }

    /// In-memory tracker: records notes instead of writing anywhere.
    #[derive(Default)]
    pub struct FakeTracker {
        pub tasks: Vec<Task>,
        pub branches: HashMap<String, Vec<Branch>>,
        pub notes: Mutex<Vec<(String, String)>>,
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
        assert_eq!(j.decision.unwrap().recommendation.as_deref(), Some("yes"));
    }

    #[test]
    fn answered_decision_leaves_and_new_question_returns() {
        let ctx = Context {
            name: "c".into(),
            workspace: "/".into(),
            tracker_dir: None,
            github_owner: None,
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
        assert!(j.decision.is_none(), "answered: out of the decision view");
        assert_ne!(j.badge, Badge::NeedsYou);
        let again = format!("{answered}\nDECISION (2026-11-13): and the docs?");
        let j = loader.join(&ctx, task(&again), vec![], &[], &[], false);
        assert!(j.decision.unwrap().text.starts_with("and the docs?"));
        assert_eq!(j.badge, Badge::NeedsYou);
        // The label alone, never asked or answered, still counts.
        let j = loader.join(&ctx, task(""), vec![], &[], &[], false);
        assert!(j.decision.is_some());
    }

    /// Counts list calls so the signature cache can be observed.
    #[derive(Default)]
    struct CountingTracker {
        lists: std::sync::atomic::AtomicUsize,
    }

    impl Tracker for CountingTracker {
        fn list(&self, _s: &str) -> Result<Vec<Task>> {
            self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![Task {
                id: "ctx-9".into(),
                ..Default::default()
            }])
        }
        fn get(&self, _id: &str) -> Result<Task> {
            anyhow::bail!("unused")
        }
        fn list_branches(&self, _id: &str) -> Result<Vec<Branch>> {
            Ok(vec![Branch {
                branch: "b".into(),
                ..Default::default()
            }])
        }
        fn note(&self, _id: &str, _t: &str) -> Result<()> {
            anyhow::bail!("unused")
        }
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
            ctx_json: "{}".into(),
            adapter: None,
            adapter_error: None,
        };
        let tr = CountingTracker::default();
        let n = || tr.lists.load(std::sync::atomic::Ordering::SeqCst);
        let st = loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!((st.tasks.len(), st.branches["ctx-9"].len(), n()), (1, 1, 1));
        loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!(n(), 1, "unchanged signature is served from cache");
        std::fs::write(tdir.join("manifest"), "root-b").unwrap();
        loader.tracker_state(&ctx, &tr, false).unwrap();
        assert_eq!(n(), 2, "a write invalidates");
        loader.tracker_state(&ctx, &tr, true).unwrap();
        assert_eq!(n(), 3, "force bypasses");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
