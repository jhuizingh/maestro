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
    Pane { session: Box<Session> },
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

/// A list view's `/` filter: the query, and whether the prompt is still open.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Filter {
    pub query: String,
    pub editing: bool,
}

impl Filter {
    /// Every space-separated word must appear in `text`, ignoring case.
    pub fn matches(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.query
            .to_lowercase()
            .split_whitespace()
            .all(|w| text.contains(w))
    }

    pub fn is_on(&self) -> bool {
        self.editing || !self.query.trim().is_empty()
    }
}

/// The text a row shows, which is what its filter searches.
pub fn task_text(t: &JoinedTask) -> String {
    format!(
        "{} {} {} {} {} {}",
        t.task.id,
        t.badge.label(),
        t.task.title,
        t.waiting_text().unwrap_or_default(),
        t.ticket_key.as_deref().unwrap_or_default(),
        t.context
    )
}

fn session_text(s: &Session, task: Option<&str>) -> String {
    format!(
        "{} {} {} {} {} {}",
        if s.closed { "closed" } else { &s.status },
        s.tmux_target.as_deref().unwrap_or_default(),
        s.worktree,
        s.branch.as_deref().unwrap_or_default(),
        task.unwrap_or_default(),
        s.description.as_deref().unwrap_or_default()
    )
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
    /// The list the detail was opened from, shown beside it in the wide layout.
    pub detail_from: View,
    /// Wide layout only: whether j/k move that list (true) or the timeline.
    pub list_focus: bool,
    /// Terminal width at the last draw; picks the layout.
    pub width: u16,
    /// Scroll offset of the timeline item preview.
    pub pscroll: u16,
    pub marks: crate::theme::Marks,
    pub timeline: Vec<Item>,
    pub viewer: Option<Viewer>,
    pub input: Option<Input>,
    pub confirm: Option<Confirm>,
    pub tfilter: Filter,
    pub dfilter: Filter,
    pub sfilter: Filter,
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
            detail_from: View::Tasks,
            list_focus: true,
            width: 0,
            pscroll: 0,
            marks: Default::default(),
            timeline: vec![],
            viewer: None,
            input: None,
            confirm: None,
            tfilter: Filter::default(),
            dfilter: Filter::default(),
            sfilter: Filter::default(),
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
        let before = self.picks();
        self.snap.sessions = sessions::live_sessions();
        for t in &mut self.snap.tasks {
            t.session = sessions::session_for(&self.snap.sessions, &t.worktrees()).cloned();
            t.badge = crate::core::badge(t);
        }
        sort_tasks(&mut self.snap.tasks);
        self.restore(before);
    }

    /// What each list's cursor is on, by id, so a refresh can keep it there.
    fn picks(&self) -> Picks {
        Picks {
            task: self
                .visible_tasks()
                .get(self.sel)
                .map(|t| t.task.id.clone()),
            decision: self
                .visible_decisions()
                .get(self.dsel)
                .map(|t| t.task.id.clone()),
            session: self
                .sessions_sorted()
                .get(self.ssel)
                .map(|s| s.session_id.clone()),
        }
    }

    /// Put each cursor back on the item it was on, wherever that item now sits. An item that
    /// left the list leaves its cursor at the same row, clamped to the new length.
    fn restore(&mut self, p: Picks) {
        fn place<T>(v: &[T], id: Option<&str>, key: impl Fn(&T) -> &str, at: usize) -> usize {
            id.and_then(|id| v.iter().position(|x| key(x) == id))
                .unwrap_or_else(|| at.min(v.len().saturating_sub(1)))
        }
        let t = self.visible_tasks();
        self.sel = place(&t, p.task.as_deref(), |t| &t.task.id, self.sel);
        let d = self.visible_decisions();
        self.dsel = place(&d, p.decision.as_deref(), |t| &t.task.id, self.dsel);
        let s = self.sessions_sorted();
        self.ssel = place(&s, p.session.as_deref(), |s| &s.session_id, self.ssel);
        // Wide detail whose item left the list: follow the cursor instead.
        if self.view == View::Detail && self.wide() {
            let gone = !self
                .origin_list()
                .iter()
                .any(|t| Some(&t.task.id) == self.detail_id.as_ref());
            if gone {
                self.follow_list();
            }
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
                let picks = self.picks();
                self.snap = s;
                self.marks = crate::theme::Marks::from_tasks(&self.snap.tasks);
                self.restore(picks);
                self.status = if errs > 0 {
                    format!("{n} tasks · {errs} error(s): {}", self.snap.errors[0])
                } else {
                    format!("{n} tasks")
                };
            }
            Msg::Refreshed(j) => {
                let picks = self.picks();
                if let Some(t) = self.snap.tasks.iter_mut().find(|t| t.task.id == j.task.id) {
                    *t = *j;
                }
                sort_tasks(&mut self.snap.tasks);
                self.marks = crate::theme::Marks::from_tasks(&self.snap.tasks);
                self.restore(picks);
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
                let picks = self.picks();
                self.closed = sessions::closed_sessions();
                self.rematch_sessions();
                self.restore(picks);
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

    /// The task list as shown: narrowed by the Tasks filter.
    pub fn visible_tasks(&self) -> Vec<&JoinedTask> {
        self.snap
            .tasks
            .iter()
            .filter(|t| self.tfilter.matches(&task_text(t)))
            .collect()
    }

    pub fn visible_decisions(&self) -> Vec<&JoinedTask> {
        self.decisions()
            .into_iter()
            .filter(|t| {
                self.dfilter
                    .matches(&format!("{} {}", t.task.id, t.task.title))
            })
            .collect()
    }

    /// The filter of the list view on screen, if it is one.
    pub fn filter(&self) -> Option<&Filter> {
        match self.view {
            View::Tasks => Some(&self.tfilter),
            View::Decisions => Some(&self.dfilter),
            View::Sessions => Some(&self.sfilter),
            _ => None,
        }
    }

    fn filter_mut(&mut self) -> Option<&mut Filter> {
        match self.view {
            View::Tasks => Some(&mut self.tfilter),
            View::Decisions => Some(&mut self.dfilter),
            View::Sessions => Some(&mut self.sfilter),
            _ => None,
        }
    }

    /// Change the current list's filter, keeping the cursor on its item if it still
    /// matches, else putting it on the first row.
    fn edit_filter(&mut self, f: impl FnOnce(&mut Filter)) {
        let before = self.picks();
        let Some(flt) = self.filter_mut() else { return };
        f(flt);
        if !flt.editing && flt.query.trim().is_empty() {
            flt.query.clear();
        }
        self.restore(before.clone());
        let (cur, was) = match self.view {
            View::Tasks => (self.picks().task, before.task),
            View::Decisions => (self.picks().decision, before.decision),
            _ => (self.picks().session, before.session),
        };
        if cur != was {
            match self.view {
                View::Tasks => self.sel = 0,
                View::Decisions => self.dsel = 0,
                _ => self.ssel = 0,
            }
        }
    }

    fn on_filter_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => self.edit_filter(|f| f.editing = false),
            KeyCode::Esc => self.edit_filter(|f| *f = Filter::default()),
            KeyCode::Backspace => self.edit_filter(|f| {
                f.query.pop();
            }),
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                self.edit_filter(|f| f.query.clear())
            }
            KeyCode::Up | KeyCode::Down => match self.view {
                View::Tasks => self.keys_tasks(k.code),
                View::Decisions => self.keys_decisions(k.code),
                _ => self.keys_sessions(k.code),
            },
            KeyCode::Char(c) => self.edit_filter(|f| f.query.push(c)),
            _ => {}
        }
    }

    pub fn current(&self) -> Option<&JoinedTask> {
        match self.view {
            View::Decisions => self.visible_decisions().get(self.dsel).copied(),
            View::Detail | View::Viewer => self
                .detail_id
                .as_ref()
                .and_then(|id| self.snap.tasks.iter().find(|t| &t.task.id == id)),
            _ => self.visible_tasks().get(self.sel).copied(),
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
        if matches!(self.view, View::Tasks | View::Decisions) {
            self.detail_from = self.view;
        }
        self.list_focus = true;
        self.detail_id = Some(id);
        self.timeline.clear();
        self.tsel = 0;
        self.pscroll = 0;
        self.go(View::Detail);
        self.load_timeline();
    }

    /// Whether the current width gets the split detail layout.
    pub fn wide(&self) -> bool {
        crate::ui::split(self.width).is_some()
    }

    /// The list the detail came from, as shown (filtered).
    pub fn origin_list(&self) -> Vec<&JoinedTask> {
        match self.detail_from {
            View::Decisions => self.visible_decisions(),
            _ => self.visible_tasks(),
        }
    }

    /// The id under the cursor of the list the detail came from.
    fn list_pick(&self) -> Option<String> {
        let at = match self.detail_from {
            View::Decisions => self.dsel,
            _ => self.sel,
        };
        self.origin_list().get(at).map(|t| t.task.id.clone())
    }

    /// Wide layout: point the detail at the list's selected item.
    fn follow_list(&mut self) {
        let Some(id) = self.list_pick() else { return };
        if self.detail_id.as_deref() != Some(id.as_str()) {
            self.detail_id = Some(id);
            self.timeline.clear();
            self.tsel = 0;
            self.pscroll = 0;
            self.load_timeline();
        }
    }

    fn move_list(&mut self, code: KeyCode) {
        let n = self.origin_list().len();
        let cur = match self.detail_from {
            View::Decisions => &mut self.dsel,
            _ => &mut self.sel,
        };
        *cur = match code {
            KeyCode::Down | KeyCode::Char('j') => (*cur + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::Char('k') => cur.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => 0,
            _ => n.saturating_sub(1),
        };
        self.follow_list();
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
            Some(Route::Session(s)) => {
                format!("{} → session {}", t.task.id, self.router.pane_label(s))
            }
            Some(Route::Orchestrator(s)) => {
                format!("{} → orchestrator {}", t.task.id, self.router.pane_label(s))
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
            InputTarget::Pane { session } => {
                let m = match router.send_to_pane(session, &text, confirmed) {
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

    /// The live or closed session list as shown: narrowed by the Sessions filter.
    pub fn sessions_sorted(&self) -> Vec<&Session> {
        let all = if self.show_closed {
            &self.closed
        } else {
            &self.snap.sessions
        };
        all.iter()
            .filter(|s| {
                let task = self.task_for_session(s).map(|t| t.task.id.as_str());
                self.sfilter.matches(&session_text(s, task))
            })
            .collect()
    }

    pub fn session_total(&self) -> usize {
        if self.show_closed {
            self.closed.len()
        } else {
            self.snap.sessions.len()
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
        if self.filter().is_some_and(|f| f.editing) {
            self.on_filter_key(k);
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
            KeyCode::Esc if self.filter().is_some_and(Filter::is_on) => {
                self.edit_filter(|f| *f = Filter::default());
                return;
            }
            KeyCode::Char('/') if self.filter().is_some() => {
                self.edit_filter(|f| f.editing = true);
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
        let n = self.visible_tasks().len();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.sel = (self.sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.sel = self.sel.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sel = n.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(t) = self.visible_tasks().get(self.sel) {
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
        match code {
            KeyCode::Char('J') | KeyCode::PageDown => {
                self.pscroll = self.pscroll.saturating_add(5);
                return;
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.pscroll = self.pscroll.saturating_sub(5);
                return;
            }
            _ => {}
        }
        let before = self.tsel;
        self.keys_detail_move(code);
        if self.tsel != before {
            self.pscroll = 0;
        }
    }

    fn keys_detail_move(&mut self, code: KeyCode) {
        let n = self.timeline.len();
        if self.wide() {
            match code {
                KeyCode::Tab => {
                    self.list_focus = !self.list_focus;
                    return;
                }
                KeyCode::Enter if self.list_focus => {
                    self.list_focus = false;
                    return;
                }
                KeyCode::Down
                | KeyCode::Up
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::Char('j' | 'k' | 'g' | 'G')
                    if self.list_focus =>
                {
                    self.move_list(code);
                    return;
                }
                _ => {}
            }
        }
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
        let n = self.visible_decisions().len();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.dsel = (self.dsel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.dsel = self.dsel.saturating_sub(1),
            KeyCode::Char('a') => self.start_input(String::new()),
            KeyCode::Char(c) if c.is_ascii_digit() => self.pick_option(c),
            KeyCode::Enter => {
                if let Some(t) = self.visible_decisions().get(self.dsel) {
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
                if let Some(s) = cur {
                    self.status = match self.router.jump(&s) {
                        Ok(m) => m,
                        Err(e) => format!("jump: {e}"),
                    };
                }
            }
            KeyCode::Char('r') => {
                if let Some(s) = cur {
                    self.input = Some(Input {
                        buf: String::new(),
                        label: format!("reply → {}", self.router.pane_label(&s)),
                        target: InputTarget::Pane {
                            session: Box::new(s),
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

/// The ids under the task, decision and session cursors.
#[derive(Clone)]
struct Picks {
    task: Option<String>,
    decision: Option<String>,
    session: Option<String>,
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

    fn snap_of(ids: &[&str]) -> Snapshot {
        let tasks = ids
            .iter()
            .map(|id| {
                let mut t = crate::core::tests::jt(id);
                t.decision = Some(Default::default());
                t
            })
            .collect();
        Snapshot {
            tasks,
            ..Default::default()
        }
    }

    fn picked(app: &App) -> (String, String) {
        (
            app.visible_tasks()[app.sel].task.id.clone(),
            app.visible_decisions()[app.dsel].task.id.clone(),
        )
    }

    #[test]
    fn selection_follows_its_item_across_a_reload() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap = snap_of(&["a", "b", "c"]);
        (app.sel, app.dsel) = (1, 1);
        // "a" was decided and dropped off: the cursor stays on "b", now one row up.
        app.on_msg(Msg::Snapshot(snap_of(&["b", "c"])));
        assert_eq!(picked(&app), ("b".into(), "b".into()));
        // Reordered: still "b".
        app.on_msg(Msg::Snapshot(snap_of(&["c", "d", "b"])));
        assert_eq!(picked(&app), ("b".into(), "b".into()));
        // "b" itself left: the cursor keeps its row, clamped to the shorter list.
        app.on_msg(Msg::Snapshot(snap_of(&["c", "d"])));
        assert_eq!(picked(&app), ("d".into(), "d".into()));
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn wide_detail_walks_the_filtered_list() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.on_msg(Msg::Snapshot(titled(&[
            ("a-1", "keep"),
            ("b-2", "drop"),
            ("c-3", "keep"),
        ])));
        app.width = 160;
        app.on_key(key('/'));
        typed(&mut app, "keep");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.detail_id.as_deref(), Some("a-1"));
        press(&mut app, KeyCode::Char('j'));
        assert_eq!(app.detail_id.as_deref(), Some("c-3"), "b-2 is filtered out");
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            (app.view, app.tfilter.query.as_str()),
            (View::Tasks, "keep")
        );
    }

    #[test]
    fn wide_detail_follows_the_list() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.on_msg(Msg::Snapshot(snap_of(&["a", "b", "c"])));
        app.width = 160;
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            (app.view, app.detail_id.as_deref()),
            (View::Detail, Some("a"))
        );
        press(&mut app, KeyCode::Char('j'));
        assert_eq!((app.sel, app.detail_id.as_deref()), (1, Some("b")));
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(app.detail_id.as_deref(), Some("c"));
        // Timeline focus: j no longer moves the list.
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!((app.sel, app.detail_id.as_deref()), (2, Some("c")));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('k'));
        assert_eq!(app.detail_id.as_deref(), Some("b"));
        // Narrow: j/k are the timeline's whatever the focus, and the open item stays.
        app.width = 100;
        press(&mut app, KeyCode::Char('k'));
        assert_eq!((app.sel, app.detail_id.as_deref()), (1, Some("b")));
        // Back to wide: same item, same focus.
        app.width = 160;
        press(&mut app, KeyCode::Char('k'));
        assert_eq!((app.sel, app.detail_id.as_deref()), (0, Some("a")));
        // Esc returns to the list with its cursor on the item last shown.
        press(&mut app, KeyCode::Esc);
        assert_eq!((app.view, app.sel), (View::Tasks, 0));
    }

    #[test]
    fn preview_scroll_resets_when_the_highlight_moves() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.view = View::Detail;
        app.width = 100;
        app.timeline = vec![Default::default(), Default::default()];
        press(&mut app, KeyCode::Char('J'));
        assert_eq!(app.pscroll, 5);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!((app.tsel, app.pscroll), (1, 0));
    }

    #[test]
    fn wide_detail_from_decisions_moves_the_decision_cursor() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.on_msg(Msg::Snapshot(snap_of(&["a", "b"])));
        app.width = 160;
        app.view = View::Decisions;
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!((app.dsel, app.sel), (1, 0));
        assert_eq!(app.detail_id.as_deref(), Some("b"));
        // The open item leaves the list: the detail follows the clamped cursor.
        app.on_msg(Msg::Snapshot(snap_of(&["a"])));
        assert_eq!(app.detail_id.as_deref(), Some("a"));
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

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn typed(app: &mut App, s: &str) {
        s.chars().for_each(|c| app.on_key(key(c)));
    }

    fn shown(app: &App) -> Vec<String> {
        app.visible_tasks()
            .iter()
            .map(|t| t.task.id.clone())
            .collect()
    }

    fn titled(ids: &[(&str, &str)]) -> Snapshot {
        let mut s = snap_of(&ids.iter().map(|(i, _)| *i).collect::<Vec<_>>());
        for (t, (_, title)) in s.tasks.iter_mut().zip(ids) {
            t.task.title = (*title).into();
        }
        s
    }

    #[test]
    fn filter_narrows_as_you_type_and_esc_clears_it() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap = titled(&[("a-1", "Fix Login"), ("b-2", "docs"), ("c-3", "login page")]);
        app.on_key(key('/'));
        assert!(app.tfilter.editing);
        // Keys that are commands elsewhere ('t', 'q') type into the prompt.
        typed(&mut app, "lo");
        assert_eq!(shown(&app), ["a-1", "c-3"]);
        typed(&mut app, "gin pa");
        assert_eq!(shown(&app), ["c-3"], "every word must match, any case");
        assert!(!app.quit);
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(shown(&app), ["a-1", "c-3"]);
        app.on_key(esc());
        assert_eq!(app.tfilter, Filter::default());
        assert_eq!(shown(&app).len(), 3);
        assert_eq!(app.view, View::Tasks);
    }

    #[test]
    fn enter_locks_the_filter_and_esc_then_unfilters() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap = titled(&[("a-1", "alpha"), ("b-2", "beta"), ("c-3", "gamma")]);
        app.on_key(key('d'));
        app.on_key(key('/'));
        typed(&mut app, "b-2");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.dfilter.editing);
        assert_eq!(app.visible_decisions().len(), 1);
        // Locked: keys are commands again, and the filter survives a reload.
        app.on_key(key('t'));
        assert_eq!(app.view, View::Tasks);
        assert_eq!(shown(&app).len(), 3, "each list has its own filter");
        app.on_key(key('d'));
        app.on_msg(Msg::Snapshot(titled(&[("b-2", "beta"), ("d-4", "delta")])));
        assert_eq!(app.current().unwrap().task.id, "b-2");
        // Esc on the locked view unfilters, without leaving it; a second Esc goes back.
        app.back = vec![View::Tasks];
        app.on_key(esc());
        assert_eq!(app.view, View::Decisions);
        assert_eq!(app.visible_decisions().len(), 2);
        app.on_key(esc());
        assert_eq!(app.view, View::Tasks);
    }

    #[test]
    fn cursor_keeps_its_item_or_goes_to_the_top() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap = titled(&[("a-1", "x"), ("b-2", "xy"), ("c-3", "xyz")]);
        app.sel = 2;
        app.on_key(key('/'));
        typed(&mut app, "xy");
        assert_eq!(app.current().unwrap().task.id, "c-3");
        typed(&mut app, "zz");
        assert!(app.current().is_none());
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.current().unwrap().task.id, "c-3", "first row of [c-3]");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.current().unwrap().task.id, "c-3");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.current().unwrap().task.id,
            "b-2",
            "Up moves while typing"
        );
        // Locking an empty filter clears it.
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.tfilter.is_on());
    }

    #[test]
    fn sessions_filter_on_branch_and_task() {
        let (tx, _rx) = channel();
        let mut app = App::new(true, tx).unwrap();
        let s = |id: &str, br: &str| Session {
            session_id: id.into(),
            branch: Some(br.into()),
            worktree: format!("/wt/{id}"),
            ..Default::default()
        };
        app.snap.sessions = vec![s("s-1", "feature-x"), s("s-2", "bugfix-y")];
        app.view = View::Sessions;
        app.on_key(key('/'));
        typed(&mut app, "bugfix");
        let ids: Vec<_> = app
            .sessions_sorted()
            .iter()
            .map(|s| s.session_id.clone())
            .collect();
        assert_eq!(ids, ["s-2"]);
        // A filter on the Viewer has no meaning: '/' there is its own search.
        app.on_key(esc());
        app.view = View::Viewer;
        app.viewer = Some(Viewer::new("t".into(), entries(), None));
        app.on_key(key('/'));
        assert!(app.viewer.as_ref().unwrap().searching);
        assert!(!app.sfilter.editing);
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
