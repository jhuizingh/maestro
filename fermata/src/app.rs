//! TUI state, key handling and the event loop.
use crate::ccmon::Ccmon;
use crate::config::{self, Context, Priority};
use crate::core::{Loader, Snapshot, sort_tasks};
use crate::history::{self, Item};
use crate::model::{JoinedTask, Session};
use crate::routing::{self, Outcome, Route, Router};
use crate::sessions;
use crate::transcript::{self, Entry, Kind};
use crate::watch::{self, Change};
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const DEBOUNCE: Duration = Duration::from_secs(2);
const SIG_CHECK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum View {
    Tasks,
    Detail,
    Decisions,
    Sessions,
    Viewer,
    Help,
}

pub enum Msg {
    Snapshot(Snapshot),
    Refreshed(Box<JoinedTask>),
    Timeline(String, Vec<Item>),
    Outcome(Outcome, InputTarget, String),
    Status(String),
    /// A session action finished: show it and re-read sessions.
    Action(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputTarget {
    Task { context: String, id: String },
    Pane { target: String, status: String },
    Describe { session: Box<Session> },
}

pub struct Input {
    pub buf: String,
    pub target: InputTarget,
    pub label: String,
}

/// Pending yes/no for anything that types into a busy pane or touches live sessions.
pub enum Confirm {
    Send { text: String, target: InputTarget },
    Close(Box<Session>),
    Reopen(Box<Session>),
    Restore,
}

impl Confirm {
    pub fn prompt(&self) -> String {
        match self {
            Confirm::Send { .. } => "target is working — send anyway? (y/n)".into(),
            Confirm::Close(s) => format!(
                "close {} ({})? y = kill claude + pane · k = kill claude only · n = cancel",
                s.tmux_target.clone().unwrap_or_default(),
                s.worktree
            ),
            Confirm::Reopen(s) => format!(
                "reopen {} with claude --resume in tmux {}? (y/n)",
                &s.session_id[..s.session_id.len().min(8)],
                s.tmux_session.clone().unwrap_or_default()
            ),
            Confirm::Restore => {
                "restore the latest ccmon snapshot (relaunches sessions)? (y/n)".into()
            }
        }
    }
}

#[derive(Default)]
pub struct Viewer {
    pub title: String,
    pub entries: Vec<Entry>,
    pub expanded: HashSet<usize>,
    /// Pre-wrapped lines: (text, entry index, is entry head).
    pub lines: Vec<(String, usize, bool)>,
    pub width: u16,
    pub cursor: usize,
    pub scroll: usize,
    pub query: String,
    pub searching: bool,
    pub matches: Vec<usize>,
    pub note: Option<String>,
}

pub fn wrap(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = vec![];
    for raw in s.split('\n') {
        let chars: Vec<char> = raw.chars().collect();
        if chars.is_empty() {
            out.push(String::new());
            continue;
        }
        for c in chars.chunks(width) {
            out.push(c.iter().collect());
        }
    }
    out
}

impl Viewer {
    pub fn new(title: String, entries: Vec<Entry>, note: Option<String>) -> Self {
        Self {
            title,
            entries,
            note,
            ..Default::default()
        }
    }

    pub fn rebuild(&mut self, width: u16) {
        self.width = width;
        let w = width.saturating_sub(4) as usize;
        self.lines.clear();
        for (i, e) in self.entries.iter().enumerate() {
            let tag = match e.kind {
                Kind::User => "▶ user",
                Kind::Assistant => "◀ assistant",
                Kind::Tool => "  ⚙",
                Kind::ToolResult => "  ↳",
            };
            if e.collapsible() {
                let open = self.expanded.contains(&i);
                let mark = if open { "▾" } else { "▸" };
                self.lines
                    .push((format!("{tag} {mark} {}", e.head), i, true));
                if open {
                    for l in wrap(&e.body, w.saturating_sub(4)) {
                        self.lines.push((format!("      {l}"), i, false));
                    }
                }
            } else {
                let ts =
                    e.ts.as_deref()
                        .map(|t| format!("  {t}"))
                        .unwrap_or_default();
                self.lines.push((format!("{tag}{ts}"), i, true));
                for l in wrap(&e.body, w.saturating_sub(2)) {
                    self.lines.push((format!("  {l}"), i, false));
                }
                self.lines.push((String::new(), i, false));
            }
        }
        self.cursor = self.cursor.min(self.lines.len().saturating_sub(1));
        self.find();
    }

    pub fn find(&mut self) {
        self.matches = if self.query.is_empty() {
            vec![]
        } else {
            let q = self.query.to_lowercase();
            let hits: HashSet<usize> = transcript::search(&self.entries, &q).into_iter().collect();
            // Matches inside collapsed bodies land on the entry head.
            self.lines
                .iter()
                .enumerate()
                .filter(|(_, (t, i, head))| {
                    t.to_lowercase().contains(&q)
                        || (*head
                            && hits.contains(i)
                            && !self.expanded.contains(i)
                            && self.entries[*i].collapsible())
                })
                .map(|(n, _)| n)
                .collect()
        };
    }

    pub fn jump(&mut self, forward: bool) {
        if self.matches.is_empty() {
            return;
        }
        let next = if forward {
            self.matches.iter().find(|&&m| m > self.cursor)
        } else {
            self.matches.iter().rev().find(|&&m| m < self.cursor)
        };
        self.cursor = *next.unwrap_or(if forward {
            &self.matches[0]
        } else {
            self.matches.last().unwrap()
        });
    }

    pub fn toggle(&mut self) {
        let Some((_, i, _)) = self.lines.get(self.cursor) else {
            return;
        };
        let i = *i;
        if !self.entries[i].collapsible() {
            return;
        }
        if !self.expanded.remove(&i) {
            self.expanded.insert(i);
        }
        let w = self.width;
        self.rebuild(w);
        if let Some(p) = self.lines.iter().position(|(_, e, h)| *e == i && *h) {
            self.cursor = p;
        }
    }

    pub fn move_by(&mut self, d: isize) {
        let n = self.lines.len() as isize;
        self.cursor = (self.cursor as isize + d).clamp(0, (n - 1).max(0)) as usize;
    }

    pub fn fit(&mut self, height: usize) {
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if height > 0 && self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
    }
}

pub struct App {
    pub loader: Arc<Loader>,
    pub router: Arc<Router>,
    pub snap: Snapshot,
    pub view: View,
    pub back: Vec<View>,
    pub sel: usize,
    pub dsel: usize,
    pub ssel: usize,
    pub tsel: usize,
    pub detail_id: Option<String>,
    pub timeline: Vec<Item>,
    pub viewer: Option<Viewer>,
    pub input: Option<Input>,
    pub confirm: Option<Confirm>,
    pub ccmon: Arc<Ccmon>,
    pub show_closed: bool,
    pub closed: Vec<Session>,
    pub status: String,
    pub loading: bool,
    pub dry_run: bool,
    pub quit: bool,
    tx: Sender<Msg>,
    rt: tokio::runtime::Runtime,
    last_load: Option<Instant>,
    sig_check: bool,
    last_sig_check: Option<Instant>,
    pub mouse: bool,
    pending: Option<Instant>,
}

impl App {
    pub fn new(dry_run: bool, tx: Sender<Msg>) -> Result<Self> {
        Ok(Self {
            loader: Arc::new(Loader::new()),
            router: Arc::new(Router::new(dry_run)),
            snap: Snapshot::default(),
            view: View::Tasks,
            back: vec![],
            sel: 0,
            dsel: 0,
            ssel: 0,
            tsel: 0,
            detail_id: None,
            timeline: vec![],
            viewer: None,
            input: None,
            confirm: None,
            ccmon: Arc::new(Ccmon::new(dry_run)),
            show_closed: false,
            closed: vec![],
            status: "loading…".into(),
            loading: false,
            dry_run,
            quit: false,
            tx,
            rt: tokio::runtime::Runtime::new()?,
            last_load: None,
            sig_check: false,
            last_sig_check: None,
            mouse: false,
            pending: None,
        })
    }

    pub fn reload(&mut self, force: bool) {
        if self.loading {
            self.pending = Some(Instant::now());
            return;
        }
        self.loading = true;
        self.last_load = Some(Instant::now());
        let loader = self.loader.clone();
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let snap = loader.load(force).await;
            let _ = tx.send(Msg::Snapshot(snap));
        });
    }

    /// Tracker events only count when the write signature moved (reads touch files too).
    pub fn on_change(&mut self, c: Change) {
        match c {
            Change::Sessions => self.rematch_sessions(),
            Change::Tasks => self.sig_check = true,
        }
    }

    fn check_sigs(&mut self) {
        if !self.sig_check || self.last_sig_check.is_some_and(|t| t.elapsed() < SIG_CHECK) {
            return;
        }
        self.sig_check = false;
        self.last_sig_check = Some(Instant::now());
        let now = Loader::current_sigs(&self.snap.contexts);
        let known = self.loader.sigs.lock().unwrap().clone();
        if now.iter().any(|(k, v)| known.get(k) != Some(v)) {
            self.pending.get_or_insert_with(Instant::now);
        }
    }

    pub fn toggle_mouse(&mut self) {
        self.mouse = !self.mouse;
        let r = if self.mouse {
            ratatui::crossterm::execute!(std::io::stdout(), EnableMouseCapture)
        } else {
            ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture)
        };
        self.status = match r {
            Ok(()) if self.mouse => "mouse on (wheel scrolls; text selection off)".into(),
            Ok(()) => "mouse off (text selection works)".into(),
            Err(e) => format!("mouse: {e}"),
        };
    }

    /// Session files change on every tool call: re-read them and re-badge, no tracker calls.
    pub fn rematch_sessions(&mut self) {
        self.snap.sessions = sessions::live_sessions();
        for t in &mut self.snap.tasks {
            t.session = sessions::session_for(&self.snap.sessions, &t.worktrees()).cloned();
            t.badge = crate::core::badge(t);
        }
        let sel_id = self.snap.tasks.get(self.sel).map(|t| t.task.id.clone());
        sort_tasks(&mut self.snap.tasks);
        if let Some(id) = sel_id
            && let Some(i) = self.snap.tasks.iter().position(|t| t.task.id == id)
        {
            self.sel = i;
        }
    }

    pub fn tick(&mut self) {
        self.check_sigs();
        if let Some(p) = self.pending
            && !self.loading
            && p.elapsed() >= Duration::from_millis(300)
            && self.last_load.is_none_or(|l| l.elapsed() >= DEBOUNCE)
        {
            self.pending = None;
            self.reload(false);
        }
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Snapshot(s) => {
                self.loading = false;
                let n = s.tasks.len();
                let errs = s.errors.len();
                self.snap = s;
                self.sel = self.sel.min(n.saturating_sub(1));
                self.status = if errs > 0 {
                    format!("{n} tasks · {errs} error(s): {}", self.snap.errors[0])
                } else {
                    format!("{n} tasks")
                };
            }
            Msg::Refreshed(j) => {
                if let Some(t) = self.snap.tasks.iter_mut().find(|t| t.task.id == j.task.id) {
                    *t = *j;
                }
                sort_tasks(&mut self.snap.tasks);
                self.status = "refreshed".into();
                self.load_timeline();
            }
            Msg::Timeline(id, items) => {
                if self.detail_id.as_deref() == Some(id.as_str()) {
                    self.timeline = items;
                    self.tsel = self.tsel.min(self.timeline.len().saturating_sub(1));
                }
            }
            Msg::Outcome(o, target, text) => match o {
                Outcome::NeedsConfirm { target: t, status } => {
                    self.status = format!("{t} is {status} — send anyway? (y/n)");
                    self.confirm = Some(Confirm::Send { text, target });
                }
                Outcome::Delivered { summary } => self.status = summary,
            },
            Msg::Status(s) => self.status = s,
            Msg::Action(s) => {
                self.status = s;
                self.closed = sessions::closed_sessions();
                self.rematch_sessions();
            }
        }
    }

    pub fn tasks(&self) -> &[JoinedTask] {
        &self.snap.tasks
    }

    pub fn decisions(&self) -> Vec<&JoinedTask> {
        self.snap
            .tasks
            .iter()
            .filter(|t| t.decision.is_some())
            .collect()
    }

    pub fn current(&self) -> Option<&JoinedTask> {
        match self.view {
            View::Decisions => self.decisions().get(self.dsel).copied(),
            View::Detail | View::Viewer => self
                .detail_id
                .as_ref()
                .and_then(|id| self.snap.tasks.iter().find(|t| &t.task.id == id)),
            _ => self.snap.tasks.get(self.sel),
        }
    }

    fn context(&self, name: &str) -> Option<&Context> {
        self.snap.contexts.iter().find(|c| c.name == name)
    }

    fn go(&mut self, v: View) {
        if self.view != v {
            self.back.push(self.view);
            self.view = v;
        }
    }

    fn go_back(&mut self) {
        self.view = self.back.pop().unwrap_or(View::Tasks);
        if self.view != View::Viewer {
            self.viewer = None;
        }
    }

    fn open_detail(&mut self, id: String) {
        self.detail_id = Some(id);
        self.timeline.clear();
        self.tsel = 0;
        self.go(View::Detail);
        self.load_timeline();
    }

    fn load_timeline(&mut self) {
        let Some(t) = self.current().cloned() else {
            return;
        };
        let mut all: Vec<Session> = self.snap.sessions.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            all.extend(sessions::closed_sessions());
            let items = history::timeline(&t, &all);
            let _ = tx.send(Msg::Timeline(t.task.id.clone(), items));
        });
    }

    fn open_url(&mut self, url: Option<String>, what: &str) {
        match url {
            Some(u) => match open::that_detached(&u) {
                Ok(()) => self.status = format!("opened {u}"),
                Err(e) => self.status = format!("open failed: {e}"),
            },
            None => self.status = format!("no {what} link"),
        }
    }

    fn link_key(&mut self, c: char) -> bool {
        let Some(t) = self.current() else {
            return false;
        };
        let (url, what) = match c {
            'o' => (
                t.ticket
                    .as_ref()
                    .and_then(|i| i.url.clone())
                    .or(t.ticket_url.clone()),
                "ticket",
            ),
            'e' => (t.epic_url(), "epic"),
            'p' => (t.pr.as_ref().and_then(|p| p.url.clone()), "PR"),
            'c' => (
                t.pr.as_ref().and_then(|p| {
                    p.checks
                        .failing_url
                        .clone()
                        .or(p.checks.latest_url.clone())
                        .or(p.url.as_ref().map(|u| format!("{u}/checks")))
                }),
                "CI",
            ),
            'w' => {
                let wt = t.worktrees().last().map(|s| s.to_string());
                match wt {
                    Some(w) => {
                        self.status = match self.router.tmux.new_window(&w) {
                            Ok(()) => format!("opened tmux window in {w}"),
                            Err(e) => format!("tmux: {e}"),
                        }
                    }
                    None => self.status = "no worktree".into(),
                }
                return true;
            }
            _ => return false,
        };
        self.open_url(url, what);
        true
    }

    fn start_input(&mut self, prefill: String) {
        let Some(t) = self.current() else {
            self.status = "no task selected".into();
            return;
        };
        let target = InputTarget::Task {
            context: t.context.clone(),
            id: t.task.id.clone(),
        };
        let route = self.route_for(t);
        let label = match &route {
            Some(Route::Session { target, .. }) => format!("{} → session {target}", t.task.id),
            Some(Route::Orchestrator { target, .. }) => {
                format!("{} → orchestrator {target}", t.task.id)
            }
            _ => format!("{} → note only", t.task.id),
        };
        self.input = Some(Input {
            buf: prefill,
            target,
            label,
        });
    }

    fn route_for(&self, t: &JoinedTask) -> Option<Route> {
        let c = self.context(&t.context)?;
        Some(routing::plan(
            &t.worktrees(),
            &c.workspace.to_string_lossy(),
            &self.snap.sessions,
        ))
    }

    fn send(&mut self, target: InputTarget, text: String, confirmed: bool) {
        let tx = self.tx.clone();
        let router = self.router.clone();
        match &target {
            InputTarget::Task { context, id } => {
                let Some(t) = self.snap.tasks.iter().find(|t| &t.task.id == id) else {
                    return;
                };
                let (Some(c), Some(route)) = (self.context(context), self.route_for(t)) else {
                    self.status = "unknown context".into();
                    return;
                };
                let tracker = self.loader.tracker(c);
                let id = id.clone();
                let target2 = target.clone();
                self.status = "sending…".into();
                std::thread::spawn(move || {
                    let m = match router.deliver(&*tracker, &id, &text, &route, confirmed) {
                        Ok(o) => Msg::Outcome(o, target2, text),
                        Err(e) => Msg::Status(format!("send failed: {e}")),
                    };
                    let _ = tx.send(m);
                });
            }
            InputTarget::Describe { session } => {
                let s2 = session.clone();
                self.run_action(move |c| c.describe(&s2, &text));
            }
            InputTarget::Pane { target: p, status } => {
                let m = match router.send_to_pane(p, status, &text, confirmed) {
                    Ok(o) => Msg::Outcome(o, target.clone(), text),
                    Err(e) => Msg::Status(format!("send failed: {e}")),
                };
                let _ = tx.send(m);
            }
        }
    }

    fn toggle_pin(&mut self, epic: bool) {
        let Some(t) = self.current() else { return };
        let key = if epic {
            t.ticket
                .as_ref()
                .and_then(|i| i.epic.as_ref())
                .map(|e| e.key.clone())
        } else {
            t.ticket_key.clone()
        };
        let Some(key) = key else {
            self.status = format!("no {} to pin", if epic { "epic" } else { "ticket" });
            return;
        };
        let Some(c) = self.context(&t.context).cloned() else {
            return;
        };
        let pin = if epic {
            Priority {
                epic: Some(key.clone()),
                ticket: None,
            }
        } else {
            Priority {
                ticket: Some(key.clone()),
                epic: None,
            }
        };
        let cur = c
            .adapter
            .as_ref()
            .map(|a| a.priorities.clone())
            .unwrap_or_default();
        let next = config::toggle_pin(&cur, pin);
        let pinned = next.len() > cur.len();
        match config::save_priorities(&c, &next) {
            Ok(()) => {
                self.status = format!("{} {key}", if pinned { "pinned" } else { "unpinned" });
                self.reload(false);
            }
            Err(e) => self.status = format!("save priorities: {e}"),
        }
    }

    fn open_item(&mut self) {
        let Some(it) = self.timeline.get(self.tsel).cloned() else {
            return;
        };
        if let Some(p) = &it.transcript
            && it.source != "worker"
        {
            self.open_transcript(p.clone(), it.title.clone());
            return;
        }
        let mut entries = vec![Entry {
            kind: Kind::Assistant,
            ts: it.ts.clone(),
            head: String::new(),
            body: it.body.clone(),
        }];
        if let Some(p) = &it.transcript {
            entries.push(Entry {
                kind: Kind::Tool,
                ts: None,
                head: format!("transcript: {p} (press t to open)"),
                body: p.clone(),
            });
        }
        self.viewer = Some(Viewer::new(
            format!("{} — {}", it.source, it.title),
            entries,
            it.transcript.clone(),
        ));
        self.go(View::Viewer);
    }

    fn open_transcript(&mut self, path: String, title: String) {
        match transcript::load(std::path::Path::new(&path)) {
            Ok(t) => {
                let note = (t.skipped_bytes > 0)
                    .then(|| format!("showing the tail; skipped {} MB", t.skipped_bytes >> 20));
                let mut v = Viewer::new(title, t.entries, None);
                if let Some(n) = note {
                    self.status = n;
                }
                v.cursor = usize::MAX;
                self.viewer = Some(v);
                self.go(View::Viewer);
            }
            Err(e) => self.status = format!("transcript: {e}"),
        }
    }

    fn open_log(&mut self, s: &Session) {
        let p = sessions::ccmon_dir()
            .join("logs")
            .join(format!("{}.log", s.session_id));
        let body =
            history::tail(&p, 256 * 1024).unwrap_or_else(|| format!("{}: unreadable", p.display()));
        let entries = vec![Entry {
            kind: Kind::Assistant,
            ts: None,
            head: String::new(),
            body,
        }];
        let mut v = Viewer::new(format!("ccmon log {}", s.session_id), entries, None);
        v.cursor = usize::MAX;
        self.viewer = Some(v);
        self.go(View::Viewer);
    }

    pub fn sessions_sorted(&self) -> Vec<&Session> {
        if self.show_closed {
            self.closed.iter().collect()
        } else {
            self.snap.sessions.iter().collect()
        }
    }

    /// Run a ccmon action off the UI thread; the result lands as `Msg::Action`.
    fn run_action<F>(&mut self, f: F)
    where
        F: FnOnce(&Ccmon) -> Result<String> + Send + 'static,
    {
        let c = self.ccmon.clone();
        let tx = self.tx.clone();
        self.status = "working…".into();
        std::thread::spawn(move || {
            let m = match f(&c) {
                Ok(s) => s,
                Err(e) => format!("failed: {e}"),
            };
            let _ = tx.send(Msg::Action(m));
        });
    }

    pub fn task_for_session(&self, s: &Session) -> Option<&JoinedTask> {
        self.snap.tasks.iter().find(|t| {
            t.worktrees()
                .iter()
                .any(|w| sessions::same_path(&s.worktree, w))
        })
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if let Some(c) = self.confirm.take() {
            let yes = matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y'));
            match c {
                Confirm::Send { text, target } if yes => self.send(target, text, true),
                Confirm::Close(s) if yes || k.code == KeyCode::Char('k') => {
                    let kill_pane = yes;
                    self.run_action(move |c| c.close(&s, kill_pane));
                }
                Confirm::Reopen(s) if yes => self.run_action(move |c| c.reopen(&s)),
                Confirm::Restore if yes => self.run_action(|c| c.snapshot_restore()),
                _ => self.status = "cancelled".into(),
            }
            return;
        }
        if self.input.is_some() {
            self.on_input_key(k);
            return;
        }
        if self.view == View::Viewer
            && let Some(v) = self.viewer.as_mut()
            && v.searching
        {
            match k.code {
                KeyCode::Enter => {
                    v.searching = false;
                    v.find();
                    v.jump(true);
                }
                KeyCode::Esc => {
                    v.searching = false;
                    v.query.clear();
                    v.find();
                }
                KeyCode::Backspace => {
                    v.query.pop();
                }
                KeyCode::Char(ch) => v.query.push(ch),
                _ => {}
            }
            return;
        }
        let code = k.code;
        // Global keys.
        match code {
            KeyCode::Char('q') if self.view != View::Viewer => {
                self.quit = true;
                return;
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => {
                self.go_back();
                return;
            }
            KeyCode::Char('?') => {
                self.go(View::Help);
                return;
            }
            KeyCode::Char('m') => {
                self.toggle_mouse();
                return;
            }
            KeyCode::Char('t') if self.view != View::Sessions && self.view != View::Viewer => {
                self.back.clear();
                self.view = View::Tasks;
                return;
            }
            KeyCode::Char('d') if self.view != View::Viewer => {
                self.back.clear();
                self.view = View::Decisions;
                return;
            }
            KeyCode::Char('s') if self.view != View::Viewer => {
                self.back.clear();
                self.view = View::Sessions;
                return;
            }
            KeyCode::Char('R') => {
                self.loader.forget_contexts();
                self.status = "reloading…".into();
                self.reload(true);
                return;
            }
            KeyCode::Char(':') | KeyCode::Char('i')
                if matches!(self.view, View::Tasks | View::Detail | View::Decisions) =>
            {
                self.start_input(String::new());
                return;
            }
            _ => {}
        }
        match self.view {
            View::Tasks => self.keys_tasks(code),
            View::Detail => self.keys_detail(code),
            View::Decisions => self.keys_decisions(code),
            View::Sessions => self.keys_sessions(code),
            View::Viewer => self.keys_viewer(code),
            View::Help => {}
        }
    }

    fn on_input_key(&mut self, k: KeyEvent) {
        let Some(inp) = self.input.as_mut() else {
            return;
        };
        match k.code {
            KeyCode::Esc => {
                self.input = None;
                self.status = "cancelled".into();
            }
            KeyCode::Enter => {
                let inp = self.input.take().unwrap();
                if inp.buf.trim().is_empty() {
                    self.status = "empty — not sent".into();
                } else {
                    self.send(inp.target, inp.buf, false);
                }
            }
            KeyCode::Backspace => {
                inp.buf.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => inp.buf.clear(),
            KeyCode::Char(c) => inp.buf.push(c),
            _ => {}
        }
    }

    fn keys_tasks(&mut self, code: KeyCode) {
        let n = self.tasks().len();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(t) = self.tasks().get(self.sel) {
                    let id = t.task.id.clone();
                    self.open_detail(id);
                }
            }
            KeyCode::Char('r') => {
                self.status = "reloading…".into();
                self.reload(true);
            }
            KeyCode::Char('*') => self.toggle_pin(false),
            KeyCode::Char('+') => self.toggle_pin(true),
            KeyCode::Char(c) => {
                self.link_key(c);
            }
            _ => {}
        }
    }

    fn keys_detail(&mut self, code: KeyCode) {
        let n = self.timeline.len();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.tsel = (self.tsel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.tsel = self.tsel.saturating_sub(1),
            KeyCode::Enter => self.open_item(),
            KeyCode::Char('r') => self.refresh_detail(),
            KeyCode::Char('a') => self.start_input(String::new()),
            KeyCode::Char('*') => self.toggle_pin(false),
            KeyCode::Char('+') => self.toggle_pin(true),
            KeyCode::Char(c) if c.is_ascii_digit() => self.pick_option(c),
            KeyCode::Char(c) => {
                self.link_key(c);
            }
            _ => {}
        }
    }

    fn refresh_detail(&mut self) {
        let Some(t) = self.current() else { return };
        let Some(c) = self.context(&t.context).cloned() else {
            return;
        };
        let id = t.task.id.clone();
        let live = self.snap.sessions.clone();
        let loader = self.loader.clone();
        let tx = self.tx.clone();
        self.status = format!("refreshing {id}…");
        std::thread::spawn(move || {
            let m = match loader.refresh_one(&c, &id, &live) {
                Ok(j) => Msg::Refreshed(Box::new(j)),
                Err(e) => Msg::Status(format!("refresh {id}: {e}")),
            };
            let _ = tx.send(m);
        });
    }

    fn pick_option(&mut self, c: char) {
        let n = c.to_digit(10).unwrap_or(0) as usize;
        let Some(d) = self.current().and_then(|t| t.decision.clone()) else {
            return;
        };
        match d.options.get(n.wrapping_sub(1)) {
            Some(o) => self.start_input(format!("Option {n}: {o}")),
            None => self.status = format!("no option {n}"),
        }
    }

    fn keys_decisions(&mut self, code: KeyCode) {
        let n = self.decisions().len();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.dsel = (self.dsel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.dsel = self.dsel.saturating_sub(1),
            KeyCode::Char('a') => self.start_input(String::new()),
            KeyCode::Char(c) if c.is_ascii_digit() => self.pick_option(c),
            KeyCode::Enter => {
                if let Some(t) = self.decisions().get(self.dsel) {
                    let id = t.task.id.clone();
                    self.open_detail(id);
                }
            }
            KeyCode::Char(c) => {
                self.link_key(c);
            }
            _ => {}
        }
    }

    fn keys_sessions(&mut self, code: KeyCode) {
        let list: Vec<Session> = self.sessions_sorted().into_iter().cloned().collect();
        let n = list.len();
        let cur = list.get(self.ssel).cloned();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.ssel = (self.ssel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.ssel = self.ssel.saturating_sub(1),
            KeyCode::Char('X') => {
                self.show_closed = !self.show_closed;
                self.closed = sessions::closed_sessions();
                self.ssel = 0;
            }
            KeyCode::Char('e') => {
                if let Some(s) = cur {
                    self.input = Some(Input {
                        buf: s.description.clone().unwrap_or_default(),
                        label: format!("describe {}", &s.session_id[..s.session_id.len().min(8)]),
                        target: InputTarget::Describe {
                            session: Box::new(s),
                        },
                    });
                }
            }
            KeyCode::Char('x') => {
                if let Some(s) = cur.filter(|s| !s.closed) {
                    let c = Confirm::Close(Box::new(s));
                    self.status = c.prompt();
                    self.confirm = Some(c);
                }
            }
            KeyCode::Char('S') => self.run_action(|c| c.snapshot_save()),
            KeyCode::Char('L') => {
                let c = Confirm::Restore;
                self.status = c.prompt();
                self.confirm = Some(c);
            }
            KeyCode::Enter if self.show_closed => {
                if let Some(s) = cur {
                    let c = Confirm::Reopen(Box::new(s));
                    self.status = c.prompt();
                    self.confirm = Some(c);
                }
            }
            KeyCode::Enter => {
                if let Some(t) = cur.and_then(|s| s.tmux_target) {
                    self.status = match self.router.tmux.switch_client(&t) {
                        Ok(()) if self.dry_run => format!("dry-run: would jump to {t}"),
                        Ok(()) => format!("jumped to {t}"),
                        Err(e) => format!("jump: {e}"),
                    };
                }
            }
            KeyCode::Char('r') => {
                if let Some(s) = cur
                    && let Some(t) = s.tmux_target.clone()
                {
                    self.input = Some(Input {
                        buf: String::new(),
                        label: format!("reply → {t}"),
                        target: InputTarget::Pane {
                            target: t,
                            status: s.status.clone(),
                        },
                    });
                }
            }
            KeyCode::Char('l') => {
                if let Some(s) = cur {
                    self.open_log(&s);
                }
            }
            KeyCode::Char('v') => {
                if let Some(s) = cur {
                    match history::session_transcript(&s) {
                        Some(p) => self.open_transcript(
                            p.to_string_lossy().into_owned(),
                            format!("session {}", s.session_id),
                        ),
                        None => self.status = "no transcript found".into(),
                    }
                }
            }
            KeyCode::Char('o') => {
                if let Some(id) = cur
                    .as_ref()
                    .and_then(|s| self.task_for_session(s))
                    .map(|t| t.task.id.clone())
                {
                    self.open_detail(id);
                }
            }
            _ => {}
        }
    }

    fn keys_viewer(&mut self, code: KeyCode) {
        let Some(v) = self.viewer.as_mut() else {
            return;
        };
        match code {
            KeyCode::Down | KeyCode::Char('j') => v.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => v.move_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => v.move_by(20),
            KeyCode::PageUp | KeyCode::Char('b') => v.move_by(-20),
            KeyCode::Home | KeyCode::Char('g') => v.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => v.cursor = v.lines.len().saturating_sub(1),
            KeyCode::Enter => v.toggle(),
            KeyCode::Char('/') => {
                v.searching = true;
                v.query.clear();
            }
            KeyCode::Char('n') => v.jump(true),
            KeyCode::Char('N') => v.jump(false),
            KeyCode::Char('t') => {
                if let Some(p) = v.note.clone() {
                    let title = v.title.clone();
                    self.open_transcript(p, title);
                }
            }
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, kind: MouseEventKind) {
        let code = match kind {
            MouseEventKind::ScrollDown => KeyCode::Down,
            MouseEventKind::ScrollUp => KeyCode::Up,
            _ => return,
        };
        self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }
}

pub fn run(dry_run: bool) -> Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
    let (wtx, wrx) = channel::<Change>();
    let mut app = App::new(dry_run, tx)?;
    app.reload(false);
    let mut errs = vec![];
    let trackers: Vec<_> = app
        .loader
        .contexts(&mut errs)
        .into_iter()
        .filter_map(|c| c.tracker_dir)
        .collect();
    let _watchers = watch::start(trackers, sessions::ccmon_dir().join("sessions"), wtx);

    let mut term = ratatui::init();
    let res = (|| -> Result<()> {
        while !app.quit {
            term.draw(|f| crate::ui::draw(f, &mut app))?;
            if event::poll(Duration::from_millis(150))? {
                match event::read()? {
                    Event::Key(k) => app.on_key(k),
                    Event::Mouse(m) => app.on_mouse(m.kind),
                    _ => {}
                }
            }
            while let Ok(m) = rx.try_recv() {
                app.on_msg(m);
            }
            while let Ok(c) = wrx.try_recv() {
                app.on_change(c);
            }
            app.tick();
        }
        Ok(())
    })();
    if app.mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        let e = |kind, head: &str, body: &str| Entry {
            kind,
            ts: None,
            head: head.into(),
            body: body.into(),
        };
        vec![
            e(Kind::User, "", "hello world"),
            e(Kind::Tool, "Bash: ls", "{\"command\": \"secret-needle\"}"),
            e(Kind::Assistant, "", "line1\nline2"),
        ]
    }

    #[test]
    fn viewer_collapse_expand_search() {
        let mut v = Viewer::new("t".into(), entries(), None);
        v.rebuild(80);
        let collapsed = v.lines.len();
        v.cursor = v.lines.iter().position(|(_, i, _)| *i == 1).unwrap();
        v.toggle();
        assert!(v.lines.len() > collapsed);
        v.toggle();
        assert_eq!(v.lines.len(), collapsed);
        v.query = "needle".into();
        v.find();
        assert_eq!(v.matches.len(), 1);
        v.cursor = 0;
        v.jump(true);
        assert_eq!(v.lines[v.cursor].1, 1);
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn destructive_session_actions_need_confirmation() {
        let (tx, rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap.sessions = vec![Session {
            session_id: "s-1".into(),
            tmux_target: Some("nowhere:9.9".into()),
            worktree: "/nonexistent".into(),
            ..Default::default()
        }];
        app.view = View::Sessions;
        app.on_key(key('x'));
        assert!(matches!(app.confirm, Some(Confirm::Close(_))));
        app.on_key(key('n'));
        assert!(app.confirm.is_none());
        assert_eq!(app.status, "cancelled");
        assert!(rx.try_recv().is_err(), "nothing ran");
        app.on_key(key('L'));
        assert!(matches!(app.confirm, Some(Confirm::Restore)));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.confirm.is_none());
        assert!(rx.try_recv().is_err(), "nothing ran");
        // Closed list: Enter asks before reopening.
        app.show_closed = true;
        app.closed = vec![Session {
            session_id: "s-2".into(),
            closed: true,
            ..Default::default()
        }];
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.confirm, Some(Confirm::Reopen(_))));
    }

    #[test]
    fn mouse_starts_off() {
        let (tx, _rx) = channel();
        assert!(!App::new(true, tx).unwrap().mouse);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("abcdefghij", 8), vec!["abcdefgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 8), vec!["a", "", "b"]);
    }
}
