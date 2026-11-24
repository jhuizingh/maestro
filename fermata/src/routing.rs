//! Routing typed input: live session pane, else the orchestrator pane, and always a note.
//! Panes are found by process (`panes::resolve`), never by ccmon's recorded address alone.
use crate::baton::Tracker;
use crate::delivery::{self, Delivery, State};
use crate::log;
use crate::model::Session;
use crate::notes::answer_note;
use crate::panes::{self, PaneRef, PaneTable, Use};
use crate::sessions::same_path;
use crate::store::{Kind, Store};
use crate::util::run_cmd;
use anyhow::{Result, bail};
use std::sync::Mutex;
use std::time::Duration;

pub trait Tmux: Send + Sync {
    /// Every pane and the pid → ppid map, reused when younger than `max_age`.
    fn pane_table(&self, max_age: Duration) -> Result<PaneTable>;
    /// Type `text` literally into the pane, then press Enter. `clear` sends C-u first.
    fn send_text(&self, pane_id: &str, text: &str, clear: bool) -> Result<()>;
    /// Switch the client to a pane id (`%NN`) or a session (`=name`).
    fn switch_client(&self, target: &str) -> Result<()>;
    /// A tmux session with exactly this name exists.
    fn has_session(&self, name: &str) -> bool;
    fn new_window(&self, dir: &str) -> Result<()>;
}

pub struct RealTmux;

fn tmux(args: &[&str]) -> Result<String> {
    let a: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = run_cmd("tmux", &a, None, &[], None, Duration::from_secs(3))?;
    if !out.ok() {
        bail!("tmux {}: {}", args[0], out.stderr.trim());
    }
    Ok(out.stdout)
}

impl Tmux for RealTmux {
    fn pane_table(&self, max_age: Duration) -> Result<PaneTable> {
        panes::real_table(max_age)
    }
    fn send_text(&self, pane_id: &str, text: &str, clear: bool) -> Result<()> {
        if clear {
            tmux(&["send-keys", "-t", pane_id, "C-u"])?;
        }
        tmux(&["send-keys", "-t", pane_id, "-l", text])?;
        tmux(&["send-keys", "-t", pane_id, "Enter"])?;
        Ok(())
    }
    fn switch_client(&self, target: &str) -> Result<()> {
        tmux(&["switch-client", "-t", target]).map(|_| ())
    }
    fn has_session(&self, name: &str) -> bool {
        tmux(&["has-session", "-t", &format!("={name}")]).is_ok()
    }
    fn new_window(&self, dir: &str) -> Result<()> {
        tmux(&["new-window", "-c", dir]).map(|_| ())
    }
}

/// Reads the real pane table; records what would have been sent and touches nothing.
#[derive(Default)]
pub struct DryRunTmux {
    pub sent: Mutex<Vec<String>>,
}

impl Tmux for DryRunTmux {
    fn pane_table(&self, max_age: Duration) -> Result<PaneTable> {
        panes::real_table(max_age)
    }
    fn send_text(&self, pane_id: &str, text: &str, clear: bool) -> Result<()> {
        let c = if clear { " (after C-u)" } else { "" };
        let line = format!("dry-run send-keys -t {pane_id}{c}: {text}");
        log::warn(&line);
        self.sent.lock().unwrap().push(line);
        Ok(())
    }
    fn switch_client(&self, target: &str) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push(format!("dry-run switch-client {target}"));
        Ok(())
    }
    fn has_session(&self, name: &str) -> bool {
        RealTmux.has_session(name)
    }
    fn new_window(&self, dir: &str) -> Result<()> {
        self.sent
            .lock()
            .unwrap()
            .push(format!("dry-run new-window {dir}"));
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Session(Box<Session>),
    Orchestrator(Box<Session>),
    NoteOnly { reason: String },
}

impl Route {
    pub fn session(&self) -> Option<&Session> {
        match self {
            Route::Session(s) | Route::Orchestrator(s) => Some(s),
            Route::NoteOnly { .. } => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Route::Session(_) => "session",
            Route::Orchestrator(_) => "orchestrator",
            Route::NoteOnly { .. } => "note",
        }
    }
}

/// The task's live session (cwd is one of its worktrees), else the orchestrator (cwd is the
/// context's workspace dir), else note only.
pub fn plan(worktrees: &[&str], workspace: &str, sessions: &[Session]) -> Route {
    let live = |s: &&Session| !s.closed && (s.tmux_target.is_some() || s.claude_pid.is_some());
    if let Some(s) = sessions
        .iter()
        .filter(live)
        .find(|s| worktrees.iter().any(|w| same_path(&s.worktree, w)))
    {
        return Route::Session(Box::new(s.clone()));
    }
    if let Some(s) = sessions
        .iter()
        .filter(live)
        .find(|s| same_path(&s.worktree, workspace))
    {
        return Route::Orchestrator(Box::new(s.clone()));
    }
    Route::NoteOnly {
        reason: "no orchestrator session found; note kept for the next sweep".into(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The target pane is busy and not Claude Code; ask the user before typing into it.
    NeedsConfirm {
        target: String,
        status: String,
    },
    Delivered {
        summary: String,
    },
}

/// Input that answers, or asks about, one tracker decision.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub decision: String,
    /// Sent with the ask key: a question whatever the text ends with.
    pub ask: bool,
}

impl Reply {
    /// `question` (keeps the decision open) for the ask key or text ending in `?`, else `answer`.
    pub fn kind(&self, text: &str) -> &'static str {
        if self.ask || text.trim_end().ends_with('?') {
            "question"
        } else {
            "answer"
        }
    }
}

/// One answer to route.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub context: String,
    pub task_id: String,
    pub text: String,
    /// The user said yes to typing into a busy non-Claude pane.
    pub confirmed: bool,
    /// A resend of this delivery: typed again, no second tracker note.
    pub resend_of: Option<String>,
    /// Recorded with the tracker's `reply` verb instead of an ANSWER note.
    pub reply: Option<Reply>,
}

pub struct Router {
    pub tmux: Box<dyn Tmux>,
    pub dry_run: bool,
    /// fermata runs inside tmux, so `switch-client` has a client to move.
    pub in_tmux: bool,
    /// Delivery records, and each session's current record; `None` keeps neither.
    pub store: Option<Store>,
}

impl Router {
    pub fn new(dry_run: bool) -> Self {
        let tmux: Box<dyn Tmux> = if dry_run {
            Box::new(DryRunTmux::default())
        } else {
            Box::new(RealTmux)
        };
        let in_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
        Self {
            tmux,
            dry_run,
            in_tmux,
            // Tests never touch the real store.
            store: (!dry_run && !cfg!(test)).then(Store::open),
        }
    }

    /// The session's record as it is now: the snapshot's copy can lag a dialog opening.
    fn fresh(&self, s: &Session) -> Session {
        self.store
            .as_ref()
            .and_then(|st| st.read(Kind::Live, &s.session_id))
            .and_then(|r| serde_json::from_value(serde_json::Value::Object(r)).ok())
            .unwrap_or_else(|| s.clone())
    }

    /// Type into the pane unless it is at a dialog, and say what happened in `d`.
    fn attempt(&self, d: &mut Delivery, s: &Session, p: &PaneRef) {
        d.session_id = s.session_id.clone();
        d.pane = p.label();
        d.pane_id = p.id.clone();
        if let Some(what) = p.is_claude().then(|| delivery::open_prompt(s)).flatten() {
            d.state = State::Held;
            d.detail = what;
            return;
        }
        match self.type_into(p, &d.payload) {
            Ok(_) => {
                d.state = if s.status == "working" {
                    State::Queued
                } else {
                    State::Delivered
                };
                d.detail.clear();
                d.sent_at = Some(crate::store::now());
            }
            Err(e) => {
                log::error(format!("send to {}: {e}", p.id));
                d.state = State::Failed;
                d.detail = e.to_string();
            }
        }
    }

    fn summary(&self, d: &Delivery) -> String {
        let dry = if self.dry_run { "dry-run: " } else { "" };
        match d.state {
            State::Delivered => format!("{dry}sent to {} [{}]", d.pane, d.pane_id),
            State::Queued => format!("{dry}queued for {} [{}] (busy)", d.pane, d.pane_id),
            State::Held => format!("held: {} is at {}; sends when it closes", d.pane, d.detail),
            State::NoteOnly => d.detail.clone(),
            State::Failed => format!("send to {} failed: {}", d.pane, d.detail),
        }
    }

    fn record(&self, d: &Delivery, what: &str) {
        if self.dry_run {
            log::warn(format!("dry-run {}", d.log_line(what)));
            return;
        }
        log::info(d.log_line(what));
        if let Some(st) = &self.store
            && let Err(e) = delivery::write(st, d)
        {
            log::error(format!("delivery {}: {e:#}", d.id));
        }
    }

    /// The session's pane right now (cached briefly).
    pub fn resolve(&self, s: &Session) -> Result<PaneRef> {
        panes::resolve(&self.tmux.pane_table(panes::CACHE_TTL)?, s, Use::Interact)
    }

    /// Where the session's pane is, for display: "(moved)" when it left the recorded address.
    pub fn pane_label(&self, s: &Session) -> String {
        match self.resolve(s) {
            Ok(p) => p.label(),
            Err(_) => s.tmux_target.clone().unwrap_or_default(),
        }
    }

    /// A busy pane needs confirmation unless it runs Claude Code, which queues input.
    fn needs_confirm(p: &PaneRef, status: &str, confirmed: bool) -> Option<Outcome> {
        (status == "working" && !confirmed && !p.is_claude()).then(|| Outcome::NeedsConfirm {
            target: p.label(),
            status: status.into(),
        })
    }

    fn type_into(&self, p: &PaneRef, text: &str) -> Result<String> {
        self.tmux.send_text(&p.id, text, p.is_claude())?;
        Ok(format!(
            "{}sent to {} [{}]",
            if self.dry_run { "dry-run: " } else { "" },
            p.label(),
            p.id
        ))
    }

    pub fn deliver(&self, tracker: &dyn Tracker, req: &Request, route: &Route) -> Result<Outcome> {
        let text = crate::util::one_line(&req.text);
        if text.is_empty() {
            bail!("nothing to send");
        }
        let task_id = req.task_id.as_str();
        let reply = req
            .reply
            .as_ref()
            .map(|r| (r.decision.as_str(), r.kind(&text)));
        let payload = match (route, reply) {
            (Route::Orchestrator(_), Some((did, kind))) => {
                format!("[fermata {task_id} {did} {kind}] {text}")
            }
            (Route::Orchestrator(_), None) => format!("[fermata {task_id}] {text}"),
            (_, Some((_, "question"))) => format!("(question) {text}"),
            _ => text.clone(),
        };
        let mut d = Delivery::new(&req.context, task_id, &text, &payload, route.name());
        d.resend_of = req.resend_of.clone();
        d.decision = reply.map(|(did, _)| did.to_string());
        d.kind = reply.map(|(_, k)| k.to_string());
        match route.session().map(|s| (self.fresh(s), self.resolve(s))) {
            Some((s, Ok(p))) => {
                if let Some(o) = Self::needs_confirm(&p, &s.status, req.confirmed) {
                    return Ok(o);
                }
                self.attempt(&mut d, &s, &p);
            }
            Some((s, Err(e))) => {
                d.session_id = s.session_id.clone();
                d.state = State::NoteOnly;
                d.detail = format!("{e}; note only");
            }
            None => {
                d.state = State::NoteOnly;
                if let Route::NoteOnly { reason } = route {
                    d.detail = reason.clone();
                }
            }
        }
        let mut parts = vec![self.summary(&d)];
        if let Some(old) = &req.resend_of {
            parts.push(format!("resend of {old}"));
        } else if let Some((did, kind)) = reply {
            // A decision reply is recorded against the decision; a note could hide nothing.
            if self.dry_run {
                log::warn(format!("dry-run reply {task_id} {did} {kind}: {text}"));
                parts.push(format!("{kind} (dry-run)"));
            } else {
                match tracker.reply(task_id, did, kind, &text) {
                    Ok(()) => parts.push(format!("recorded as {kind}")),
                    Err(e) => {
                        log::error(format!("reply {task_id} {did}: {e}"));
                        parts.push(format!("reply failed: {e}"))
                    }
                }
            }
        } else {
            let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%MZ").to_string();
            let note = answer_note(&ts, &d.note_outcome(), &text);
            if self.dry_run {
                log::warn(format!("dry-run note {task_id}: {note}"));
                parts.push("note (dry-run)".into());
            } else {
                match tracker.note(task_id, &note) {
                    Ok(()) => parts.push("noted".into()),
                    Err(e) => {
                        log::error(format!("note {task_id}: {e}"));
                        parts.push(format!("note failed: {e}"))
                    }
                }
            }
        }
        self.record(
            &d,
            if req.resend_of.is_some() {
                "resent"
            } else {
                "routed"
            },
        );
        if let (Some(old), Some(st), false) = (&req.resend_of, &self.store, self.dry_run) {
            let new = d.id.clone();
            let _ = delivery::update(st, old, |o| {
                o.superseded_by = Some(new);
                true
            });
        }
        Ok(Outcome::Delivered {
            summary: parts.join("; "),
        })
    }

    /// Send a held delivery once its pane has left the dialog. Under the store lock, so two
    /// fermata instances never both send it. Returns what happened, or `None` if still held.
    pub fn release(&self, id: &str) -> Option<String> {
        let st = self.store.as_ref()?;
        let mut out = None;
        let r = delivery::update(st, id, |d| {
            if d.state != State::Held {
                return false;
            }
            let Some(s) = st
                .read(Kind::Live, &d.session_id)
                .and_then(|r| serde_json::from_value::<Session>(serde_json::Value::Object(r)).ok())
            else {
                d.state = State::NoteOnly;
                d.detail = "its session ended while the answer was held".into();
                out = Some(format!("{}: {}", d.task, d.detail));
                return true;
            };
            let p = match self.resolve(&s) {
                Ok(p) => p,
                Err(e) => {
                    d.state = State::NoteOnly;
                    d.detail = format!("{e}; note only");
                    out = Some(format!("{}: {}", d.task, d.detail));
                    return true;
                }
            };
            if p.is_claude() && delivery::open_prompt(&s).is_some() {
                return false;
            }
            self.attempt(d, &s, &p);
            out = Some(format!("{}: {}", d.task, self.summary(d)));
            true
        });
        match r {
            Ok(true) => {
                if let Some(d) = delivery::read(st, id) {
                    log::info(d.log_line("released"));
                }
                out
            }
            Ok(false) => None,
            Err(e) => {
                log::error(format!("release delivery {id}: {e:#}"));
                None
            }
        }
    }

    /// Quick reply straight to a session pane (Sessions view), no task attached.
    pub fn send_to_pane(&self, s: &Session, text: &str, confirmed: bool) -> Result<Outcome> {
        let p = self.resolve(s)?;
        if let Some(o) = Self::needs_confirm(&p, &s.status, confirmed) {
            return Ok(o);
        }
        let summary = self.type_into(&p, &crate::util::one_line(text))?;
        Ok(Outcome::Delivered { summary })
    }

    /// Jump the client to the session's pane.
    pub fn jump(&self, s: &Session) -> Result<String> {
        let p = self.resolve(s)?;
        self.switch(&p.id, &p.label())
    }

    fn switch(&self, target: &str, label: &str) -> Result<String> {
        if !self.in_tmux {
            bail!("not inside tmux; run: tmux attach -t '{target}'");
        }
        self.tmux.switch_client(target)?;
        Ok(if self.dry_run {
            format!("dry-run: would jump to {label}")
        } else {
            format!("jumped to {label}")
        })
    }

    /// `w` on a task: its live Claude pane, else a tmux session named for the task, else a new
    /// window in the worktree. Never the orchestrator: that is not the task's session.
    pub fn open_task(
        &self,
        route: Option<&Route>,
        names: &[String],
        worktree: Option<&str>,
    ) -> String {
        let mut why = "no live session".to_string();
        if let Some(Route::Session(s)) = route {
            match self.resolve(s) {
                Ok(p) => {
                    return self
                        .switch(&p.id, &p.label())
                        .unwrap_or_else(|e| format!("jump: {e}"));
                }
                Err(e) => why = format!("{e}"),
            }
        }
        if let Some(n) = names.iter().find(|n| self.tmux.has_session(n)) {
            return self
                .switch(&format!("={n}"), &format!("session {n}"))
                .unwrap_or_else(|e| format!("jump: {e}"));
        }
        let Some(w) = worktree else {
            return format!("{why}; no worktree");
        };
        if !self.in_tmux {
            return format!("{why}; not inside tmux, worktree is {w}");
        }
        match self.tmux.new_window(w) {
            Ok(()) => format!("{why}; opened new window in {w}"),
            Err(e) => format!("{why}; tmux: {e}"),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::core::tests::FakeTracker;
    use crate::panes::tests::{pane, table};
    use std::sync::Arc;

    type Sent = Arc<Mutex<Vec<(String, String, bool)>>>;
    pub type Calls = Arc<Mutex<Vec<String>>>;

    /// Panes: orchestrator `o:1.2` = %2 (shell 20, claude 21) in /ws; task session `a:1.1` = %1
    /// (shell 10, claude 11) in /wt/a; a plain zsh %9 at `o:1.3` in /other.
    #[derive(Default)]
    struct FakeTmux {
        sent: Sent,
        /// `switch-client <target>` and `new-window <dir>`, in order.
        calls: Calls,
        table: PaneTable,
        sessions: Vec<String>,
    }

    impl Tmux for FakeTmux {
        fn pane_table(&self, _max_age: Duration) -> Result<PaneTable> {
            Ok(self.table.clone())
        }
        fn send_text(&self, t: &str, text: &str, clear: bool) -> Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((t.into(), text.into(), clear));
            Ok(())
        }
        fn switch_client(&self, t: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("switch-client {t}"));
            Ok(())
        }
        fn has_session(&self, name: &str) -> bool {
            self.sessions.iter().any(|s| s == name)
        }
        fn new_window(&self, d: &str) -> Result<()> {
            self.calls.lock().unwrap().push(format!("new-window {d}"));
            Ok(())
        }
    }

    fn std_table() -> PaneTable {
        table(
            vec![
                pane("%1", 10, "a:1.1", "claude", "/wt/a"),
                pane("%2", 20, "o:1.2", "claude", "/ws"),
                pane("%9", 90, "o:1.3", "zsh", "/other"),
            ],
            &[(11, 10), (21, 20), (10, 1), (20, 1), (90, 1)],
        )
    }

    fn sess(wt: &str, target: &str, pid: Option<i64>, status: &str) -> Session {
        Session {
            worktree: wt.into(),
            tmux_target: Some(target.into()),
            claude_pid: pid,
            status: status.into(),
            ..Default::default()
        }
    }

    fn router_with(table: PaneTable) -> (Router, Sent) {
        let t = FakeTmux {
            table,
            ..Default::default()
        };
        let sent = t.sent.clone();
        (
            Router {
                tmux: Box::new(t),
                dry_run: false,
                in_tmux: true,
                store: None,
            },
            sent,
        )
    }

    fn router() -> (Router, Sent) {
        router_with(std_table())
    }

    fn req(task: &str, text: &str, confirmed: bool) -> Request {
        Request {
            task_id: task.into(),
            text: text.into(),
            confirmed,
            ..Default::default()
        }
    }

    /// A router over `std_table()` keeping delivery records in a scratch store.
    fn stored_router(name: &str) -> (Router, Sent, crate::store::tests::Scratch) {
        let sc = crate::store::tests::scratch(name);
        let (mut r, sent) = router();
        r.store = Some(sc.1.clone());
        (r, sent, sc)
    }

    fn put_session(st: &Store, s: &Session) {
        let serde_json::Value::Object(m) = serde_json::to_value(s).unwrap() else {
            unreachable!()
        };
        st.write(Kind::Live, &s.session_id, &m).unwrap();
    }

    fn ids(s: &str, mut x: Session) -> Session {
        x.session_id = s.into();
        x
    }

    #[test]
    fn answers_record_their_outcome_on_the_note_and_the_delivery() {
        let (r, sent, sc) = stored_router("route-outcome");
        let tr = FakeTracker::default();
        let s = ids("s-a", sess("/wt/a", "a:1.1", Some(11), "working"));
        put_session(&sc.1, &s);
        r.deliver(
            &tr,
            &req("ctx-1", "go", false),
            &plan(&["/wt/a"], "/ws", &[s]),
        )
        .unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1);
        let n = tr.notes.lock().unwrap()[0].1.clone();
        assert!(n.contains(", queued for a:1.1): go"), "{n}");
        let d = &delivery::all(&sc.1)[0];
        assert_eq!((d.state, d.pane_id.as_str()), (State::Queued, "%1"));
        assert!(d.sent_at.is_some());
        r.deliver(&tr, &req("ctx-4", "x", false), &plan(&[], "/ws", &[]))
            .unwrap();
        assert!(tr.notes.lock().unwrap()[1].1.contains(", note only): x"));
        assert_eq!(delivery::all(&sc.1)[1].state, State::NoteOnly);
    }

    /// Input on a tracker decision is a reply, never an ANSWER note: a `?` or the ask key makes
    /// a question, and the orchestrator is told which decision and which kind.
    #[test]
    fn decision_input_is_a_reply_not_a_note() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", Some(21), "idle")]);
        let rq = |task: &str, text: &str, ask| Request {
            reply: Some(Reply {
                decision: "d1a2b3c".into(),
                ask,
            }),
            ..req(task, text, false)
        };
        r.deliver(&tr, &rq("ctx-2", "Option 1: merge", false), &route)
            .unwrap();
        r.deliver(&tr, &rq("ctx-2", "what does 2 change? ", false), &route)
            .unwrap();
        let o = r
            .deliver(&tr, &rq("ctx-2", "explain the risk", true), &route)
            .unwrap();
        let kinds: Vec<String> = tr
            .replies
            .lock()
            .unwrap()
            .iter()
            .map(|x| x[2].clone())
            .collect();
        assert_eq!(kinds, ["answer", "question", "question"]);
        assert_eq!(tr.replies.lock().unwrap()[0][1], "d1a2b3c");
        assert!(tr.notes.lock().unwrap().is_empty(), "no ANSWER note");
        assert_eq!(
            sent.lock().unwrap()[0].1,
            "[fermata ctx-2 d1a2b3c answer] Option 1: merge"
        );
        assert!(
            matches!(o, Outcome::Delivered { summary } if summary.contains("recorded as question"))
        );
        // No session at all: the reply is still recorded.
        r.deliver(&tr, &rq("ctx-3", "yes", false), &plan(&[], "/x", &[]))
            .unwrap();
        assert_eq!(tr.replies.lock().unwrap().len(), 4);
    }

    /// Keystrokes typed into an open dialog answer the dialog, not the session.
    #[test]
    fn a_pane_at_a_dialog_is_held_then_released_when_it_closes() {
        let (r, sent, sc) = stored_router("route-held");
        let tr = FakeTracker::default();
        // The snapshot predates the dialog; routing must read the record fresh.
        let snap = ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle"));
        let mut now = snap.clone();
        now.prompt_open = Some("AskUserQuestion".into());
        put_session(&sc.1, &now);
        let o = r
            .deliver(
                &tr,
                &req("ctx-1", "yes", false),
                &plan(&["/wt/a"], "/ws", &[snap]),
            )
            .unwrap();
        assert!(sent.lock().unwrap().is_empty());
        assert!(matches!(o, Outcome::Delivered { ref summary } if summary.starts_with("held:")));
        assert!(
            tr.notes.lock().unwrap()[0]
                .1
                .contains(", held for a:1.1): yes")
        );
        let id = delivery::all(&sc.1)[0].id.clone();
        assert_eq!(r.release(&id), None, "still at the dialog");
        now.prompt_open = None;
        put_session(&sc.1, &now);
        assert!(r.release(&id).unwrap().contains("sent to a:1.1"));
        assert_eq!(sent.lock().unwrap()[0], ("%1".into(), "yes".into(), true));
        assert_eq!(delivery::read(&sc.1, &id).unwrap().state, State::Delivered);
        assert_eq!(r.release(&id), None, "sent once only");
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_held_answer_whose_session_ends_becomes_note_only() {
        let (r, sent, sc) = stored_router("route-held-gone");
        let tr = FakeTracker::default();
        let mut s = ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle"));
        s.prompt_open = Some("permission".into());
        put_session(&sc.1, &s);
        r.deliver(
            &tr,
            &req("ctx-1", "y", false),
            &plan(&["/wt/a"], "/ws", &[s]),
        )
        .unwrap();
        let id = delivery::all(&sc.1)[0].id.clone();
        let _ = std::fs::remove_file(sc.1.root.join("sessions/s-a.json"));
        assert!(r.release(&id).unwrap().contains("session ended"));
        assert_eq!(delivery::read(&sc.1, &id).unwrap().state, State::NoteOnly);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn resend_types_again_without_a_second_note() {
        let (r, sent, sc) = stored_router("route-resend");
        let tr = FakeTracker::default();
        let s = ids("s-a", sess("/wt/a", "a:1.1", Some(11), "idle"));
        put_session(&sc.1, &s);
        let route = plan(&["/wt/a"], "/ws", &[s]);
        r.deliver(&tr, &req("ctx-1", "go", false), &route).unwrap();
        let first = delivery::all(&sc.1)[0].id.clone();
        let mut again = req("ctx-1", "go", false);
        again.resend_of = Some(first.clone());
        r.deliver(&tr, &again, &route).unwrap();
        assert_eq!(sent.lock().unwrap().len(), 2);
        assert_eq!(tr.notes.lock().unwrap().len(), 1);
        let all = delivery::all(&sc.1);
        let new = all.iter().find(|d| d.id != first).unwrap();
        assert_eq!(new.resend_of.as_deref(), Some(first.as_str()));
        let old = delivery::read(&sc.1, &first).unwrap();
        assert_eq!(old.superseded_by.as_deref(), Some(new.id.as_str()));
    }

    /// A router over `std_table()`, inside tmux, recording `switch-client`/`new-window` calls.
    pub fn fake_router() -> (Router, Calls) {
        let t = FakeTmux {
            table: std_table(),
            ..Default::default()
        };
        let calls = t.calls.clone();
        let r = Router {
            tmux: Box::new(t),
            dry_run: false,
            in_tmux: true,
            store: None,
        };
        (r, calls)
    }

    #[test]
    fn plans() {
        let s = vec![
            sess("/ws", "o:1.2", Some(21), "idle"),
            sess("/wt/a", "a:1.1", Some(11), "idle"),
        ];
        assert_eq!(
            plan(&["/wt/a"], "/ws", &s),
            Route::Session(Box::new(s[1].clone()))
        );
        assert!(matches!(
            plan(&["/wt/b"], "/ws", &s),
            Route::Orchestrator(_)
        ));
        assert!(matches!(plan(&["/wt/b"], "/x", &s), Route::NoteOnly { .. }));
    }

    #[test]
    fn session_route_sends_and_notes() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let s = [sess("/wt/a", "a:1.1", Some(11), "idle")];
        let route = plan(&["/wt/a"], "/ws", &s);
        let o = r
            .deliver(&tr, &req("ctx-1", "yes\nplease", false), &route)
            .unwrap();
        assert!(matches!(o, Outcome::Delivered { .. }));
        assert_eq!(
            sent.lock().unwrap()[0],
            ("%1".into(), "yes please".into(), true)
        );
        let notes = tr.notes.lock().unwrap();
        assert_eq!(notes[0].0, "ctx-1");
        assert!(notes[0].1.starts_with("ANSWER (fermata, "));
        assert!(notes[0].1.ends_with("): yes please"));
    }

    #[test]
    fn orchestrator_prefix() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", Some(21), "idle")]);
        r.deliver(&tr, &req("ctx-2", "go", false), &route).unwrap();
        assert_eq!(sent.lock().unwrap()[0].1, "[fermata ctx-2] go");
    }

    /// The bug this guards: the orchestrator was recorded at o:1.2, a new pane took that index,
    /// and the answer was typed into a plain zsh.
    #[test]
    fn moved_orchestrator_gets_the_answer_not_the_pane_at_its_old_address() {
        let (r, sent) = router_with(table(
            vec![
                pane("%9", 90, "o:1.2", "zsh", "/other"),
                pane("%2", 20, "o:1.3", "claude", "/ws"),
            ],
            &[(21, 20), (20, 1), (90, 1)],
        ));
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", Some(21), "idle")]);
        let o = r.deliver(&tr, &req("ctx-2", "1", false), &route).unwrap();
        assert_eq!(sent.lock().unwrap()[0].0, "%2");
        assert!(
            matches!(o, Outcome::Delivered { ref summary } if summary.contains("o:1.3 (moved)")),
            "{o:?}"
        );
    }

    #[test]
    fn busy_claude_pane_is_not_asked_about() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", Some(21), "working")]);
        let o = r.deliver(&tr, &req("ctx-3", "go", false), &route).unwrap();
        assert!(matches!(o, Outcome::Delivered { .. }));
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn busy_non_claude_pane_needs_confirm_and_is_not_cleared() {
        // Dead claude pid; the recorded pane is a shell in the worktree, so the fallback applies.
        let (r, sent) = router_with(table(
            vec![pane("%4", 40, "o:1.2", "python3", "/ws")],
            &[(40, 1)],
        ));
        let tr = FakeTracker::default();
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.2", Some(999), "working")]);
        let o = r.deliver(&tr, &req("ctx-3", "go", false), &route).unwrap();
        assert!(matches!(o, Outcome::NeedsConfirm { .. }));
        assert!(sent.lock().unwrap().is_empty());
        assert!(tr.notes.lock().unwrap().is_empty());
        r.deliver(&tr, &req("ctx-3", "go", true), &route).unwrap();
        assert_eq!(
            sent.lock().unwrap()[0],
            ("%4".into(), "[fermata ctx-3] go".into(), false)
        );
    }

    #[test]
    fn note_only_and_missing_pane() {
        let (r, sent) = router();
        let tr = FakeTracker::default();
        let o = r
            .deliver(&tr, &req("ctx-4", "x", false), &plan(&[], "/ws", &[]))
            .unwrap();
        assert!(
            matches!(o, Outcome::Delivered { ref summary } if summary.contains("no orchestrator"))
        );
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(tr.notes.lock().unwrap().len(), 1);
        // Dead pid, and the pane at the recorded address is somewhere else: note only.
        let route = plan(&[], "/ws", &[sess("/ws", "o:1.3", Some(999), "idle")]);
        let o = r.deliver(&tr, &req("ctx-4", "x", false), &route).unwrap();
        assert!(matches!(o, Outcome::Delivered { ref summary } if summary.contains("not found")));
        assert!(sent.lock().unwrap().is_empty());
        assert_eq!(tr.notes.lock().unwrap().len(), 2);
    }

    #[test]
    fn quick_reply_and_jump_use_the_pane_id() {
        let (r, sent) = router();
        let s = sess("/wt/a", "a:1.1", Some(11), "working");
        r.send_to_pane(&s, "ok", false).unwrap();
        assert_eq!(sent.lock().unwrap()[0].0, "%1");
        assert_eq!(r.jump(&s).unwrap(), "jumped to a:1.1");
        let gone = sess("/wt/z", "z:1.1", None, "idle");
        assert!(r.send_to_pane(&gone, "ok", false).is_err());
        assert!(r.jump(&gone).is_err());
    }

    fn open_router(sessions: &[&str], in_tmux: bool) -> (Router, Calls) {
        let t = FakeTmux {
            table: std_table(),
            sessions: sessions.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let calls = t.calls.clone();
        let r = Router {
            tmux: Box::new(t),
            dry_run: false,
            in_tmux,
            store: None,
        };
        (r, calls)
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn w_jumps_to_the_live_session_pane() {
        let (r, calls) = open_router(&["a-ctx-1"], true);
        let s = [sess("/wt/a", "a:1.1", Some(11), "idle")];
        let route = plan(&["/wt/a"], "/ws", &s);
        let m = r.open_task(Some(&route), &names(&["a-ctx-1"]), Some("/wt/a"));
        assert_eq!(m, "jumped to a:1.1");
        assert_eq!(*calls.lock().unwrap(), vec!["switch-client %1"]);
    }

    #[test]
    fn w_with_a_dead_session_opens_a_window() {
        let (r, calls) = open_router(&[], true);
        let s = [sess("/wt/a", "z:9.9", Some(999), "idle")];
        let route = plan(&["/wt/a"], "/ws", &s);
        let m = r.open_task(Some(&route), &[], Some("/wt/a"));
        assert!(m.ends_with("opened new window in /wt/a"), "{m}");
        assert_eq!(*calls.lock().unwrap(), vec!["new-window /wt/a"]);
        // No session at all.
        let (r, calls) = open_router(&[], true);
        let route = plan(&["/wt/b"], "/x", &[]);
        let m = r.open_task(Some(&route), &[], Some("/wt/b"));
        assert_eq!(m, "no live session; opened new window in /wt/b");
        assert_eq!(*calls.lock().unwrap(), vec!["new-window /wt/b"]);
    }

    #[test]
    fn w_never_jumps_to_the_orchestrator() {
        let (r, calls) = open_router(&[], true);
        let route = plan(&["/wt/b"], "/ws", &[sess("/ws", "o:1.2", Some(21), "idle")]);
        assert!(matches!(route, Route::Orchestrator(_)));
        r.open_task(Some(&route), &[], Some("/wt/b"));
        assert_eq!(*calls.lock().unwrap(), vec!["new-window /wt/b"]);
    }

    #[test]
    fn w_falls_back_to_the_task_named_tmux_session() {
        let (r, calls) = open_router(&["legacy-b"], true);
        let route = plan(&["/wt/b"], "/x", &[]);
        let m = r.open_task(
            Some(&route),
            &names(&["b-ctx-2", "legacy-b"]),
            Some("/wt/b"),
        );
        assert_eq!(m, "jumped to session legacy-b");
        assert_eq!(*calls.lock().unwrap(), vec!["switch-client =legacy-b"]);
    }

    #[test]
    fn w_outside_tmux_gives_an_attach_hint() {
        let (r, calls) = open_router(&[], false);
        let s = [sess("/wt/a", "a:1.1", Some(11), "idle")];
        let route = plan(&["/wt/a"], "/ws", &s);
        let m = r.open_task(Some(&route), &[], Some("/wt/a"));
        assert!(m.contains("tmux attach -t '%1'"), "{m}");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn dry_run_touches_nothing() {
        let r = Router::new(true);
        let tr = FakeTracker::default();
        let route = plan(&["/wt"], "/ws", &[sess("/wt", "real:1.1", None, "idle")]);
        r.deliver(&tr, &req("ctx-5", "x", false), &route).unwrap();
        assert!(tr.notes.lock().unwrap().is_empty());
    }

    /// `w` against real tmux: needs a client to move, so run it inside a throwaway server
    /// (`tmux -L x`), with `FERMATA_W_TEST=1` and `--ignored`. It skips otherwise.
    #[test]
    #[ignore]
    fn open_task_moves_the_real_client() {
        if std::env::var("FERMATA_W_TEST").as_deref() != Ok("1")
            || std::env::var_os("TMUX").is_none()
        {
            eprintln!("skipped: set FERMATA_W_TEST=1 and run inside a throwaway tmux server");
            return;
        }
        let tag = std::process::id();
        let (live, named) = (format!("fw-live-{tag}"), format!("fw-named-{tag}"));
        let dir = std::env::temp_dir()
            .to_string_lossy()
            .trim_end_matches('/')
            .to_string();
        for n in [&live, &named] {
            tmux(&["new-session", "-d", "-s", n, "-c", &dir, "sleep 60"]).unwrap();
        }
        let pane = |n: &str| {
            tmux(&[
                "display",
                "-p",
                "-t",
                &format!("={n}:"),
                "#{pane_id} #{pane_pid}",
            ])
        };
        let lp = pane(&live).unwrap();
        let (lid, lpid) = lp.trim().split_once(' ').unwrap();
        let client = || tmux(&["list-clients", "-F", "#{client_session}"]).unwrap();
        let r = Router {
            tmux: Box::new(RealTmux),
            dry_run: false,
            in_tmux: true,
            store: None,
        };
        let s = Session {
            claude_pid: lpid.parse().ok(),
            tmux_target: Some(format!("{live}:0.0")),
            worktree: dir.clone(),
            status: "idle".into(),
            ..Default::default()
        };
        let m1 = r.open_task(Some(&Route::Session(Box::new(s))), &[], Some(&dir));
        let c1 = client();
        let m2 = r.open_task(None, &["fw-absent".into(), named.clone()], Some(&dir));
        let c2 = client();
        for n in [&live, &named] {
            let _ = tmux(&["kill-session", "-t", &format!("={n}")]);
        }
        assert!(m1.starts_with("jumped to"), "{m1}");
        assert!(c1.contains(&live), "{c1} (pane {lid})");
        assert_eq!(m2, format!("jumped to session {named}"));
        assert!(c2.contains(&named), "{c2}");
    }

    /// Real tmux against a scratch session this test creates and kills. Run with `--ignored`.
    /// The pane runs `sh -c 'cat'`, so the "claude" pid is cat and the pane pid its parent.
    #[test]
    #[ignore]
    fn scratch_tmux_roundtrip() {
        let name = format!("fermata-scratch-{}", std::process::id());
        let dir = std::env::temp_dir();
        let dir = dir.to_string_lossy().trim_end_matches('/').to_string();
        tmux(&[
            "new-session",
            "-d",
            "-s",
            &name,
            "-c",
            &dir,
            "-x",
            "120",
            "-y",
            "20",
            "sh -c 'cat; :'",
        ])
        .unwrap();
        // A second pane before the first shifts nothing by pid, but would by address.
        tmux(&[
            "split-window",
            "-b",
            "-t",
            &format!("={name}:"),
            "-c",
            &dir,
            "sleep 30",
        ])
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let t = panes::real_table(Duration::ZERO).unwrap();
        let shell = t
            .panes
            .iter()
            .find(|p| p.addr.starts_with(&format!("{name}:")) && p.command != "sleep")
            .expect("scratch pane")
            .clone();
        let cat = t
            .ppid
            .iter()
            .find(|(_, pp)| **pp == shell.pid)
            .map(|(c, _)| *c)
            .expect("cat under the pane shell");
        let s = Session {
            claude_pid: Some(cat),
            tmux_target: Some(format!("{name}:0.0")),
            worktree: dir.clone(),
            status: "idle".into(),
            ..Default::default()
        };
        let r = Router {
            tmux: Box::new(RealTmux),
            dry_run: false,
            in_tmux: true,
            store: None,
        };
        let resolved = r.resolve(&s);
        let destroy = panes::resolve(&t, &s, Use::Destroy);
        let tr = FakeTracker::default();
        let o = r.deliver(
            &tr,
            &req("ctx-x", "hello $HOME; Enter", false),
            &Route::Orchestrator(Box::new(s)),
        );
        std::thread::sleep(Duration::from_millis(300));
        let pane = tmux(&["capture-pane", "-p", "-t", &shell.id]).unwrap_or_default();
        let _ = tmux(&["kill-session", "-t", &format!("={name}")]);
        let resolved = resolved.unwrap();
        assert_eq!(resolved.id, shell.id);
        assert!(resolved.by_pid);
        assert_eq!(destroy.unwrap().id, shell.id);
        assert!(
            matches!(o.unwrap(), Outcome::Delivered { ref summary } if summary.starts_with("sent")),
        );
        assert!(
            pane.contains("[fermata ctx-x] hello $HOME; Enter"),
            "pane: {pane}"
        );
        assert_eq!(tr.notes.lock().unwrap().len(), 1);
    }
}
