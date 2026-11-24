//! Owns the data and the actions, and implements `Api` over them. Clients never see past it.
use crate::actions::Actions;
use crate::api::{self, Answer, Api, Code, Command, Error, Event, Query, Reply, TaskRef, Verb};
use crate::config;
use crate::core::{Loader, Snapshot, sort_tasks};
use crate::delivery::{self, Delivery, State as DState};
use crate::history;
use crate::model::{Decision, DecisionStatus, JoinedTask, Session};
use crate::project::{self, Parts};
use crate::routing::{self, Ask, Outcome, Request, Route, Router};
use crate::sessions;
use crate::transcript;
use crate::watch::{self, Change};
use std::collections::hash_map::RandomState;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hash};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

const DEBOUNCE: Duration = Duration::from_secs(2);
const SIG_CHECK: Duration = Duration::from_millis(500);
/// How often records whose claude process is gone are ended (`Actions::sweep`).
const SWEEP_EVERY: Duration = Duration::from_secs(60);
/// How often unacknowledged deliveries are looked at (transcript check, alarms).
const DELIVERY_CHECK: Duration = Duration::from_secs(5);
/// How often the state is rebuilt with no change behind it: labels that depend on the time.
const REPUBLISH: Duration = Duration::from_secs(5);
/// The driver's longest sleep between ticks.
const TICK: Duration = Duration::from_millis(150);
const NOTICES: usize = 20;
/// How long a command's `request_id` is remembered.
const REQUEST_TTL: Duration = Duration::from_secs(600);
const REQUESTS: usize = 256;
const PANE_ONLY: &str = "only its pane can answer that";

enum Msg {
    /// The cache-only first view; dropped once a real snapshot has landed.
    Preview(Box<Snapshot>),
    Snapshot(Box<Snapshot>),
    /// A background sweep ended sessions.
    Swept(String),
    /// Delivery records changed (released, found in a transcript): re-read them.
    Deliveries(Option<String>),
    Change(Change),
}

#[derive(Default)]
struct Inner {
    snap: Arc<Snapshot>,
    /// Oldest first; see "Answer delivery" in DESIGN.md.
    deliveries: Vec<Delivery>,
    phase: api::Phase,
    loading: bool,
    pending: Option<Instant>,
    /// When to retry a failed tracker read (backoff from the loader).
    retry_at: Option<Instant>,
    last_load: Option<Instant>,
    sig_check: bool,
    last_sig_check: Option<Instant>,
    /// Off in `--dry-run` and in tests; `start` turns it on.
    sweep: bool,
    last_sweep: Option<Instant>,
    last_delivery_check: Option<Instant>,
    last_publish: Option<Instant>,
    /// Deliveries already announced as not received, so each is announced once.
    alarmed: HashSet<String>,
    /// Held deliveries being released, so a burst of session events starts one attempt.
    releasing: HashSet<String>,
    notices: VecDeque<api::Notice>,
    next_seq: u64,
}

impl Inner {
    fn live(&self, id: &str) -> Option<&Session> {
        self.snap.sessions.iter().find(|s| s.session_id == id)
    }

    fn task(&self, r: &TaskRef) -> Option<&JoinedTask> {
        self.snap
            .tasks
            .iter()
            .find(|t| t.context == r.context && t.task.id == r.id)
    }

    fn context(&self, name: &str) -> Option<&config::Context> {
        self.snap.contexts.iter().find(|c| c.name == name)
    }

    fn notice(&mut self, text: String) {
        self.next_seq += 1;
        self.notices.push_back(api::Notice {
            seq: self.next_seq,
            at: crate::store::now(),
            text,
        });
        while self.notices.len() > NOTICES {
            self.notices.pop_front();
        }
    }
}

/// Reads session records; a seam so tests choose them.
pub struct Records {
    pub live: Box<dyn Fn() -> Vec<Session> + Send + Sync>,
    pub closed: Box<dyn Fn() -> Vec<Session> + Send + Sync>,
}

impl Default for Records {
    fn default() -> Self {
        Self {
            live: Box::new(sessions::live_sessions),
            closed: Box::new(sessions::closed_sessions),
        }
    }
}

type Remembered = Arc<Mutex<Option<Result<Reply, Error>>>>;

struct Request1 {
    at: Instant,
    verb: Verb,
    result: Remembered,
}

struct Shared {
    inner: Mutex<Inner>,
    loader: Arc<Loader>,
    router: Arc<Router>,
    actions: Arc<Actions>,
    records: Records,
    tx: Sender<Msg>,
    events: broadcast::Sender<Event>,
    state: RwLock<Arc<api::State>>,
    /// Held while a state document is built and stored, so revs follow the data's order.
    publishing: Mutex<()>,
    instance: String,
    started: Instant,
    dry_run: bool,
    /// Keys transcript references and confirm tokens; new in each process.
    keys: RandomState,
    transcripts: Mutex<HashMap<String, PathBuf>>,
    requests: Mutex<HashMap<String, Request1>>,
}

#[derive(Clone)]
pub struct Engine(Arc<Shared>);

impl Engine {
    /// An engine that does nothing by itself until `start`; returns its message queue.
    fn build(
        loader: Loader,
        router: Router,
        actions: Actions,
        records: Records,
    ) -> (Engine, Receiver<Msg>) {
        let (tx, rx) = channel();
        let keys = RandomState::new();
        let instance = format!(
            "{:016x}",
            keys.hash_one((std::process::id(), chrono::Utc::now().timestamp_nanos_opt()))
        );
        let state = api::State {
            instance: instance.clone(),
            rev: 1,
            ..Default::default()
        };
        let e = Engine(Arc::new(Shared {
            inner: Mutex::default(),
            dry_run: router.dry_run,
            loader: Arc::new(loader),
            router: Arc::new(router),
            actions: Arc::new(actions),
            records,
            tx,
            events: broadcast::channel(64).0,
            state: RwLock::new(Arc::new(state)),
            publishing: Mutex::new(()),
            instance,
            started: Instant::now(),
            keys,
            transcripts: Mutex::default(),
            requests: Mutex::default(),
        }));
        (e, rx)
    }

    /// The real engine: cached view at once, then the first load, watching files from here on.
    pub fn start(dry_run: bool) -> Engine {
        let (e, rx) = Self::build(
            Loader::new(),
            Router::new(dry_run),
            Actions::new(dry_run),
            Records::default(),
        );
        e.0.inner.lock().unwrap().sweep = !dry_run;
        e.preview();
        e.reload(false);
        let e2 = e.clone();
        std::thread::spawn(move || e2.drive(rx));
        e
    }

    fn drive(&self, rx: Receiver<Msg>) {
        let mut errs = vec![];
        let trackers: Vec<_> = self
            .0
            .loader
            .contexts(&mut errs)
            .into_iter()
            .filter_map(|c| c.tracker_dir)
            .collect();
        let store = crate::store::Store::open();
        // Hooks create the root on first write; make it now so it is watched from the start.
        let _ = std::fs::create_dir_all(store.root.join("sessions"));
        let (wtx, wrx) = channel::<Change>();
        let _watchers = watch::start(trackers, store.live_dirs(), wtx);
        let tx = self.0.tx.clone();
        std::thread::spawn(move || {
            while let Ok(c) = wrx.recv() {
                if tx.send(Msg::Change(c)).is_err() {
                    return;
                }
            }
        });
        loop {
            match rx.recv_timeout(TICK) {
                Ok(m) => self.on_msg(m),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.tick();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.inner.lock().unwrap()
    }

    fn preview(&self) {
        let (loader, tx) = (self.0.loader.clone(), self.0.tx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Msg::Preview(Box::new(loader.preview_blocking())));
        });
    }

    fn reload(&self, force: bool) {
        {
            let mut i = self.lock();
            if i.loading {
                i.pending = Some(Instant::now());
                return;
            }
            i.loading = true;
            i.last_load = Some(Instant::now());
        }
        self.publish();
        let (loader, tx) = (self.0.loader.clone(), self.0.tx.clone());
        std::thread::spawn(move || {
            // A load that panics must still end, or no later one would start.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                loader.load_blocking(force)
            }));
            let _ = tx.send(Msg::Snapshot(Box::new(r.unwrap_or_default())));
        });
    }

    /// Tracker events only count when the write signature moved (reads touch files too).
    fn check_sigs(&self) {
        let contexts = {
            let mut i = self.lock();
            if !i.sig_check || i.last_sig_check.is_some_and(|t| t.elapsed() < SIG_CHECK) {
                return;
            }
            i.sig_check = false;
            i.last_sig_check = Some(Instant::now());
            i.snap.clone()
        };
        let now = Loader::current_sigs(&contexts.contexts);
        let known = self.0.loader.sigs.lock().unwrap().clone();
        if now.iter().any(|(k, v)| known.get(k) != Some(v)) {
            self.lock().pending.get_or_insert_with(Instant::now);
        }
    }

    /// Session files change on every tool call: re-read them and re-badge, no tracker calls.
    fn rematch_sessions(&self) {
        let live = (self.0.records.live)();
        {
            let mut i = self.lock();
            let snap = Arc::make_mut(&mut i.snap);
            for t in &mut snap.tasks {
                t.session = sessions::session_for(&live, &t.worktrees()).cloned();
                crate::prompts::apply(t);
                t.badge = crate::core::badge(t);
            }
            snap.sessions = live;
            sort_tasks(&mut snap.tasks);
        }
        self.load_deliveries();
        self.release_held();
        self.publish();
    }

    fn load_deliveries(&self) {
        if let Some(st) = &self.0.router.store {
            let all = delivery::all(st);
            self.lock().deliveries = all;
        }
    }

    /// Held answers whose session has left its dialog (or gone): try sending them now.
    fn release_held(&self) {
        let ready: Vec<String> = {
            let mut i = self.lock();
            let v: Vec<String> = i
                .deliveries
                .iter()
                .filter(|d| d.state == DState::Held && !i.releasing.contains(&d.id))
                .filter(|d| {
                    i.live(&d.session_id)
                        .is_none_or(|s| delivery::open_prompt(s).is_none())
                })
                .map(|d| d.id.clone())
                .collect();
            i.releasing.extend(v.iter().cloned());
            v
        };
        for id in ready {
            let (router, tx) = (self.0.router.clone(), self.0.tx.clone());
            std::thread::spawn(move || {
                let m = router.release(&id);
                let _ = tx.send(Msg::Deliveries(m));
            });
        }
    }

    /// Cross-check pending deliveries against their session's transcript, then announce each
    /// one still unacknowledged past its window, once.
    fn check_deliveries(&self) {
        self.lock().releasing.clear();
        self.load_deliveries();
        self.release_held();
        let Some(st) = self.0.router.store.clone() else {
            return;
        };
        let at = chrono::Utc::now();
        let mut i = self.lock();
        let look: Vec<(String, PathBuf, String, String)> = i
            .deliveries
            .iter()
            .filter(|d| {
                d.pending() && d.age(at) >= DELIVERY_CHECK && d.age(at) < delivery::LOOK_WINDOW
            })
            .filter_map(|d| {
                let tp = i.live(&d.session_id)?.transcript_path.clone()?;
                let since = d.sent_at.clone()?;
                Some((d.id.clone(), tp.into(), d.payload.clone(), since))
            })
            .collect();
        if !look.is_empty() {
            let tx = self.0.tx.clone();
            std::thread::spawn(move || {
                if mark_found(&st, look) {
                    let _ = tx.send(Msg::Deliveries(None));
                }
            });
        }
        let alarms: Vec<(String, String)> = i
            .deliveries
            .iter()
            .filter_map(|d| {
                let v = d.view(i.live(&d.session_id), at);
                (v.alarm && !i.alarmed.contains(&d.id))
                    .then(|| (d.id.clone(), format!("{}: {}", d.task, v.label)))
            })
            .collect();
        i.alarmed.extend(alarms.iter().map(|(id, _)| id.clone()));
        if let Some((_, first)) = alarms.first() {
            let more = match alarms.len() {
                1 => String::new(),
                n => format!(" (+{} more)", n - 1),
            };
            i.notice(format!("{first}{more} — A in the task's detail resends"));
        }
    }

    fn tick(&self) {
        self.check_sigs();
        let due = |last: &mut Option<Instant>, every: Duration| {
            let d = last.is_none_or(|t| t.elapsed() >= every);
            if d {
                *last = Some(Instant::now());
            }
            d
        };
        let sweep = {
            let mut i = self.lock();
            i.sweep && due(&mut i.last_sweep, SWEEP_EVERY)
        };
        if sweep {
            self.sweep();
        }
        let check = due(&mut self.lock().last_delivery_check, DELIVERY_CHECK);
        if check {
            self.check_deliveries();
        }
        let reload = {
            let mut i = self.lock();
            let retry = !i.loading && i.retry_at.is_some_and(|t| t <= Instant::now());
            if retry {
                i.retry_at = None;
            }
            let settled = !i.loading
                && i.pending
                    .is_some_and(|p| p.elapsed() >= Duration::from_millis(300))
                && i.last_load.is_none_or(|l| l.elapsed() >= DEBOUNCE);
            if settled {
                i.pending = None;
            }
            retry || settled
        };
        if reload {
            self.reload(false);
        }
        let old = self
            .lock()
            .last_publish
            .is_none_or(|t| t.elapsed() >= REPUBLISH);
        if old || check {
            self.publish();
        }
    }

    fn sweep(&self) {
        if let Some(st) = self.0.router.store.clone() {
            std::thread::spawn(move || {
                delivery::prune(&st);
                crate::turns::prune(&st);
                if let Err(e) = crate::prompts::sync_real(&st, false) {
                    crate::log::warn(format!("prompts sync: {e:#}"));
                }
            });
        }
        let (c, tx) = (self.0.actions.clone(), self.0.tx.clone());
        std::thread::spawn(move || match c.sweep() {
            Ok(out) if !out.is_empty() => {
                let _ = tx.send(Msg::Swept(format!("swept: {}", out.join(", "))));
            }
            Ok(_) => {}
            Err(e) => crate::log::warn(format!("sweep: {e:#}")),
        });
    }

    fn on_msg(&self, m: Msg) {
        match m {
            Msg::Preview(s) => {
                let mut i = self.lock();
                if i.phase == api::Phase::Loaded {
                    return;
                }
                i.phase = api::Phase::Preview;
                i.snap = Arc::new(*s);
            }
            Msg::Snapshot(s) => {
                let mut i = self.lock();
                i.loading = false;
                i.phase = api::Phase::Loaded;
                i.retry_at = s
                    .retry_in_s
                    .map(|n| Instant::now() + Duration::from_secs(n));
                i.snap = Arc::new(*s);
            }
            Msg::Swept(s) => {
                self.lock().notice(s);
                self.rematch_sessions();
            }
            Msg::Deliveries(s) => {
                self.load_deliveries();
                if let Some(s) = s {
                    self.lock().notice(s);
                }
            }
            Msg::Change(Change::Sessions) => return self.rematch_sessions(),
            Msg::Change(Change::Tasks) => {
                self.lock().sig_check = true;
                return;
            }
        }
        self.publish();
    }

    /// Rebuild the state document; when anything in it differs, bump `rev` and say what.
    fn publish(&self) {
        let _one = self.0.publishing.lock().unwrap();
        let (snap, deliveries, phase, loading, notices) = {
            let mut i = self.lock();
            i.last_publish = Some(Instant::now());
            (
                i.snap.clone(),
                i.deliveries.clone(),
                i.phase,
                i.loading,
                i.notices.iter().cloned().collect(),
            )
        };
        let panes = snap
            .sessions
            .iter()
            .map(|s| (s.session_id.clone(), self.0.router.pane_label(s)))
            .collect();
        let mut next = project::state(&Parts {
            snap: &snap,
            deliveries: &deliveries,
            health: &self.0.loader.health.rows(),
            panes: &panes,
            phase,
            loading,
            notices,
            now: chrono::Utc::now(),
        });
        let old = self.state();
        let sections = old.changed(&next);
        if sections.is_empty() {
            return;
        }
        next.instance = self.0.instance.clone();
        next.rev = old.rev + 1;
        let rev = next.rev;
        *self.0.state.write().unwrap() = Arc::new(next);
        // No subscriber is not an error.
        let _ = self.0.events.send(Event {
            instance: self.0.instance.clone(),
            rev,
            sections,
        });
    }

    // Transcript references.

    fn reference(&self, path: &str) -> String {
        let r = format!("{:016x}", self.0.keys.hash_one(("transcript", path)));
        self.0
            .transcripts
            .lock()
            .unwrap()
            .insert(r.clone(), path.into());
        r
    }

    /// The file behind a reference: a `.jsonl` really under the projects directory.
    fn dereference(&self, r: &str) -> Result<PathBuf, Error> {
        let p = self
            .0
            .transcripts
            .lock()
            .unwrap()
            .get(r)
            .cloned()
            .ok_or_else(|| Error::not_found("such transcript"))?;
        under(&p, &sessions::projects_dir())
    }

    // Queries.

    fn task_query<T>(
        &self,
        r: &TaskRef,
        f: impl FnOnce(JoinedTask, Option<config::Context>, Vec<Session>) -> T,
    ) -> Result<T, Error> {
        let (t, ctx, mut all) = {
            let i = self.lock();
            let t = i.task(r).cloned().ok_or_else(|| no_task(r))?;
            (t, i.context(&r.context).cloned(), i.snap.sessions.clone())
        };
        all.extend((self.0.records.closed)());
        Ok(f(t, ctx, all))
    }

    fn detail(&self, r: &TaskRef) -> Result<api::TaskDetail, Error> {
        let (route, description) = {
            let i = self.lock();
            let t = i.task(r).ok_or_else(|| no_task(r))?;
            (self.route(&i, t), t.task.description.clone())
        };
        let st = self.state();
        Ok(api::TaskDetail {
            task: st.task(r).cloned().ok_or_else(|| no_task(r))?,
            description,
            deliveries: st
                .deliveries
                .iter()
                .filter(|d| d.context == r.context && d.task == r.id)
                .cloned()
                .collect(),
            route: route
                .map(|x| project::route(&x, |s| self.0.router.pane_label(s)))
                .unwrap_or_default(),
        })
    }

    fn route(&self, i: &Inner, t: &JoinedTask) -> Option<Route> {
        let c = i.context(&t.context)?;
        Some(routing::plan(
            &t.worktrees(),
            &c.workspace.to_string_lossy(),
            &i.snap.sessions,
        ))
    }

    fn live_session(&self, id: &str) -> Result<Session, Error> {
        self.lock().live(id).cloned().ok_or_else(|| no_session(id))
    }

    fn any_session(&self, id: &str) -> Result<Session, Error> {
        self.live_session(id).or_else(|e| {
            (self.0.records.closed)()
                .into_iter()
                .find(|s| s.session_id == id)
                .ok_or(e)
        })
    }

    fn closed(&self) -> Vec<api::Session> {
        let snap = self.lock().snap.clone();
        (self.0.records.closed)()
            .iter()
            .map(|s| project::session(s, &snap, None))
            .collect()
    }

    fn log(&self, id: &str) -> Result<api::Log, Error> {
        // Checked before it becomes part of a file name.
        if !crate::store::valid_id(id) {
            return Err(no_session(id));
        }
        let s = self.any_session(id)?;
        let p = crate::store::Store::open().log_path(&s.session_id);
        Ok(api::Log {
            text: history::tail(&p, 256 * 1024)
                .unwrap_or_else(|| format!("{}: unreadable", p.display())),
            transcript: history::session_transcript(&s)
                .map(|p| self.reference(&p.to_string_lossy())),
        })
    }

    fn read_transcript(&self, r: &str, around: Option<&str>) -> Result<api::Transcript, Error> {
        let at = match around {
            None => None,
            Some(a) => Some(
                crate::util::parse_ts(a)
                    .ok_or_else(|| Error::new(Code::BadRequest, "around is not a timestamp"))?,
            ),
        };
        let p = self.dereference(r)?;
        let t = transcript::load(&p).map_err(Error::source)?;
        let after = |e: &transcript::Entry| {
            let ts = e.ts.as_deref().and_then(crate::util::parse_ts);
            at.zip(ts).is_some_and(|(at, ts)| ts >= at)
        };
        Ok(api::Transcript {
            at: t.entries.iter().position(after),
            skipped_bytes: t.skipped_bytes,
            entries: t
                .entries
                .into_iter()
                .map(|e| project::entry(e, |p| self.reference(p)))
                .collect(),
        })
    }

    // Commands.

    /// A command that needs a yes passes only with the token for exactly this command, what it
    /// acts on (`shown`) and this prompt.
    fn confirmed(&self, c: &Command, shown: impl Hash, prompt: String) -> Result<(), Error> {
        let token = self.token(&c.verb, shown, &prompt);
        if c.confirm.as_ref() == Some(&token) {
            return Ok(());
        }
        let mut e = Error::new(Code::NeedsConfirm, prompt);
        e.confirm = Some(token);
        Err(e)
    }

    fn token(&self, verb: &Verb, shown: impl Hash, prompt: &str) -> String {
        let of = (format!("{verb:?}"), shown, prompt);
        format!("{:016x}", self.0.keys.hash_one(of))
    }

    /// Run a send unconfirmed, and again as confirmed when the client holds its token.
    fn send_with(
        &self,
        c: &Command,
        send: impl Fn(Option<Ask>) -> anyhow::Result<Outcome>,
    ) -> Result<String, Error> {
        let mut yes = None;
        // A pane that changes between the two asks again, about what it is now.
        loop {
            match send(yes.clone()).map_err(Error::source)? {
                Outcome::Delivered { summary } => return Ok(summary),
                Outcome::NeedsConfirm(ask) => {
                    let prompt = format!("{} is {} — send anyway?", ask.target, ask.status);
                    self.confirmed(c, &ask, prompt)?;
                    yes = Some(ask);
                }
            }
        }
    }

    /// Relay `text` to the task's route; `reply` is the tracker decision it answers.
    fn deliver(
        &self,
        c: &Command,
        task: &TaskRef,
        text: &str,
        reply: Option<routing::Reply>,
        resend_of: Option<String>,
    ) -> Result<Reply, Error> {
        let (tracker, route) = {
            let i = self.lock();
            let t = i.task(task).ok_or_else(|| no_task(task))?;
            let (Some(ctx), Some(route)) = (i.context(&t.context), self.route(&i, t)) else {
                return Err(Error::new(Code::NotFound, "unknown context"));
            };
            (self.0.loader.tracker(ctx), route)
        };
        let before: HashSet<String> = self.stored().into_iter().map(|d| d.id).collect();
        let summary = self.send_with(c, |confirmed| {
            let req = Request {
                context: task.context.clone(),
                task_id: task.id.clone(),
                text: text.to_string(),
                confirmed,
                resend_of: resend_of.clone(),
                reply: reply.clone(),
            };
            self.0.router.deliver(&*tracker, &req, &route)
        })?;
        self.load_deliveries();
        self.publish();
        let made = self
            .lock()
            .deliveries
            .iter()
            .rev()
            .find(|d| d.context == task.context && d.task == task.id && !before.contains(&d.id))
            .map(|d| d.id.clone());
        let st = self.state();
        Ok(Reply {
            summary,
            delivery: made.and_then(|id| st.deliveries.iter().find(|d| d.id == id).cloned()),
        })
    }

    fn stored(&self) -> Vec<Delivery> {
        self.0
            .router
            .store
            .as_ref()
            .map(delivery::all)
            .unwrap_or_default()
    }

    fn answer(
        &self,
        c: &Command,
        task: &TaskRef,
        text: &str,
        decision: Option<&str>,
        ask: bool,
    ) -> Result<Reply, Error> {
        typeable(text)?;
        let id = {
            let i = self.lock();
            let t = i.task(task).ok_or_else(|| no_task(task))?;
            match decision {
                None => None,
                Some(key) => {
                    let d = unresolved(t, key)?;
                    if d.pane_only() {
                        return Err(Error::new(Code::BadRequest, PANE_ONLY));
                    }
                    d.id.clone()
                }
            }
        };
        if ask && id.is_none() {
            return Err(Error::new(
                Code::BadRequest,
                "no tracker decision to ask about",
            ));
        }
        let reply = id.map(|d| routing::Reply { decision: d, ask });
        self.deliver(c, task, text, reply, None)
    }

    /// Send a failed or unacknowledged answer again, to the same decision as the same kind.
    fn resend(&self, c: &Command, id: &str) -> Result<Reply, Error> {
        self.load_deliveries();
        let d = {
            let i = self.lock();
            let d = i
                .deliveries
                .iter()
                .find(|d| d.id == id)
                .ok_or_else(|| Error::not_found(format!("delivery {id}")))?;
            let v = project::deliveries(&i.deliveries, &i.snap.sessions, chrono::Utc::now())
                .into_iter()
                .find(|v| v.id == id && v.resendable);
            if v.is_none() {
                return Err(Error::new(
                    Code::BadRequest,
                    "nothing to resend: only a task's latest answer, and only if it did not arrive",
                ));
            }
            if let Some(key) = &d.decision {
                let task = TaskRef::new(&d.context, &d.task);
                unresolved(i.task(&task).ok_or_else(|| no_task(&task))?, key)?;
            }
            d.clone()
        };
        let reply = d.decision.clone().map(|decision| routing::Reply {
            decision,
            ask: d.kind.as_deref() == Some("question"),
        });
        let task = TaskRef::new(&d.context, &d.task);
        self.deliver(c, &task, &d.text, reply, Some(d.id))
    }

    fn pin(&self, task: &TaskRef, target: api::PinTarget, pinned: bool) -> Result<Reply, Error> {
        let mut i = self.lock();
        let t = i.task(task).ok_or_else(|| no_task(task))?;
        let what = match target {
            api::PinTarget::Ticket => "ticket",
            api::PinTarget::Epic => "epic",
        };
        let Some(pin) = project::pin_of(t, target) else {
            return Err(Error::new(Code::BadRequest, format!("no {what} to pin")));
        };
        let key = pin.epic.clone().or(pin.ticket.clone()).unwrap_or_default();
        let Some(c) = i.context(&task.context).cloned() else {
            return Err(Error::new(Code::NotFound, "unknown context"));
        };
        let cur = c
            .adapter
            .as_ref()
            .map(|a| a.priorities.clone())
            .unwrap_or_default();
        let done = if pinned { "pinned" } else { "unpinned" };
        if self.0.dry_run {
            return Ok(Reply::new(format!("dry-run: would be {done}: {key}")));
        }
        if cur.contains(&pin) != pinned {
            let next = config::toggle_pin(&cur, pin);
            config::save_priorities(&c, &next)
                .map_err(|e| Error::source(format!("save priorities: {e}")))?;
            // A second pin before the reload must build on this one.
            let snap = Arc::make_mut(&mut i.snap);
            if let Some(a) = snap
                .contexts
                .iter_mut()
                .find(|x| x.name == c.name)
                .and_then(|x| x.adapter.as_mut())
            {
                a.priorities = next;
            }
            drop(i);
            self.reload(false);
        }
        Ok(Reply::new(format!("{done} {key}")))
    }

    /// Run a session action, then show what it changed.
    fn act(&self, f: impl FnOnce(&Actions) -> anyhow::Result<String>) -> Result<Reply, Error> {
        let r = f(&self.0.actions);
        self.rematch_sessions();
        r.map(Reply::new).map_err(Error::source)
    }

    fn close(&self, c: &Command, id: &str, keep_pane: bool) -> Result<Reply, Error> {
        let s = self.live_session(id)?;
        let prompt = format!(
            "close {} ({})?",
            s.tmux_target.as_deref().unwrap_or_default(),
            s.worktree
        );
        // A yes to closing the pane too covers keeping it, but not the reverse.
        let whole = Verb::Close {
            session: id.into(),
            keep_pane: false,
        };
        if c.confirm != Some(self.token(&whole, s.claude_pid, &prompt)) {
            self.confirmed(c, s.claude_pid, prompt)?;
        }
        self.act(|a| a.close(&s, !keep_pane))
    }

    fn reopen(&self, c: &Command, id: &str) -> Result<Reply, Error> {
        let s = (self.0.records.closed)()
            .into_iter()
            .find(|s| s.session_id == id)
            .ok_or_else(|| no_session(id))?;
        let prompt = format!(
            "reopen {} with claude --resume in tmux {}?",
            id.get(..8).unwrap_or(id),
            s.tmux_session.as_deref().unwrap_or_default()
        );
        self.confirmed(c, &s.worktree, prompt)?;
        self.act(|a| a.reopen(&s))
    }

    fn restore(&self, c: &Command) -> Result<Reply, Error> {
        let (_, file) = self.0.actions.latest_snapshot().map_err(Error::source)?;
        let names: Vec<&str> = file.sessions.iter().map(|s| &*s.tmux_session).collect();
        let prompt = format!(
            "restore the snapshot of {} (relaunches {}: {})?",
            file.created.get(..16).unwrap_or(&file.created),
            names.len(),
            crate::util::truncate(&names.join(", "), 120)
        );
        let shown = serde_json::to_string(&file).map_err(|e| Error::source(e.to_string()))?;
        self.confirmed(c, shown, prompt)?;
        self.act(|a| a.snapshot_restore(&file))
    }

    fn refresh(&self, task: &TaskRef, force: bool) -> Result<Reply, Error> {
        let (c, live) = {
            let i = self.lock();
            i.task(task).ok_or_else(|| no_task(task))?;
            let c = i
                .context(&task.context)
                .cloned()
                .ok_or_else(|| Error::new(Code::NotFound, "unknown context"))?;
            (c, i.snap.sessions.clone())
        };
        let j = self
            .0
            .loader
            .refresh_one(&c, &task.id, &live, force)
            .map_err(|e| Error::source(format!("refresh {}: {e}", task.id)))?;
        self.replace(j);
        Ok(Reply::new("refreshed"))
    }

    fn replace(&self, j: JoinedTask) {
        {
            let mut i = self.lock();
            let snap = Arc::make_mut(&mut i.snap);
            if let Some(t) = snap
                .tasks
                .iter_mut()
                .find(|t| t.task.id == j.task.id && t.context == j.context)
            {
                *t = j;
            }
            sort_tasks(&mut snap.tasks);
        }
        self.publish();
    }

    fn run(&self, c: &Command) -> Result<Reply, Error> {
        match &c.verb {
            Verb::Refresh { force, task: None } => {
                if *force {
                    self.0.loader.forget_contexts();
                }
                self.reload(true);
                Ok(Reply::new("reloading…"))
            }
            Verb::Refresh {
                force,
                task: Some(t),
            } => self.refresh(t, *force),
            Verb::Answer {
                task,
                text,
                decision,
                ask,
            } => self.answer(c, task, text, decision.as_deref(), *ask),
            Verb::Resend { delivery } => self.resend(c, delivery),
            Verb::Pin {
                task,
                target,
                pinned,
            } => self.pin(task, *target, *pinned),
            Verb::Send { session, text } => {
                typeable(text)?;
                let s = self.live_session(session)?;
                self.send_with(c, |yes| self.0.router.send_to_pane(&s, text, yes.as_ref()))
                    .map(Reply::new)
            }
            Verb::Describe { session, text } => {
                one_line(text)?;
                let s = self.any_session(session)?;
                self.act(|a| a.describe(&s, text))
            }
            Verb::Close { session, keep_pane } => self.close(c, session, *keep_pane),
            Verb::Reopen { session } => self.reopen(c, session),
            Verb::SnapshotSave => self.act(|a| a.snapshot_save()),
            Verb::SnapshotRestore => self.restore(c),
        }
    }

    /// The remembered slot for a `request_id`, or why it cannot be used.
    fn slot(&self, id: &str, verb: &Verb) -> Result<Remembered, Error> {
        let mut m = self.0.requests.lock().unwrap();
        m.retain(|_, r| r.at.elapsed() < REQUEST_TTL);
        if m.len() >= REQUESTS && !m.contains_key(id) {
            let oldest = m.iter().min_by_key(|(_, r)| r.at).map(|(k, _)| k.clone());
            m.remove(&oldest.unwrap_or_default());
        }
        let r = m.entry(id.to_string()).or_insert_with(|| Request1 {
            at: Instant::now(),
            verb: verb.clone(),
            result: Remembered::default(),
        });
        if r.verb != *verb {
            return Err(Error::new(
                Code::BadRequest,
                "request_id was already used for another command",
            ));
        }
        Ok(r.result.clone())
    }
}

impl Api for Engine {
    fn state(&self) -> Arc<api::State> {
        self.0.state.read().unwrap().clone()
    }

    fn query(&self, q: Query) -> Result<Answer, Error> {
        Ok(match q {
            Query::Server => Answer::Server(api::ServerInfo {
                api: api::VERSION.into(),
                instance: self.0.instance.clone(),
                build: crate::health::build_line(),
                version: env!("CARGO_PKG_VERSION").into(),
                commit: env!("FERMATA_COMMIT").into(),
                dry_run: self.0.dry_run,
                read_only: false,
                uptime_s: self.0.started.elapsed().as_secs(),
                paths: crate::health::paths(self.0.loader.baton.as_deref())
                    .into_iter()
                    .map(|(name, path)| api::NamedPath {
                        name: name.into(),
                        path,
                    })
                    .collect(),
            }),
            Query::Task(r) => Answer::Task(Box::new(self.detail(&r)?)),
            Query::Timeline(r) => {
                let items = self.task_query(&r, |t, _, all| history::timeline(&t, &all))?;
                Answer::Timeline(api::Timeline {
                    items: items
                        .into_iter()
                        .map(|i| project::item(i, |p| self.reference(p)))
                        .collect(),
                })
            }
            Query::Conversation(r) => {
                let entries = self.task_query(&r, |t, c, all| {
                    crate::conversation::load(&self.0.loader, c.as_ref(), &t, &all)
                })?;
                Answer::Conversation(api::Conversation {
                    entries: entries
                        .into_iter()
                        .map(|e| project::entry(e, |p| self.reference(p)))
                        .collect(),
                })
            }
            Query::Sessions { closed } => Answer::Sessions(api::Sessions {
                sessions: if closed {
                    self.closed()
                } else {
                    self.state().sessions.clone()
                },
            }),
            Query::Log { session } => Answer::Log(self.log(&session)?),
            Query::Pane { session } => {
                let s = self.live_session(&session)?;
                let p = self.0.router.resolve(&s).map_err(Error::source)?;
                Answer::Pane(api::Pane {
                    label: p.label(),
                    id: p.id,
                })
            }
            Query::Transcript { reference, around } => {
                Answer::Transcript(self.read_transcript(&reference, around.as_deref())?)
            }
        })
    }

    fn command(&self, c: Command) -> Result<Reply, Error> {
        let Some(id) = &c.request_id else {
            return self.run(&c);
        };
        let slot = self.slot(id, &c.verb)?;
        // Held while it runs, so a retry that overtakes it waits instead of running too.
        let mut done = slot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(r) = &*done {
            return r.clone();
        }
        let r = self.run(&c);
        // Asking for a yes did nothing yet: the repeat with the token must run.
        if !matches!(&r, Err(e) if e.code == Code::NeedsConfirm) {
            *done = Some(r.clone());
        }
        r
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }
}

/// Control characters are keys in a pane: Esc and Ctrl-C interrupt, and a newline submits.
fn one_line(text: &str) -> Result<(), Error> {
    match text.contains(char::is_control) {
        true => Err(Error::new(
            Code::BadRequest,
            "text must be one line without control characters",
        )),
        false => Ok(()),
    }
}

fn typeable(text: &str) -> Result<(), Error> {
    if text.trim().is_empty() {
        return Err(Error::new(Code::BadRequest, "empty — not sent"));
    }
    one_line(text)
}

/// The task's decision `key`, unless it is settled.
fn unresolved<'a>(t: &'a JoinedTask, key: &str) -> Result<&'a Decision, Error> {
    t.decisions
        .iter()
        .filter(|d| d.status != DecisionStatus::Resolved)
        .find(|d| d.key() == key || d.id.as_deref() == Some(key))
        .ok_or_else(|| {
            let what = format!("decision {key} of {} is no longer unresolved", t.task.id);
            Error::new(Code::Gone, what)
        })
}

fn no_task(r: &TaskRef) -> Error {
    Error::not_found(format!("task {r}"))
}

fn no_session(id: &str) -> Error {
    Error::not_found(format!("session {id}"))
}

/// `p` with symlinks followed, when it is a `.jsonl` inside `dir`.
fn under(p: &Path, dir: &Path) -> Result<PathBuf, Error> {
    let real = p.canonicalize().map_err(Error::source)?;
    let dir = dir.canonicalize().map_err(Error::source)?;
    if !real.starts_with(&dir) || real.extension().is_none_or(|e| e != "jsonl") {
        return Err(Error::new(
            Code::Forbidden,
            "not a transcript under the Claude projects directory",
        ));
    }
    Ok(real)
}

/// Mark the deliveries whose text their transcript shows; whether any was.
fn mark_found(st: &crate::store::Store, look: Vec<(String, PathBuf, String, String)>) -> bool {
    let mut any = false;
    for (id, tp, payload, since) in look {
        if delivery::in_transcript(&tp, &payload, &since) {
            let r = delivery::update(st, &id, |d| {
                d.received_at.is_none() && {
                    d.received_at = Some(crate::store::now());
                    d.received_via = Some("transcript".into());
                    true
                }
            });
            if r.is_ok_and(|b| b)
                && let Some(d) = delivery::read(st, &id)
            {
                crate::log::info(d.log_line("received"));
                any = true;
            }
        }
    }
    any
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::actions::FakeSystem;
    use crate::core::tests::{FakeTracker, jt};
    use crate::model::Branch;
    use crate::routing::tests::{Sent, ids, put_session, sess, stored_router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An engine over fakes that runs nothing by itself.
    pub struct Rig {
        pub e: Engine,
        rx: Receiver<Msg>,
        sent: Sent,
        tr: Arc<FakeTracker>,
        sys: Arc<FakeSystem>,
        live: Arc<Mutex<Vec<Session>>>,
        closed: Arc<Mutex<Vec<Session>>>,
        sc: crate::store::tests::Scratch,
    }

    pub fn rig(name: &str) -> Rig {
        let (router, sent, sc) = stored_router(&format!("engine-{name}"));
        let tr = Arc::new(FakeTracker::default());
        let t2 = tr.clone();
        let sys = Arc::new(FakeSystem::default());
        let live: Arc<Mutex<Vec<Session>>> = Arc::default();
        let closed: Arc<Mutex<Vec<Session>>> = Arc::default();
        let (l2, c2) = (live.clone(), closed.clone());
        let (e, rx) = Engine::build(
            Loader::with_factory(None, Box::new(move |_| t2.clone())),
            router,
            Actions {
                store: sc.1.clone(),
                relaunch: None,
                sys: sys.clone(),
                dry_run: false,
            },
            Records {
                live: Box::new(move || l2.lock().unwrap().clone()),
                closed: Box::new(move || c2.lock().unwrap().clone()),
            },
        );
        Rig {
            e,
            rx,
            sent,
            tr,
            sys,
            live,
            closed,
            sc,
        }
    }

    impl Rig {
        /// Load tasks of context `c`, whose workspace is `/ws/c`.
        pub fn load(&self, tasks: Vec<JoinedTask>) {
            self.e.on_msg(Msg::Snapshot(Box::new(self.snap(tasks))));
        }

        fn snap(&self, tasks: Vec<JoinedTask>) -> Snapshot {
            let mut c = crate::app::tests::ctx("c");
            c.workspace = self.sc.0.clone();
            Snapshot {
                contexts: vec![c],
                tasks,
                sessions: self.live.lock().unwrap().clone(),
                ..Default::default()
            }
        }

        /// A session, also as the record routing reads afresh.
        pub fn session(&self, s: Session) {
            put_session(&self.sc.1, &s);
            self.live.lock().unwrap().push(s);
        }

        fn run(&self, verb: Verb, confirm: Option<&str>) -> Result<Reply, Error> {
            self.e.command(Command {
                verb,
                confirm: confirm.map(Into::into),
                request_id: None,
            })
        }

        pub fn typed(&self) -> usize {
            self.sent.lock().unwrap().len()
        }
    }

    /// A task with a worktree and one open decision, `d1`.
    pub fn task(id: &str, wt: &str) -> JoinedTask {
        let mut t = jt(id);
        t.branches = vec![Branch {
            worktree: wt.into(),
            ..Default::default()
        }];
        t.decisions = vec![Decision {
            id: Some("d1".into()),
            text: "which?".into(),
            ..Default::default()
        }];
        t
    }

    fn at(id: &str) -> TaskRef {
        TaskRef::new("c", id)
    }

    fn answer(text: &str, decision: Option<&str>, ask: bool) -> Verb {
        Verb::Answer {
            task: at("a"),
            text: text.into(),
            decision: decision.map(Into::into),
            ask,
        }
    }

    fn code<T: std::fmt::Debug>(r: Result<T, Error>) -> Code {
        r.unwrap_err().code
    }

    fn token(r: Result<Reply, Error>) -> String {
        let e = r.unwrap_err();
        assert_eq!(e.code, Code::NeedsConfirm, "{e:?}");
        e.confirm.unwrap()
    }

    #[test]
    fn a_preview_never_replaces_a_load() {
        let r = rig("preview");
        let preview = |n: &str| Msg::Preview(Box::new(r.snap(vec![jt(n)])));
        r.e.on_msg(preview("cached"));
        let st = r.e.state();
        assert_eq!(
            (st.phase, st.tasks[0].id.as_str()),
            (api::Phase::Preview, "cached")
        );
        r.load(vec![jt("real")]);
        r.e.on_msg(preview("late"));
        let st = r.e.state();
        assert_eq!(
            (st.phase, st.tasks[0].id.as_str()),
            (api::Phase::Loaded, "real")
        );
    }

    #[test]
    fn a_failed_list_schedules_a_retry_and_a_clean_one_disarms_it() {
        let r = rig("retry");
        let mut s = r.snap(vec![]);
        s.retry_in_s = Some(30);
        r.e.on_msg(Msg::Snapshot(Box::new(s)));
        let left = r.e.lock().retry_at.unwrap() - Instant::now();
        assert!(
            left > Duration::from_secs(25) && left <= Duration::from_secs(30),
            "{left:?}"
        );
        r.load(vec![]);
        assert!(r.e.lock().retry_at.is_none());
    }

    #[test]
    fn a_reload_during_a_load_waits_for_it() {
        let r = rig("pending");
        r.e.lock().loading = true;
        r.run(
            Verb::Refresh {
                force: false,
                task: None,
            },
            None,
        )
        .unwrap();
        assert!(r.e.lock().pending.is_some());
        assert!(r.rx.try_recv().is_err(), "no second load started");
    }

    #[test]
    fn rev_moves_only_when_something_shown_changes() {
        let r = rig("rev");
        let mut ev = r.e.subscribe();
        r.load(vec![jt("a")]);
        let one = r.e.state();
        let first = ev.try_recv().unwrap();
        assert_eq!((first.rev, &first.instance), (one.rev, &one.instance));
        assert!(first.sections.contains(&api::Section::Tasks));
        r.e.publish();
        assert_eq!(r.e.state().rev, one.rev);
        assert!(ev.try_recv().is_err());
        r.session(ids("s-1", sess("/wt/z", "a:1.1", Some(11), "idle")));
        r.e.on_msg(Msg::Change(Change::Sessions));
        assert_eq!(r.e.state().rev, one.rev + 1);
        let next = ev.try_recv().unwrap();
        assert!(next.sections.contains(&api::Section::Sessions));
        assert!(!next.sections.contains(&api::Section::Decisions));
    }

    #[test]
    fn an_answer_goes_to_its_session_and_returns_the_delivery() {
        let r = rig("answer");
        r.session(ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle")));
        r.load(vec![task("a", "/wt/a")]);
        let out = r.run(answer("go", Some("d1"), false), None).unwrap();
        assert_eq!(r.sent.lock().unwrap()[0].0, "%1");
        assert_eq!(r.tr.replies.lock().unwrap()[0][1..], ["d1", "answer", "go"]);
        let d = out.delivery.unwrap();
        assert_eq!((d.task.as_str(), d.context.as_str()), ("a", "c"));
        assert_eq!(r.e.state().deliveries[0].id, d.id);
        // A second answer returns its own delivery, not the first.
        let two = r.run(answer("again", None, false), None).unwrap();
        assert_ne!(two.delivery.unwrap().id, d.id);
        assert_eq!(r.tr.notes.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_answer_that_cannot_land_changes_nothing() {
        let r = rig("refused");
        r.session(ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle")));
        let mut t = task("a", "/wt/a");
        let mut done = t.decisions[0].clone();
        done.id = Some("d2".into());
        done.status = crate::model::DecisionStatus::Resolved;
        t.decisions.push(done);
        t.decisions.push(Decision {
            id: Some("p1".into()),
            prompt: Some("plan".into()),
            ..Default::default()
        });
        let mut legacy = jt("b");
        legacy.decisions = vec![Decision::default()];
        r.load(vec![t, legacy]);
        for (verb, want) in [
            (answer(" \n ", Some("d1"), false), Code::BadRequest),
            (answer("x", Some("d2"), false), Code::Gone),
            (answer("x", Some("nope"), false), Code::Gone),
            (answer("x", None, true), Code::BadRequest),
        ] {
            assert_eq!(code(r.run(verb.clone(), None)), want, "{verb:?}");
        }
        let legacy_ask = Verb::Answer {
            task: at("b"),
            text: "x".into(),
            decision: Some("legacy".into()),
            ask: true,
        };
        assert_eq!(code(r.run(legacy_ask, None)), Code::BadRequest);
        // The same id in another context is another task.
        let other = Verb::Answer {
            task: TaskRef::new("other", "a"),
            text: "x".into(),
            decision: None,
            ask: false,
        };
        assert_eq!(code(r.run(other, None)), Code::NotFound);
        assert_eq!(r.typed(), 0);
        assert!(r.tr.replies.lock().unwrap().is_empty());
        assert!(r.tr.notes.lock().unwrap().is_empty());
        assert!(delivery::all(&r.sc.1).is_empty());
    }

    #[test]
    fn a_prompt_only_its_pane_can_answer_is_refused() {
        let r = rig("pane-only");
        r.session(ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle")));
        let mut t = task("a", "/wt/a");
        t.decisions[0].id = None;
        t.decisions[0].prompt = Some("permission".into());
        assert!(t.decisions[0].pane_only());
        let key = t.decisions[0].key().to_string();
        r.load(vec![t]);
        let e = r.run(answer("1", Some(&key), false), None).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), (Code::BadRequest, PANE_ONLY));
        assert_eq!(r.typed(), 0);
    }

    /// A busy pane that is not Claude asks first, and its token says yes to that pane only.
    #[test]
    fn a_confirm_token_covers_one_prompt() {
        let r = rig("token");
        r.session(ids("s-z", sess("/other", "o:1.3", None, "working")));
        r.session(ids("s-y", sess("/other", "o:1.3", None, "working")));
        r.load(vec![]);
        let send = |id: &str| Verb::Send {
            session: id.into(),
            text: "hello".into(),
        };
        let t = token(r.run(send("s-z"), None));
        assert_eq!(r.typed(), 0, "unconfirmed types nothing");
        assert_eq!(code(r.run(send("s-z"), Some("0"))), Code::NeedsConfirm);
        assert_eq!(code(r.run(send("s-z"), Some(""))), Code::NeedsConfirm);
        assert_eq!(code(r.run(send("s-y"), Some(&t))), Code::NeedsConfirm);
        assert_eq!(r.typed(), 0, "another session's token types nothing");
        let other = Verb::Send {
            session: "s-z".into(),
            text: "other".into(),
        };
        assert_eq!(code(r.run(other, Some(&t))), Code::NeedsConfirm);
        assert_eq!(r.typed(), 0, "another text's token types nothing");
        r.run(send("s-z"), Some(&t)).unwrap();
        assert_eq!(r.typed(), 1);
    }

    #[test]
    fn text_that_would_press_keys_is_refused() {
        let r = rig("keys");
        r.session(ids("s-1", sess("/wt/a", "a:1.1", Some(11), "idle")));
        r.load(vec![task("a", "/wt/a")]);
        for text in [
            "\x1b",
            "go\x03",
            "y\nrm -rf x",
            "y\r",
            "a\tb",
            "x\u{9b}1A",
            "\x7f",
            "a\0",
        ] {
            let session = || "s-1".to_string();
            let send = Verb::Send {
                session: session(),
                text: text.into(),
            };
            let name = Verb::Describe {
                session: session(),
                text: text.into(),
            };
            for v in [
                send,
                name,
                answer(text, None, false),
                answer(text, Some("d1"), true),
            ] {
                assert_eq!(code(r.run(v, None)), Code::BadRequest, "{text:?}");
            }
        }
        for text in ["", "  "] {
            let send = Verb::Send {
                session: "s-1".into(),
                text: text.into(),
            };
            assert_eq!(code(r.run(send, None)), Code::BadRequest);
        }
        assert_eq!(r.typed(), 0);
        assert!(r.tr.notes.lock().unwrap().is_empty());
        r.run(answer("-R é 日本", None, false), None).unwrap();
        assert_eq!(r.typed(), 1);
    }

    #[test]
    fn close_needs_its_own_token_and_a_changed_session_voids_it() {
        let r = rig("close");
        r.session(ids("s-1", sess("/wt/a", "a:1.1", Some(11), "idle")));
        r.load(vec![]);
        let close = |keep_pane| Verb::Close {
            session: "s-1".into(),
            keep_pane,
        };
        let t = token(r.run(close(false), None));
        assert!(r.sys.calls.lock().unwrap().is_empty());
        assert!(r.sc.1.sessions(crate::store::Kind::Closed).is_empty());
        // The pane now runs a different claude: what was shown is not what would be closed.
        r.live.lock().unwrap()[0].claude_pid = Some(12);
        r.e.on_msg(Msg::Change(Change::Sessions));
        assert_eq!(code(r.run(close(false), Some(&t))), Code::NeedsConfirm);
        assert!(r.sys.calls.lock().unwrap().is_empty());
        assert!(r.sc.1.sessions(crate::store::Kind::Closed).is_empty());
        let t = token(r.run(close(true), None));
        assert_eq!(
            code(r.run(close(false), Some(&t))),
            Code::NeedsConfirm,
            "a yes to keeping the pane is none to closing it"
        );
        let t = token(r.run(close(false), None));
        r.run(close(true), Some(&t)).unwrap();
        assert_eq!(r.sc.1.sessions(crate::store::Kind::Closed).len(), 1);
    }

    #[test]
    fn reopen_takes_a_closed_session_only() {
        let r = rig("reopen");
        r.session(ids("s-live", sess("/wt/a", "a:1.1", Some(11), "idle")));
        let mut c = ids("s-closed", sess("/wt/a", "a:1.1", None, "idle"));
        c.closed = true;
        r.closed.lock().unwrap().push(c);
        r.load(vec![]);
        let open = |s: &str| Verb::Reopen { session: s.into() };
        assert_eq!(code(r.run(open("s-live"), None)), Code::NotFound);
        let t = token(r.run(open("s-closed"), None));
        assert!(r.sys.calls.lock().unwrap().is_empty());
        r.closed.lock().unwrap()[0].worktree = "/elsewhere".into();
        assert_eq!(code(r.run(open("s-closed"), Some(&t))), Code::NeedsConfirm);
        assert!(r.sys.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn restoring_a_snapshot_asks_first() {
        let r = rig("restore");
        let dir = r.sc.1.root.join("snapshots");
        std::fs::create_dir_all(&dir).unwrap();
        let save = |name: &str| {
            let v = serde_json::json!({"created": "2026-01-02T03:04:05Z", "sessions": [{
                "tmux_session": name, "worktree": r.sc.0, "branch": "b",
                "session_id": "abc-1", "claude_pid": 1, "alive": true}]});
            std::fs::write(dir.join("latest.json"), v.to_string()).unwrap();
        };
        save("one");
        let e = r.run(Verb::SnapshotRestore, None).unwrap_err();
        assert!(e.message.contains("relaunches 1: one"), "{}", e.message);
        let t = e.confirm.unwrap();
        token(r.run(Verb::SnapshotRestore, Some("guess")));
        // The snapshot changed after the question: nothing of the new one is launched.
        save("two");
        token(r.run(Verb::SnapshotRestore, Some(&t)));
        assert!(r.sys.calls.lock().unwrap().is_empty());
        save("one");
        r.run(Verb::SnapshotRestore, Some(&t)).unwrap();
        assert!(r.sys.calls.lock().unwrap()[0].starts_with("launch one"));
    }

    #[test]
    fn a_repeated_request_runs_once() {
        let r = rig("repeat");
        r.session(ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle")));
        r.load(vec![task("a", "/wt/a")]);
        let req = |verb| Command {
            verb,
            confirm: None,
            request_id: Some("k1".into()),
        };
        let one = r.e.command(req(answer("go", None, false))).unwrap();
        let two = r.e.command(req(answer("go", None, false))).unwrap();
        assert_eq!(one, two);
        assert_eq!(r.typed(), 1);
        let other = r.e.command(req(answer("stop", None, false)));
        assert_eq!(code(other), Code::BadRequest);
        assert_eq!(r.typed(), 1);
    }

    #[test]
    fn a_request_that_asked_for_a_yes_can_be_repeated_with_it() {
        let r = rig("repeat-confirm");
        r.session(ids("s-z", sess("/other", "o:1.3", None, "working")));
        r.load(vec![]);
        let req = |confirm| Command {
            verb: Verb::Send {
                session: "s-z".into(),
                text: "hello".into(),
            },
            confirm,
            request_id: Some("k1".into()),
        };
        let t = token(r.e.command(req(None)));
        r.e.command(req(Some(t.clone()))).unwrap();
        r.e.command(req(Some(t))).unwrap();
        assert_eq!(r.typed(), 1);
    }

    /// A retry sent before the first attempt finished waits for its result.
    #[test]
    fn a_retry_that_overtakes_its_first_attempt_waits() {
        let r = rig("overtake");
        let verb = Verb::SnapshotSave;
        let slot = r.e.slot("k", &verb).unwrap();
        let mut running = slot.lock().unwrap();
        let n = Arc::new(AtomicUsize::new(0));
        let (e, n2) = (r.e.clone(), n.clone());
        let retry = std::thread::spawn(move || {
            let out = e.command(Command {
                verb,
                confirm: None,
                request_id: Some("k".into()),
            });
            n2.fetch_add(1, Ordering::SeqCst);
            out
        });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(n.load(Ordering::SeqCst), 0, "still waiting");
        *running = Some(Ok(Reply::new("first")));
        drop(running);
        assert_eq!(retry.join().unwrap().unwrap(), Reply::new("first"));
    }

    #[test]
    fn pins_are_set_not_toggled_and_build_on_each_other() {
        let r = rig("pin");
        let t = |id: &str, key: &str| {
            let mut t = jt(id);
            t.ticket_key = Some(key.into());
            t
        };
        let mut snap = r.snap(vec![t("a", "T-1"), t("b", "T-2"), jt("c")]);
        snap.contexts[0].adapter = Some(Default::default());
        r.e.on_msg(Msg::Snapshot(Box::new(snap)));
        // No load starts behind the test; a pin only asks for one.
        r.e.lock().loading = true;
        let pin = |id: &str, pinned| {
            let target = api::PinTarget::Ticket;
            r.run(
                Verb::Pin {
                    task: at(id),
                    target,
                    pinned,
                },
                None,
            )
        };
        let file = || std::fs::read_to_string(r.sc.0.join("fermata.yaml")).unwrap_or_default();
        pin("a", true).unwrap();
        assert!(file().contains("T-1"));
        pin("a", true).unwrap();
        assert!(file().contains("T-1"), "a second set keeps it");
        pin("b", true).unwrap();
        assert!(
            file().contains("T-1") && file().contains("T-2"),
            "{}",
            file()
        );
        pin("a", false).unwrap();
        assert!(!file().contains("T-1") && file().contains("T-2"));
        assert_eq!(code(pin("c", true)), Code::BadRequest);
        assert_eq!(code(pin("zz", true)), Code::NotFound);
    }

    #[test]
    fn a_resend_is_of_a_delivery_that_did_not_arrive() {
        let r = rig("resend");
        r.session(ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle")));
        r.load(vec![task("a", "/wt/a")]);
        let id = r.run(answer("go", Some("d1"), false), None).unwrap();
        let id = id.delivery.unwrap().id;
        let again = || {
            r.run(
                Verb::Resend {
                    delivery: id.clone(),
                },
                None,
            )
        };
        assert_eq!(code(again()), Code::BadRequest);
        let fail = |d: &mut Delivery| {
            d.state = DState::Failed;
            true
        };
        delivery::update(&r.sc.1, &id, fail).unwrap();
        let new = again().unwrap().delivery.unwrap();
        assert_ne!(new.id, id);
        assert_eq!(r.typed(), 2);
        assert_eq!(r.tr.replies.lock().unwrap().len(), 1, "recorded once");
        assert_eq!(code(again()), Code::BadRequest, "already resent");
        let none = Verb::Resend {
            delivery: "nope".into(),
        };
        assert_eq!(code(r.run(none, None)), Code::NotFound);
    }

    #[test]
    fn a_resend_never_follows_a_newer_answer_or_a_settled_decision() {
        let r = rig("resend-old");
        r.session(ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle")));
        r.load(vec![task("a", "/wt/a")]);
        let send = |text: &str| {
            let d = r.run(answer(text, Some("d1"), false), None).unwrap();
            let id = d.delivery.unwrap().id;
            let fail = |d: &mut Delivery| {
                d.state = DState::Failed;
                true
            };
            delivery::update(&r.sc.1, &id, fail).unwrap();
            id
        };
        let resend = |id: &str| {
            let delivery = id.to_string();
            r.run(Verb::Resend { delivery }, None)
        };
        let old = send("one");
        std::thread::sleep(Duration::from_millis(2));
        let new = send("two");
        assert_eq!(code(resend(&old)), Code::BadRequest);
        let mut t = task("a", "/wt/a");
        t.decisions[0].status = DecisionStatus::Resolved;
        r.load(vec![t]);
        assert_eq!(code(resend(&new)), Code::Gone);
        assert_eq!(r.typed(), 2);
        r.load(vec![task("a", "/wt/a")]);
        resend(&new).unwrap();
        assert_eq!(r.typed(), 3);
    }

    #[test]
    fn remembered_requests_are_capped() {
        let r = rig("cap");
        for n in 0..REQUESTS + 10 {
            r.e.slot(&format!("k{n}"), &Verb::SnapshotSave).unwrap();
        }
        assert_eq!(r.e.0.requests.lock().unwrap().len(), REQUESTS);
        let last = format!("k{}", REQUESTS + 9);
        assert!(r.e.0.requests.lock().unwrap().contains_key(&last));
    }

    #[test]
    fn only_transcripts_under_the_projects_directory_are_read() {
        let sc = crate::store::tests::scratch("engine-under");
        let (dir, out) = (sc.0.join("projects"), sc.0.join("outside"));
        std::fs::create_dir_all(dir.join("p")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        for f in [
            dir.join("p/a.jsonl"),
            dir.join("p/a.txt"),
            out.join("b.jsonl"),
        ] {
            std::fs::write(f, "{}").unwrap();
        }
        std::os::unix::fs::symlink(out.join("b.jsonl"), dir.join("p/link.jsonl")).unwrap();
        assert!(under(&dir.join("p/a.jsonl"), &dir).is_ok());
        for bad in [
            dir.join("p/a.txt"),
            out.join("b.jsonl"),
            dir.join("p/link.jsonl"),
            dir.join("p/../../outside/b.jsonl"),
        ] {
            assert_eq!(code(under(&bad, &dir)), Code::Forbidden, "{bad:?}");
        }
        assert!(under(&dir.join("p/missing.jsonl"), &dir).is_err());
    }

    #[test]
    fn a_transcript_is_named_only_by_a_reference_it_was_given() {
        let r = rig("refs");
        let q = |reference: &str, around: Option<&str>| {
            r.e.query(Query::Transcript {
                reference: reference.into(),
                around: around.map(Into::into),
            })
        };
        for bad in ["", "0", "/etc/passwd", "../x.jsonl"] {
            assert_eq!(code(q(bad, None)), Code::NotFound, "{bad}");
        }
        let given = r.e.reference("/etc/passwd.jsonl");
        assert!(q(&given, None).is_err());
        assert_eq!(code(q(&given, Some("noon"))), Code::BadRequest);
    }

    #[test]
    fn a_log_is_read_by_a_known_session_id_only() {
        let r = rig("log");
        for bad in ["../../x", "a/b", "", "unknown"] {
            let q = Query::Log {
                session: bad.into(),
            };
            assert_eq!(code(r.e.query(q)), Code::NotFound, "{bad}");
        }
    }

    #[test]
    fn a_refreshed_task_replaces_itself_in_its_own_context() {
        let r = rig("replace");
        let mut other = jt("a");
        other.context = "d".into();
        r.load(vec![jt("a"), other]);
        let mut new = jt("a");
        new.task.title = "new".into();
        r.e.replace(new);
        let st = r.e.state();
        let title = |c: &str| st.task(&TaskRef::new(c, "a")).unwrap().title.clone();
        assert_eq!((title("c").as_str(), title("d").as_str()), ("new", ""));
    }
}
