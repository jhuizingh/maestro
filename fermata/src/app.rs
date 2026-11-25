//! TUI state, key handling and the event loop.
use crate::api::{
    self, Answer, Api, Badge, Code, Command, Context, Decision, DecisionStatus, Delivery,
    DeliveryState, Entry, EntryKind, Error, Item, Phase, PinTarget, Query, Reply, RouteKind,
    Session, State, Task, TaskRef, Verb,
};
use crate::desk::Desk;
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const PANE_ONLY: &str = "only its pane can answer that: Enter (in Decisions) goes there";

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum View {
    Tasks,
    Detail,
    Decisions,
    Sessions,
    Viewer,
    Help,
    /// The context picker (`C`).
    Contexts,
    /// Each external source's health (`H`).
    Health,
}

/// Whose items the list views show; see "Information architecture" in DESIGN.md.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Scope {
    /// Every context, each in its own block under a heading.
    #[default]
    All,
    /// One context only.
    One(String),
}

pub enum Msg {
    Timeline(String, Vec<Item>),
    /// A task's conversation, built off the UI thread.
    Conversation(String, Vec<Entry>),
    Status(String),
    /// A command finished.
    Done {
        verb: Verb,
        after: After,
        result: Box<Result<Reply, Error>>,
    },
}

/// What a finished command does beyond showing its result.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum After {
    /// Nothing; an error is shown after this prefix.
    Say(&'static str),
    /// Re-read the closed sessions.
    Session,
    /// Re-read the timeline.
    Detail,
}

impl After {
    fn prefix(self) -> &'static str {
        match self {
            After::Say(p) => p,
            After::Session => "failed: ",
            After::Detail => "",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputTarget {
    /// Input on a task; `decision` is the key or id of the one it replies to, and `ask` sends
    /// it as a question whatever it ends with.
    Task {
        task: TaskRef,
        decision: Option<String>,
        ask: bool,
    },
    Pane {
        session: String,
    },
    Describe {
        session: String,
    },
}

pub struct Input {
    pub buf: String,
    pub target: InputTarget,
    pub label: String,
}

/// Pending yes/no for anything that types into a busy pane or touches live sessions: the
/// command to repeat with `token` on a yes.
#[derive(Debug, Clone)]
pub struct Confirm {
    pub verb: Verb,
    token: String,
    message: String,
    after: After,
}

impl Confirm {
    pub fn prompt(&self) -> String {
        let keys = match self.verb {
            Verb::Close { .. } => "y = kill claude + pane · k = kill claude only · n = cancel",
            _ => "(y/n)",
        };
        format!("{} {keys}", self.message)
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
pub fn task_text(t: &Task) -> String {
    format!(
        "{} {} {} {} {} {}",
        t.id,
        t.badge_text,
        t.title,
        t.waiting.as_deref().unwrap_or_default(),
        t.ticket_key.as_deref().unwrap_or_default(),
        t.context
    )
}

/// How many Sessions rows get an index key: `0`-`9`, then `M-a`..`M-z`.
pub const INDEX_KEYS: usize = 36;

/// The index key shown on Sessions row `i` (empty past the last one).
pub fn index_label(i: usize) -> String {
    match i {
        0..=9 => i.to_string(),
        10..INDEX_KEYS => format!("M-{}", (b'a' + (i - 10) as u8) as char),
        _ => String::new(),
    }
}

/// The Sessions row an index key picks: a bare digit, or Meta with a lowercase letter.
pub fn index_of_key(k: &KeyEvent) -> Option<usize> {
    match k.code {
        KeyCode::Char(c @ '0'..='9') if (k.modifiers - KeyModifiers::SHIFT).is_empty() => {
            Some(c as usize - '0' as usize)
        }
        KeyCode::Char(c @ 'a'..='z') if k.modifiers == KeyModifiers::ALT => {
            Some(10 + (c as u8 - b'a') as usize)
        }
        _ => None,
    }
}

fn session_text(s: &Session, task: Option<&str>) -> String {
    format!(
        "{} {} {} {} {} {}",
        if s.closed { "closed" } else { &s.status },
        s.tmux,
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
    /// Put the cursor on this entry at the next rebuild.
    pub start: Option<usize>,
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
                EntryKind::User => "▶ user",
                EntryKind::Assistant => "◀ assistant",
                EntryKind::Tool => "  ⚙",
                EntryKind::ToolResult => "  ↳",
            };
            let tag = e.label.as_deref().unwrap_or(tag);
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
        if let Some(i) = self.start.take()
            && let Some(p) = self.lines.iter().position(|(_, e, _)| *e == i)
        {
            self.cursor = p;
        }
        self.cursor = self.cursor.min(self.lines.len().saturating_sub(1));
        self.find();
    }

    pub fn find(&mut self) {
        self.matches = if self.query.is_empty() {
            vec![]
        } else {
            let q = self.query.to_lowercase();
            let hit = |e: &Entry| {
                e.head.to_lowercase().contains(&q) || e.body.to_lowercase().contains(&q)
            };
            // Matches inside collapsed bodies land on the entry head.
            self.lines
                .iter()
                .enumerate()
                .filter(|(_, (t, i, head))| {
                    t.to_lowercase().contains(&q)
                        || (*head
                            && hit(&self.entries[*i])
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

    /// The entry under the cursor.
    pub fn entry(&self) -> Option<&Entry> {
        let (_, i, _) = self.lines.get(self.cursor)?;
        self.entries.get(*i)
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
    api: Arc<dyn Api>,
    desk: Arc<Desk>,
    pub server: api::ServerInfo,
    /// With `--serve`: the API's address, or why it is not served.
    pub serving: Option<Result<String, String>>,
    pub snap: Arc<State>,
    pub view: View,
    pub back: Vec<View>,
    pub sel: usize,
    pub dsel: usize,
    pub ssel: usize,
    pub tsel: usize,
    /// Context picker cursor.
    pub csel: usize,
    pub scope: Scope,
    /// `u`: show everyone's tasks rather than only yours.
    pub everyone: bool,
    /// The context `M` returns to: the last one picked.
    pub last_ctx: Option<String>,
    pub detail_id: Option<String>,
    /// Tasks whose registry entries were read for the detail since the last snapshot.
    registry_read: HashSet<String>,
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
    /// Viewers a transcript was opened over, for Esc.
    under: Vec<Viewer>,
    pub input: Option<Input>,
    pub confirm: Option<Confirm>,
    pub tfilter: Filter,
    pub dfilter: Filter,
    pub sfilter: Filter,
    pub show_closed: bool,
    pub closed: Vec<Session>,
    pub status: String,
    pub quit: bool,
    tx: Sender<Msg>,
    pub mouse: bool,
    /// The last notice shown.
    seen_notice: u64,
}

impl App {
    pub fn new(api: Arc<dyn Api>, desk: Arc<Desk>, tx: Sender<Msg>) -> Self {
        let server = match api.query(Query::Server) {
            Ok(Answer::Server(i)) => i,
            _ => Default::default(),
        };
        let seen_notice = api.state().notices.last().map_or(0, |n| n.seq);
        Self {
            api,
            desk,
            server,
            serving: None,
            snap: Default::default(),
            view: View::Tasks,
            back: vec![],
            sel: 0,
            dsel: 0,
            ssel: 0,
            tsel: 0,
            csel: 0,
            scope: Scope::All,
            everyone: false,
            last_ctx: None,
            detail_id: None,
            registry_read: HashSet::new(),
            detail_from: View::Tasks,
            list_focus: true,
            width: 0,
            pscroll: 0,
            marks: Default::default(),
            timeline: vec![],
            viewer: None,
            under: vec![],
            input: None,
            confirm: None,
            tfilter: Filter::default(),
            dfilter: Filter::default(),
            sfilter: Filter::default(),
            show_closed: false,
            closed: vec![],
            status: "loading…".into(),
            quit: false,
            tx,
            mouse: false,
            seen_notice,
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

    /// A task's deliveries, newest first.
    pub fn deliveries_for(&self, t: &Task) -> Vec<&Delivery> {
        self.snap
            .deliveries
            .iter()
            .rev()
            .filter(|d| d.task == t.id && d.context == t.context)
            .collect()
    }

    /// Take the engine's latest state, keeping each cursor on its item. False when it is the
    /// one already shown.
    pub fn sync(&mut self) -> bool {
        let st = self.api.state();
        if st.instance == self.snap.instance && st.rev == self.snap.rev {
            return false;
        }
        let picks = self.picks();
        let old = std::mem::replace(&mut self.snap, st);
        self.marks = crate::theme::Marks::from_tasks(&self.snap.tasks);
        self.restore(picks);
        let n = self.snap.tasks.len();
        if self.snap.phase == Phase::Preview && old.phase != Phase::Preview {
            self.status = format!("{n} tasks (cached) · loading…");
        }
        if self.snap.phase == Phase::Loaded && self.snap.loaded_at != old.loaded_at {
            self.registry_read.clear();
            self.status = self.loaded_text();
        }
        let fresh = self.snap.notices.iter().rfind(|n| n.seq > self.seen_notice);
        if let Some((seq, text)) = fresh.map(|n| (n.seq, n.text.clone())) {
            self.seen_notice = seq;
            self.status = text;
            self.load_closed();
        }
        true
    }

    /// The status line after a full load.
    fn loaded_text(&self) -> String {
        let n = self.snap.tasks.len();
        if let Scope::One(c) = &self.scope
            && self.context(c).is_none()
        {
            return format!("no context named {c} — C to pick one");
        }
        if let Some(e) = self.snap.errors.first() {
            return format!("{n} tasks · {} error(s): {e}", self.snap.errors.len());
        }
        let stale = |c: &Context| c.stale.as_ref().map(|s| (c.name.clone(), s.text()));
        match self.snap.contexts.iter().find_map(stale) {
            Some((c, st)) => format!("{n} tasks · {c}: {st}"),
            None => format!("{n} tasks"),
        }
    }

    /// Run a command off the UI thread; the result lands as `Msg::Done`.
    fn run_cmd(&mut self, c: impl Into<Command>, after: After) {
        let c = c.into();
        let (api, tx) = (self.api.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let verb = c.verb.clone();
            let result = api.command(c);
            let _ = tx.send(Msg::Done {
                verb,
                after,
                result: Box::new(result),
            });
        });
    }

    /// A session command; one that needs a yes asks before this returns.
    fn run_action(&mut self, verb: Verb) {
        let asks = matches!(
            verb,
            Verb::Close { .. } | Verb::Reopen { .. } | Verb::SnapshotRestore
        );
        if asks {
            let result = self.api.command(verb.clone().into());
            self.on_done(verb, After::Session, result);
            return;
        }
        self.status = "working…".into();
        self.run_cmd(verb, After::Session);
    }

    fn on_done(&mut self, verb: Verb, after: After, result: Result<Reply, Error>) {
        self.sync();
        match result {
            Ok(r) => self.status = r.summary,
            Err(Error {
                code: Code::NeedsConfirm,
                message,
                confirm: Some(token),
            }) => {
                let c = Confirm {
                    verb,
                    token,
                    message,
                    after,
                };
                self.status = c.prompt();
                self.confirm = Some(c);
                return;
            }
            Err(e) => {
                self.status = format!("{}{e}", after.prefix());
                if after == After::Detail {
                    return;
                }
            }
        }
        match after {
            After::Say(_) => {}
            After::Session => self.load_closed(),
            After::Detail => self.load_timeline(),
        }
    }

    fn load_closed(&mut self) {
        let Ok(Answer::Sessions(l)) = self.api.query(Query::Sessions { closed: true }) else {
            return;
        };
        let picks = self.picks();
        self.closed = l.sessions;
        self.restore(picks);
    }

    /// `A` on a task: resend its latest answer that failed or went unacknowledged.
    fn resend(&mut self) {
        let Some(t) = self.current() else { return };
        let Some(id) = self
            .deliveries_for(t)
            .into_iter()
            .find(|d| d.state == DeliveryState::Failed || d.pending())
            .filter(|d| d.resendable)
            .map(|d| d.id.clone())
        else {
            self.status = "nothing to resend: no unacknowledged answer on this task".into();
            return;
        };
        self.status = "sending…".into();
        self.run_cmd(Verb::Resend { delivery: id }, After::Say("send failed: "));
    }

    /// What each list's cursor is on, by id, so a refresh can keep it there.
    fn picks(&self) -> Picks {
        Picks {
            task: self.visible_tasks().get(self.sel).map(|t| t.id.clone()),
            decision: self.visible_decisions().get(self.dsel).map(|r| r.key()),
            session: self.sessions_sorted().get(self.ssel).map(|s| s.id.clone()),
        }
    }

    /// Put each cursor back on the item it was on, wherever that item now sits. An item that
    /// left the list leaves its cursor at the same row, clamped to the new length.
    fn restore(&mut self, p: Picks) {
        fn place<T>(v: &[T], id: Option<&str>, key: impl Fn(&T) -> String, at: usize) -> usize {
            id.and_then(|id| v.iter().position(|x| key(x) == id))
                .unwrap_or_else(|| at.min(v.len().saturating_sub(1)))
        }
        let t = self.visible_tasks();
        self.sel = place(&t, p.task.as_deref(), |t| t.id.clone(), self.sel);
        let d = self.visible_decisions();
        self.dsel = place(&d, p.decision.as_deref(), |r| r.key(), self.dsel);
        let s = self.sessions_sorted();
        self.ssel = place(&s, p.session.as_deref(), |s| s.id.clone(), self.ssel);
        // Wide detail whose item left the list: follow the cursor instead.
        if self.view == View::Detail && self.wide() {
            let gone = !self
                .origin_list()
                .iter()
                .any(|t| Some(&t.id) == self.detail_id.as_ref());
            if gone {
                self.follow_list();
            }
        }
    }

    pub fn on_msg(&mut self, m: Msg) {
        match m {
            Msg::Timeline(id, items) => {
                if self.detail_id.as_deref() == Some(id.as_str()) {
                    self.timeline = items;
                    self.tsel = self.tsel.min(self.timeline.len().saturating_sub(1));
                }
            }
            Msg::Conversation(id, entries) => self.show_conversation(id, entries),
            Msg::Status(s) => self.status = s,
            Msg::Done {
                verb,
                after,
                result,
            } => self.on_done(verb, after, *result),
        }
    }

    /// The scope's tasks, yours only unless `everyone`, in list order: grouped by context in
    /// all-contexts mode.
    pub fn tasks(&self) -> Vec<&Task> {
        let v = self.snap.tasks.iter().filter(|t| self.shown_to_you(t));
        self.arrange(v.collect(), |t| Some(&t.context))
    }

    fn shown_to_you(&self, t: &Task) -> bool {
        self.everyone || t.mine
    }

    /// Tasks the mine filter hides: in the scope, or in context `ctx` when given.
    pub fn hidden(&self, ctx: Option<&str>) -> usize {
        self.snap
            .tasks
            .iter()
            .filter(|t| match ctx {
                Some(c) => t.context == c,
                None => self.in_scope(Some(&t.context)),
            })
            .filter(|t| !self.shown_to_you(t))
            .count()
    }

    /// Contexts in the scope where who you are is unknown, so mine shows everyone there.
    pub fn unknown_me(&self) -> Vec<&str> {
        self.contexts()
            .filter(|c| self.in_scope(Some(&c.name)) && c.me.is_none())
            .map(|c| c.name.as_str())
            .collect()
    }

    /// The registered contexts: all but the entry for sessions of no context.
    pub fn contexts(&self) -> impl Iterator<Item = &Context> {
        self.snap.contexts.iter().filter(|c| !c.name.is_empty())
    }

    /// `u`: mine ↔ everyone, keeping each cursor on its item where it is still shown.
    fn toggle_everyone(&mut self) {
        let picks = self.picks();
        self.everyone = !self.everyone;
        self.restore(picks);
        self.status = if self.everyone {
            "everyone's tasks".into()
        } else {
            match self.hidden(None) {
                0 => "your tasks".into(),
                n => format!("your tasks · {n} hidden (u for everyone)"),
            }
        };
    }

    /// One row per unresolved decision, in task order; a task's own oldest first.
    pub fn decisions(&self) -> Vec<DecisionRow<'_>> {
        self.tasks()
            .into_iter()
            .flat_map(|t| {
                t.decisions.iter().map(move |d| DecisionRow {
                    task: t,
                    decision: d,
                })
            })
            .collect()
    }

    /// Decisions waiting on you, across the scope (answered ones wait on the orchestrator).
    pub fn open_decisions(&self) -> usize {
        self.decisions()
            .iter()
            .filter(|r| r.decision.status == DecisionStatus::Open)
            .count()
    }

    /// Tasks needing you outside the scope (always 0 in all-contexts mode).
    /// Shown tasks in the scope with no known state; never silently idle.
    pub fn state_unknown(&self) -> usize {
        self.tasks()
            .iter()
            .filter(|t| t.badge == Badge::Unknown)
            .count()
    }

    /// Stateless backlog not shown as rows: in the scope, or in context `ctx` when given.
    pub fn backlog(&self, ctx: Option<&str>) -> usize {
        self.contexts()
            .filter(|c| match ctx {
                Some(x) => c.name == x,
                None => self.in_scope(Some(&c.name)),
            })
            .map(|c| c.counts.backlog)
            .sum()
    }

    pub fn needs_elsewhere(&self) -> usize {
        self.snap
            .tasks
            .iter()
            .filter(|t| t.needs_you && !self.in_scope(Some(&t.context)))
            .count()
    }

    /// Whether the lists are cut into one block per context.
    pub fn grouped(&self) -> bool {
        self.scope == Scope::All
    }

    pub fn in_scope(&self, ctx: Option<&str>) -> bool {
        match &self.scope {
            Scope::All => true,
            Scope::One(n) => ctx == Some(n.as_str()),
        }
    }

    /// Keep the scope's items; in all-contexts mode, stable-sort them into registry order
    /// blocks, with items of no context last.
    fn arrange<'a, T>(
        &'a self,
        v: Vec<&'a T>,
        ctx: impl Fn(&'a T) -> Option<&'a str>,
    ) -> Vec<&'a T> {
        let mut v: Vec<&T> = v.into_iter().filter(|x| self.in_scope(ctx(x))).collect();
        if self.grouped() {
            let rank = |x: &'a T| {
                ctx(x)
                    .and_then(|c| self.contexts().position(|k| k.name == c))
                    .unwrap_or(usize::MAX)
            };
            v.sort_by_key(|x| rank(x));
        }
        v
    }

    /// Change the scope, keeping each cursor on its item where it is still shown.
    pub fn set_scope(&mut self, s: Scope) {
        let picks = self.picks();
        if let Scope::One(c) = &s {
            self.last_ctx = Some(c.clone());
        }
        self.scope = s;
        self.restore(picks);
        self.status = match &self.scope {
            Scope::All => "all contexts".into(),
            Scope::One(c) => format!("context: {c}"),
        };
    }

    /// `M`: all contexts ↔ the last context picked (the picker when there is none).
    fn toggle_mode(&mut self) {
        match (&self.scope, self.last_ctx.clone()) {
            (Scope::One(_), _) => self.set_scope(Scope::All),
            (Scope::All, Some(c)) => self.set_scope(Scope::One(c)),
            (Scope::All, None) => self.open_picker(),
        }
    }

    /// `[` / `]`: the previous / next context, in one-context mode only.
    fn step_context(&mut self, forward: bool) {
        let Scope::One(cur) = &self.scope else {
            self.status = "[ ] step contexts in one-context mode (M)".into();
            return;
        };
        let names: Vec<&str> = self.contexts().map(|c| c.name.as_str()).collect();
        if names.is_empty() {
            return;
        }
        let at = names.iter().position(|n| n == cur).unwrap_or(0);
        let n = names.len();
        let next = if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        };
        self.set_scope(Scope::One(names[next].to_string()));
    }

    fn open_picker(&mut self) {
        self.csel = match &self.scope {
            Scope::One(c) => self.contexts().position(|k| &k.name == c).unwrap_or(0),
            Scope::All => 0,
        };
        self.go(View::Contexts);
    }

    fn keys_contexts(&mut self, code: KeyCode) {
        let n = self.contexts().count();
        match code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.csel = (self.csel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.csel = self.csel.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.csel = 0,
            KeyCode::End | KeyCode::Char('G') => self.csel = n.saturating_sub(1),
            KeyCode::Enter => {
                let name = self.contexts().nth(self.csel).map(|c| c.name.clone());
                if let Some(name) = name {
                    self.go_back();
                    self.set_scope(Scope::One(name));
                }
            }
            _ => {}
        }
    }

    /// The task list as shown: the scope's tasks, narrowed by the Tasks filter.
    pub fn visible_tasks(&self) -> Vec<&Task> {
        self.tasks()
            .into_iter()
            .filter(|t| self.tfilter.matches(&task_text(t)))
            .collect()
    }

    pub fn visible_decisions(&self) -> Vec<DecisionRow<'_>> {
        self.decisions()
            .into_iter()
            .filter(|r| {
                let (t, d) = (r.task, r.decision);
                self.dfilter
                    .matches(&format!("{} {} {}", t.id, t.title, d.text))
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

    pub fn current(&self) -> Option<&Task> {
        match self.view {
            View::Decisions => self.visible_decisions().get(self.dsel).map(|r| r.task),
            View::Detail | View::Viewer => self
                .detail_id
                .as_ref()
                .and_then(|id| self.snap.tasks.iter().find(|t| &t.id == id)),
            _ => self.visible_tasks().get(self.sel).copied(),
        }
    }

    /// The views the scope keys work from: the lists and the detail.
    fn scoped_view(&self) -> bool {
        matches!(
            self.view,
            View::Tasks | View::Detail | View::Decisions | View::Sessions
        )
    }

    fn context(&self, name: &str) -> Option<&Context> {
        self.contexts().find(|c| c.name == name)
    }

    fn go(&mut self, v: View) {
        if self.view != v {
            self.back.push(self.view);
            self.view = v;
        }
    }

    fn go_back(&mut self) {
        if self.view == View::Viewer
            && let Some(v) = self.under.pop()
        {
            self.viewer = Some(v);
            return;
        }
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
        self.detail_id = Some(id.clone());
        self.timeline.clear();
        self.tsel = 0;
        self.pscroll = 0;
        self.go(View::Detail);
        self.load_timeline();
        if !self.registry_read.contains(&id) {
            self.refresh_detail(false);
        }
    }

    /// Whether the current width gets the split detail layout.
    pub fn wide(&self) -> bool {
        crate::ui::split(self.width).is_some()
    }

    /// The list the detail came from, as shown (filtered).
    pub fn origin_list(&self) -> Vec<&Task> {
        match self.detail_from {
            View::Decisions => self.visible_decisions().iter().map(|r| r.task).collect(),
            _ => self.visible_tasks(),
        }
    }

    /// The id under the cursor of the list the detail came from.
    fn list_pick(&self) -> Option<String> {
        let at = match self.detail_from {
            View::Decisions => self.dsel,
            _ => self.sel,
        };
        self.origin_list().get(at).map(|t| t.id.clone())
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
        let Some(t) = self.current().map(Task::task_ref) else {
            return;
        };
        let (api, tx) = (self.api.clone(), self.tx.clone());
        std::thread::spawn(move || {
            let m = match api.query(Query::Timeline(t.clone())) {
                Ok(Answer::Timeline(l)) => Msg::Timeline(t.id, l.items),
                Ok(_) => return,
                Err(e) => Msg::Status(format!("timeline: {e}")),
            };
            let _ = tx.send(m);
        });
    }

    /// Build the task's conversation; it opens when it is ready.
    fn load_conversation(&mut self) {
        let Some(t) = self.current().map(Task::task_ref) else {
            return;
        };
        let (api, tx) = (self.api.clone(), self.tx.clone());
        self.status = format!("reading the conversation of {}…", t.id);
        std::thread::spawn(move || {
            let m = match api.query(Query::Conversation(t.clone())) {
                Ok(Answer::Conversation(c)) => Msg::Conversation(t.id, c.entries),
                Ok(_) => return,
                Err(e) => Msg::Status(format!("conversation: {e}")),
            };
            let _ = tx.send(m);
        });
    }

    fn show_conversation(&mut self, id: String, entries: Vec<Entry>) {
        // You moved on while it loaded: do not pull you back.
        if self.view != View::Detail || self.detail_id.as_ref() != Some(&id) {
            return;
        }
        self.status = if entries.is_empty() {
            format!("nothing recorded about {id} yet")
        } else {
            String::new()
        };
        let mut v = Viewer::new(format!("conversation {id}"), entries, None);
        v.start = v.entries.len().checked_sub(1);
        self.viewer = Some(v);
        self.go(View::Viewer);
    }

    fn open_url(&mut self, url: Option<String>, what: &str) {
        self.status = match url {
            Some(u) => self.desk.open_url(&u),
            None => format!("no {what} link"),
        };
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
                let task = t.task_ref();
                let (api, desk, tx) = (self.api.clone(), self.desk.clone(), self.tx.clone());
                self.status = "finding the task's session…".into();
                std::thread::spawn(move || {
                    let names = desk.names(wt.as_deref());
                    let m = desk.open_task(task_pane(&*api, &task), &names, wt.as_deref());
                    let _ = tx.send(Msg::Status(m));
                });
                return true;
            }
            'W' => {
                let wt = t.worktrees().last().map(|s| s.to_string());
                self.status = match wt {
                    Some(w) => self.desk.new_window(&w),
                    None => "no worktree".into(),
                };
                return true;
            }
            _ => return false,
        };
        self.open_url(url, what);
        true
    }

    /// The decision input goes to: the selected row in Decisions, else the task's only open
    /// one. `None` means a plain instruction.
    pub fn current_decision(&self) -> Option<&Decision> {
        if self.view == View::Decisions {
            return self.visible_decisions().get(self.dsel).map(|r| r.decision);
        }
        // A prompt row is the pane's own question: typing for the task is not its answer.
        let open = self.current()?.open_decisions();
        let mut open = open.filter(|d| !d.prompt_row);
        match (open.next(), open.next()) {
            (Some(d), None) => Some(d),
            _ => None,
        }
    }

    fn start_input(&mut self, prefill: String) {
        self.start_input_as(prefill, false)
    }

    /// `ask`: the ask key, which records the input as a question about the decision.
    fn start_input_as(&mut self, prefill: String, ask: bool) {
        let Some(t) = self.current() else {
            self.status = "no task selected".into();
            return;
        };
        let cur = self.current_decision();
        if cur.is_some_and(|d| d.pane_only) {
            self.status = PANE_ONLY.into();
            return;
        }
        // A legacy note goes by its key; a prompt row is no decision of the task's.
        let named = cur.filter(|d| !d.prompt_row);
        let tracked = named.is_some_and(|d| d.id.is_some());
        let did = named.map(|d| d.id.clone().unwrap_or_else(|| d.key.clone()));
        if ask && !tracked {
            self.status = "no tracker decision to ask about".into();
            return;
        }
        let target = InputTarget::Task {
            task: t.task_ref(),
            decision: did.clone(),
            ask,
        };
        let route = match self.api.query(Query::Task(t.task_ref())) {
            Ok(Answer::Task(d)) => d.route,
            _ => Default::default(),
        };
        let pane = route.pane.unwrap_or_default();
        let what = match (&did, ask) {
            (Some(d), true) => format!("{} {d} ask", t.id),
            (Some(d), false) => format!("{} {d}", t.id),
            _ => t.id.clone(),
        };
        let label = match route.kind {
            RouteKind::Session => format!("{what} → session {pane}"),
            RouteKind::Orchestrator => format!("{what} → orchestrator {pane}"),
            _ if tracked => format!("{what} → tracker only"),
            _ => format!("{what} → note only"),
        };
        self.input = Some(Input {
            buf: prefill,
            target,
            label,
        });
    }

    fn send(&mut self, target: InputTarget, text: String) {
        match target {
            InputTarget::Task {
                task,
                decision,
                ask,
            } => {
                self.status = "sending…".into();
                let v = Verb::Answer {
                    task,
                    text,
                    decision,
                    ask,
                };
                self.run_cmd(v, After::Say("send failed: "));
            }
            InputTarget::Pane { session } => {
                self.status = "sending…".into();
                self.run_cmd(Verb::Send { session, text }, After::Say("send failed: "));
            }
            InputTarget::Describe { session } => self.run_action(Verb::Describe { session, text }),
        }
    }

    fn toggle_pin(&mut self, epic: bool) {
        let Some(t) = self.current() else { return };
        let (target, pinned) = match epic {
            true => (PinTarget::Epic, !t.pins.epic),
            false => (PinTarget::Ticket, !t.pins.ticket),
        };
        let task = t.task_ref();
        let v = Verb::Pin {
            task,
            target,
            pinned,
        };
        self.run_cmd(v, After::Say(""));
    }

    fn open_item(&mut self) {
        let Some(it) = self.timeline.get(self.tsel).cloned() else {
            return;
        };
        if let Some(p) = &it.transcript
            && it.source != "worker"
        {
            let at = (it.source == "unattributed")
                .then(|| it.ts.clone())
                .flatten();
            self.open_transcript(p.clone(), it.title.clone(), at);
            return;
        }
        let mut entries = vec![Entry {
            kind: EntryKind::Assistant,
            ts: it.ts.clone(),
            head: String::new(),
            body: it.body.clone(),
            ..Default::default()
        }];
        if let Some(p) = &it.transcript {
            entries.push(Entry {
                kind: EntryKind::Tool,
                ts: None,
                head: format!("transcript: {p} (press t to open)"),
                body: p.clone(),
                ..Default::default()
            });
        }
        self.viewer = Some(Viewer::new(
            format!("{} — {}", it.source, it.title),
            entries,
            it.transcript.clone(),
        ));
        self.go(View::Viewer);
    }

    /// Open a transcript at its end, or at the first entry from `around` on.
    fn open_transcript(&mut self, reference: String, title: String, around: Option<String>) {
        let t = match self.api.query(Query::Transcript { reference, around }) {
            Ok(Answer::Transcript(t)) => t,
            Ok(_) => return,
            Err(e) => {
                self.status = format!("transcript: {e}");
                return;
            }
        };
        if t.skipped_bytes > 0 {
            self.status = format!("showing the tail; skipped {} MB", t.skipped_bytes >> 20);
        }
        let mut v = Viewer::new(title, t.entries, None);
        v.cursor = usize::MAX;
        v.start = t.at;
        // Opened from another viewer: Esc goes back to that one.
        if let Some(u) = self.viewer.replace(v)
            && self.view == View::Viewer
        {
            self.under.push(u);
        }
        self.go(View::Viewer);
    }

    fn log(&mut self, session: &str) -> Option<api::Log> {
        let q = Query::Log {
            session: session.into(),
        };
        match self.api.query(q) {
            Ok(Answer::Log(l)) => Some(l),
            Ok(_) => None,
            Err(e) => {
                self.status = format!("session log: {e}");
                None
            }
        }
    }

    fn open_log(&mut self, s: &Session) {
        let Some(l) = self.log(&s.id) else { return };
        let entries = vec![Entry {
            body: l.text,
            ..Default::default()
        }];
        let mut v = Viewer::new(format!("session log {}", s.id), entries, None);
        v.cursor = usize::MAX;
        self.viewer = Some(v);
        self.go(View::Viewer);
    }

    /// The live or closed session list as shown: narrowed by the Sessions filter.
    pub fn sessions_sorted(&self) -> Vec<&Session> {
        self.scoped_sessions()
            .into_iter()
            .filter(|s| {
                let task = self.task_for_session(s).map(|t| t.id.as_str());
                self.sfilter.matches(&session_text(s, task))
            })
            .collect()
    }

    /// The scope's live or closed sessions, grouped by context in all-contexts mode.
    fn scoped_sessions(&self) -> Vec<&Session> {
        let all = if self.show_closed {
            &self.closed
        } else {
            &self.snap.sessions
        };
        self.arrange(all.iter().collect(), |s| s.context.as_deref())
    }

    pub fn session_total(&self) -> usize {
        self.scoped_sessions().len()
    }

    /// The scope's live sessions (the Sessions tab count).
    pub fn live_sessions(&self) -> usize {
        self.snap
            .sessions
            .iter()
            .filter(|s| self.in_scope(s.context.as_deref()))
            .count()
    }

    pub fn task_for_session(&self, s: &Session) -> Option<&Task> {
        self.snap.task(s.task.as_ref()?)
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
            let mut verb = c.verb;
            if let Verb::Close { keep_pane, .. } = &mut verb {
                *keep_pane = !yes;
            }
            let close = matches!(verb, Verb::Close { .. });
            if yes || (close && k.code == KeyCode::Char('k')) {
                self.status = match c.after {
                    After::Session => "working…",
                    _ => "sending…",
                }
                .into();
                let cmd = Command {
                    verb,
                    confirm: Some(c.token),
                    request_id: None,
                };
                self.run_cmd(cmd, c.after);
            } else {
                self.status = "cancelled".into();
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
        // Before the global keys, so `M-d` picks a row rather than switching views.
        if self.view == View::Sessions
            && let Some(i) = index_of_key(&k)
        {
            self.pick_session(i);
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
            KeyCode::Char('H') if self.view != View::Viewer => {
                self.go(View::Health);
                return;
            }
            KeyCode::Char('t') if self.view != View::Viewer => {
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
            KeyCode::Char('M') if self.scoped_view() => {
                self.toggle_mode();
                return;
            }
            KeyCode::Char('u') if matches!(self.view, View::Tasks | View::Detail) => {
                self.toggle_everyone();
                return;
            }
            KeyCode::Char('C') if self.scoped_view() => {
                self.open_picker();
                return;
            }
            KeyCode::Char(c @ ('[' | ']')) if self.scoped_view() => {
                self.step_context(c == ']');
                return;
            }
            KeyCode::Char('R') => {
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
            View::Contexts => self.keys_contexts(code),
            View::Help | View::Health => {}
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
                    self.send(inp.target, inp.buf);
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
                    let id = t.id.clone();
                    self.open_detail(id);
                }
            }
            KeyCode::Char('r') => {
                self.reload(false);
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
            KeyCode::Char('r') => self.refresh_detail(true),
            KeyCode::Char('a') => self.start_input(String::new()),
            KeyCode::Char('A') => self.resend(),
            KeyCode::Char('v') => self.load_conversation(),
            KeyCode::Char('Q') => self.start_input_as(String::new(), true),
            KeyCode::Char('*') => self.toggle_pin(false),
            KeyCode::Char('+') => self.toggle_pin(true),
            KeyCode::Char(c) if c.is_ascii_digit() => self.pick_option(c),
            KeyCode::Char(c) => {
                self.link_key(c);
            }
            _ => {}
        }
    }

    /// Re-read the detail's task with its registry entries; `force` also bypasses ticket and PR
    /// caches (`r`). Opening a detail does it unforced, once per task per snapshot.
    fn refresh_detail(&mut self, force: bool) {
        let Some(task) = self.current().map(Task::task_ref) else {
            return;
        };
        self.registry_read.insert(task.id.clone());
        if force {
            self.status = format!("refreshing {}…", task.id);
        }
        let v = Verb::Refresh {
            force,
            task: Some(task),
        };
        self.run_cmd(v, After::Detail);
    }

    /// Reload everything; `force` also re-reads the context list.
    fn reload(&mut self, force: bool) {
        self.status = "reloading…".into();
        let r = self.api.command(Verb::Refresh { force, task: None }.into());
        if let Err(e) = r {
            self.status = e.to_string();
        }
    }

    fn jump_to(&mut self, session: String) {
        self.status = match self.api.query(Query::Pane { session }) {
            Ok(Answer::Pane(p)) => self.desk.jump(&p).map_err(|e| e.to_string()),
            Ok(_) => return,
            Err(e) => Err(e.to_string()),
        }
        .unwrap_or_else(|e| format!("jump: {e}"));
    }

    fn pick_option(&mut self, c: char) {
        let n = c.to_digit(10).unwrap_or(0) as usize;
        let Some(d) = self
            .current_decision()
            .or_else(|| self.current().and_then(|t| t.decisions.first()))
            .cloned()
        else {
            return;
        };
        if d.pane_only {
            self.status = PANE_ONLY.into();
            return;
        }
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
            KeyCode::Char('Q') => self.start_input_as(String::new(), true),
            KeyCode::Char(c) if c.is_ascii_digit() => self.pick_option(c),
            KeyCode::Enter => {
                let ds = self.visible_decisions();
                let Some(r) = ds.get(self.dsel) else {
                    return;
                };
                let id = r.task.id.clone();
                let prompt = r.decision.prompt.is_some();
                match r.task.session_id.clone().filter(|_| prompt) {
                    Some(s) => self.jump_to(s),
                    None => self.open_detail(id),
                }
            }
            KeyCode::Char(c) => {
                self.link_key(c);
            }
            _ => {}
        }
    }

    /// An index key: select that row and act as `Enter` would on it.
    fn pick_session(&mut self, i: usize) {
        if i >= self.sessions_sorted().len() {
            self.status = format!("no session {}", index_label(i));
            return;
        }
        self.ssel = i;
        self.keys_sessions(KeyCode::Enter);
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
                self.load_closed();
                self.ssel = 0;
            }
            KeyCode::Char('e') => {
                if let Some(s) = cur {
                    self.input = Some(Input {
                        buf: s.description.clone().unwrap_or_default(),
                        label: format!("describe {}", &s.id[..s.id.len().min(8)]),
                        target: InputTarget::Describe { session: s.id },
                    });
                }
            }
            KeyCode::Char('x') => {
                if let Some(s) = cur.filter(|s| !s.closed) {
                    self.run_action(Verb::Close {
                        session: s.id,
                        keep_pane: false,
                    });
                }
            }
            KeyCode::Char('S') => self.run_action(Verb::SnapshotSave),
            KeyCode::Char('L') => self.run_action(Verb::SnapshotRestore),
            KeyCode::Enter if self.show_closed => {
                if let Some(s) = cur {
                    self.run_action(Verb::Reopen { session: s.id });
                }
            }
            KeyCode::Enter => {
                if let Some(s) = cur {
                    self.jump_to(s.id);
                }
            }
            KeyCode::Char('r') => {
                if let Some(s) = cur {
                    self.input = Some(Input {
                        buf: String::new(),
                        label: format!("reply → {}", s.pane),
                        target: InputTarget::Pane { session: s.id },
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
                    let Some(l) = self.log(&s.id) else { return };
                    match l.transcript {
                        Some(r) => self.open_transcript(r, format!("session {}", s.id), None),
                        None => self.status = "no transcript found".into(),
                    }
                }
            }
            KeyCode::Char('o') => {
                if let Some(id) = cur
                    .as_ref()
                    .and_then(|s| self.task_for_session(s))
                    .map(|t| t.id.clone())
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
                let title = v.title.clone();
                if let Some(l) = v.entry().and_then(|e| e.link.clone()) {
                    self.open_transcript(l.transcript, title, l.ts);
                } else if let Some(p) = v.note.clone() {
                    self.open_transcript(p, title, None);
                } else {
                    self.status = "no transcript behind this entry".into();
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

/// The task's own live pane, or why there is none.
fn task_pane(api: &dyn Api, t: &TaskRef) -> Result<api::Pane, String> {
    let none = || "no live session".to_string();
    let Ok(Answer::Task(d)) = api.query(Query::Task(t.clone())) else {
        return Err(none());
    };
    let session = d
        .route
        .session_id
        .filter(|_| d.route.kind == RouteKind::Session)
        .ok_or_else(none)?;
    match api.query(Query::Pane { session }) {
        Ok(Answer::Pane(p)) => Ok(p),
        Ok(_) => Err(none()),
        Err(e) => Err(e.to_string()),
    }
}

/// How long the TUI waits for a key before it looks for new state.
const POLL: Duration = Duration::from_millis(150);
/// The longest an unchanged screen goes undrawn: ages on it count seconds.
const REDRAW: Duration = Duration::from_secs(1);

pub fn run(
    api: Arc<dyn Api>,
    desk: Desk,
    context: Option<String>,
    serving: Option<Result<String, String>>,
) -> Result<()> {
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = channel();
    let mut app = App::new(api, Arc::new(desk), tx);
    app.serving = serving;
    if let Some(c) = context {
        app.set_scope(Scope::One(c));
    }
    let mut term = ratatui::init();
    let res = (|| -> Result<()> {
        let mut stale = true;
        let mut drawn = Instant::now();
        while !app.quit {
            stale |= app.sync();
            if stale || drawn.elapsed() >= REDRAW {
                term.draw(|f| crate::ui::draw(f, &mut app))?;
                (stale, drawn) = (false, Instant::now());
            }
            if event::poll(POLL)? {
                stale = true;
                match event::read()? {
                    Event::Key(k) => app.on_key(k),
                    Event::Mouse(m) => app.on_mouse(m.kind),
                    _ => {}
                }
            }
            while let Ok(m) = rx.try_recv() {
                stale = true;
                app.on_msg(m);
            }
        }
        Ok(())
    })();
    if app.mouse {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    }
    ratatui::restore();
    res
}

/// A Decisions view row: one decision and the task posing it.
#[derive(Clone, Copy)]
pub struct DecisionRow<'a> {
    pub task: &'a Task,
    pub decision: &'a Decision,
}

impl DecisionRow<'_> {
    /// Stable across reloads: task id and decision key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.task.id, self.decision.key)
    }
}

/// The ids under the task, decision and session cursors.
#[derive(Clone)]
struct Picks {
    task: Option<String>,
    decision: Option<String>,
    session: Option<String>,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::core::Snapshot;
    use crate::model;
    use std::collections::HashMap;
    use std::ops::{Deref, DerefMut};
    use std::sync::Mutex;
    use tokio::sync::broadcast;

    /// Serves a scripted state, records commands, and asks before the destructive ones.
    #[derive(Default)]
    pub struct FakeApi {
        state: Mutex<Arc<State>>,
        commands: Mutex<Vec<Command>>,
        /// Pane id by session.
        panes: Mutex<HashMap<String, String>>,
        closed: Mutex<Vec<Session>>,
        transcript: Mutex<api::Transcript>,
    }

    impl Api for FakeApi {
        fn state(&self) -> Arc<State> {
            self.state.lock().unwrap().clone()
        }

        fn query(&self, q: Query) -> Result<Answer, Error> {
            Ok(match q {
                Query::Server => Answer::Server(Default::default()),
                Query::Sessions { .. } => Answer::Sessions(api::Sessions {
                    sessions: self.closed.lock().unwrap().clone(),
                }),
                Query::Transcript { .. } => {
                    Answer::Transcript(self.transcript.lock().unwrap().clone())
                }
                Query::Pane { session } => match self.panes.lock().unwrap().get(&session) {
                    Some(id) => Answer::Pane(api::Pane {
                        id: id.clone(),
                        label: id.clone(),
                    }),
                    None => return Err(Error::not_found("pane")),
                },
                q => return Err(Error::not_found(format!("{q:?}"))),
            })
        }

        fn command(&self, c: Command) -> Result<Reply, Error> {
            self.commands.lock().unwrap().push(c.clone());
            let asks = matches!(
                c.verb,
                Verb::Close { .. } | Verb::Reopen { .. } | Verb::SnapshotRestore
            );
            if asks && c.confirm.is_none() {
                let mut e = Error::new(Code::NeedsConfirm, "really?");
                e.confirm = Some("tok".into());
                return Err(e);
            }
            Ok(Reply::new("done"))
        }

        fn subscribe(&self) -> broadcast::Receiver<api::Event> {
            broadcast::channel(1).1
        }
    }

    /// An app over the fakes. `m` is the snapshot that `load` projects and serves.
    pub struct Fix {
        pub app: App,
        pub m: Snapshot,
        pub health: crate::health::Health,
        pub api: Arc<FakeApi>,
        calls: crate::desk::tests::Calls,
        rx: Receiver<Msg>,
    }

    impl Deref for Fix {
        type Target = App;
        fn deref(&self) -> &App {
            &self.app
        }
    }

    impl DerefMut for Fix {
        fn deref_mut(&mut self) -> &mut App {
            &mut self.app
        }
    }

    impl Fix {
        fn serve(&mut self, phase: Phase) {
            let rev = self.api.state().rev + 1;
            let mut st = crate::project::state(&crate::project::Parts {
                snap: &self.m,
                deliveries: &[],
                health: &self.health.rows(),
                panes: &HashMap::new(),
                phase,
                loading: false,
                notices: vec![],
                now: chrono::Utc::now(),
            });
            st.rev = rev;
            st.instance = "i".into();
            if phase == Phase::Loaded {
                st.loaded_at = Some(format!("load {rev}"));
            }
            *self.api.state.lock().unwrap() = Arc::new(st);
            self.app.sync();
        }

        pub fn load(&mut self) {
            self.serve(Phase::Loaded);
        }

        fn load_snap(&mut self, s: Snapshot) {
            self.m = s;
            self.load();
        }

        fn close(&mut self, sessions: Vec<Session>) {
            *self.api.closed.lock().unwrap() = sessions;
            self.app.show_closed = true;
            self.app.load_closed();
        }

        fn pane(&self, session: &str, id: &str) {
            let mut p = self.api.panes.lock().unwrap();
            p.insert(session.into(), id.into());
        }

        fn confirmed(&self) -> Vec<Verb> {
            let c = self.api.commands.lock().unwrap();
            c.iter()
                .filter(|c| c.confirm.is_some())
                .map(|c| c.verb.clone())
                .collect()
        }

        /// Wait for one background command to land.
        fn land(&mut self) {
            let m = self.rx.recv_timeout(Duration::from_secs(5)).unwrap();
            self.app.on_msg(m);
        }
    }

    pub fn fix() -> Fix {
        let (tx, rx) = channel();
        let api = Arc::new(FakeApi::default());
        let (desk, calls) = crate::desk::tests::fake(&[], true);
        Fix {
            app: App::new(api.clone(), Arc::new(desk), tx),
            m: Snapshot::default(),
            health: Default::default(),
            api,
            calls,
            rx,
        }
    }

    pub fn ctx(n: &str) -> crate::config::Context {
        crate::config::Context {
            name: n.into(),
            workspace: format!("/ws/{n}").into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: String::new(),
            adapter: None,
            adapter_error: None,
        }
    }

    fn entries() -> Vec<Entry> {
        let e = |kind, head: &str, body: &str| Entry {
            kind,
            ts: None,
            head: head.into(),
            body: body.into(),
            ..Default::default()
        };
        vec![
            e(EntryKind::User, "", "hello world"),
            e(
                EntryKind::Tool,
                "Bash: ls",
                "{\"command\": \"secret-needle\"}",
            ),
            e(EntryKind::Assistant, "", "line1\nline2"),
        ]
    }

    #[test]
    fn conversation_opens_transcripts_and_comes_back() {
        let mut app = fix();
        let at = |m: &str| Entry {
            kind: EntryKind::User,
            body: format!("at {m}"),
            ..Default::default()
        };
        *app.api.transcript.lock().unwrap() = api::Transcript {
            entries: vec![at("00"), at("05"), at("10")],
            at: Some(1),
            ..Default::default()
        };
        app.m.tasks = vec![crate::core::tests::jt("ctx-1")];
        app.load();
        let e = Entry {
            body: "hi".into(),
            label: Some("◀ orchestrator".into()),
            link: Some(api::Link {
                transcript: "t".into(),
                ts: Some("2026-11-23T14:05:00Z".into()),
            }),
            ..Default::default()
        };
        app.on_msg(Msg::Conversation("ctx-1".into(), vec![e.clone()]));
        assert!(app.viewer.is_none(), "only over that task's detail");
        app.view = View::Detail;
        app.detail_id = Some("ctx-1".into());
        app.on_msg(Msg::Conversation("ctx-1".into(), vec![e]));
        assert!(app.view == View::Viewer);
        app.viewer.as_mut().unwrap().rebuild(80);
        assert!(
            app.viewer.as_ref().unwrap().lines[0]
                .0
                .starts_with("◀ orchestrator")
        );
        let press = |app: &mut App, c| app.on_key(KeyEvent::new(c, KeyModifiers::NONE));
        press(&mut app, KeyCode::Char('t'));
        let v = app.viewer.as_mut().unwrap();
        v.rebuild(80);
        assert_eq!(v.entry().unwrap().body, "at 05");
        press(&mut app, KeyCode::Esc);
        assert!(app.view == View::Viewer);
        assert_eq!(app.viewer.as_ref().unwrap().title, "conversation ctx-1");
        press(&mut app, KeyCode::Esc);
        assert!(app.view == View::Detail && app.viewer.is_none());
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
                t.decisions = vec![Default::default()];
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
            app.visible_tasks()[app.sel].id.clone(),
            app.visible_decisions()[app.dsel].task.id.clone(),
        )
    }

    #[test]
    fn the_status_says_how_fresh_the_tasks_are() {
        let mut app = fix();
        app.m = snap_of(&["a", "b"]);
        app.serve(Phase::Preview);
        assert_eq!(app.visible_tasks().len(), 2);
        assert_eq!(app.status, "2 tasks (cached) · loading…");
        app.m.contexts = vec![ctx("c")];
        app.m.tracker_stale.insert(
            "c".into(),
            model::Stale {
                as_of: "2026-11-21T14:02:00Z".into(),
                error: "timed out".into(),
            },
        );
        app.load();
        assert!(
            app.status.contains("refresh failed: timed out"),
            "{}",
            app.status
        );
        app.m.errors = vec!["boom".into()];
        app.load();
        assert_eq!(app.status, "2 tasks · 1 error(s): boom");
        // An unchanged state leaves the status alone.
        app.status = "mine".into();
        app.sync();
        assert_eq!(app.status, "mine");
        app.app.scope = Scope::One("gone".into());
        app.load();
        assert_eq!(app.status, "no context named gone — C to pick one");
    }

    #[test]
    fn notices_show_once() {
        let mut app = fix();
        let mut st = State {
            rev: 1,
            notices: vec![api::Notice {
                seq: 1,
                at: String::new(),
                text: "ctx-1: not received".into(),
            }],
            ..Default::default()
        };
        *app.api.state.lock().unwrap() = Arc::new(st.clone());
        app.sync();
        assert_eq!(app.status, "ctx-1: not received");
        app.status.clear();
        st.rev = 2;
        *app.api.state.lock().unwrap() = Arc::new(st);
        app.sync();
        assert_eq!(app.status, "");
    }

    #[test]
    fn selection_follows_its_item_across_a_reload() {
        let mut app = fix();
        app.load_snap(snap_of(&["a", "b", "c"]));
        (app.sel, app.dsel) = (1, 1);
        // "a" was decided and dropped off: the cursor stays on "b", now one row up.
        app.load_snap(snap_of(&["b", "c"]));
        assert_eq!(picked(&app), ("b".into(), "b".into()));
        // Reordered: still "b".
        app.load_snap(snap_of(&["c", "d", "b"]));
        assert_eq!(picked(&app), ("b".into(), "b".into()));
        // "b" itself left: the cursor keeps its row, clamped to the shorter list.
        app.load_snap(snap_of(&["c", "d"]));
        assert_eq!(picked(&app), ("d".into(), "d".into()));
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn wide_detail_walks_the_filtered_list() {
        let mut app = fix();
        app.load_snap(titled(&[("a-1", "keep"), ("b-2", "drop"), ("c-3", "keep")]));
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
        let mut app = fix();
        app.load_snap(snap_of(&["a", "b", "c"]));
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
        let mut app = fix();
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
        let mut app = fix();
        app.load_snap(snap_of(&["a", "b"]));
        app.width = 160;
        app.view = View::Decisions;
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Char('j'));
        assert_eq!((app.dsel, app.sel), (1, 0));
        assert_eq!(app.detail_id.as_deref(), Some("b"));
        // The open item leaves the list: the detail follows the clamped cursor.
        app.load_snap(snap_of(&["a"]));
        assert_eq!(app.detail_id.as_deref(), Some("a"));
    }

    #[test]
    fn destructive_session_actions_need_confirmation() {
        let mut app = fix();
        app.m.sessions = vec![model::Session {
            session_id: "s-1".into(),
            tmux_target: Some("nowhere:9.9".into()),
            worktree: "/nonexistent".into(),
            ..Default::default()
        }];
        app.load();
        app.view = View::Sessions;
        app.on_key(key('x'));
        assert!(matches!(asked(&app), Some(Verb::Close { .. })));
        assert!(
            app.status.contains("k = kill claude only"),
            "{}",
            app.status
        );
        app.on_key(key('n'));
        assert!(app.confirm.is_none());
        assert_eq!(app.status, "cancelled");
        assert!(app.confirmed().is_empty(), "nothing ran");
        app.on_key(key('L'));
        assert!(matches!(asked(&app), Some(Verb::SnapshotRestore)));
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.confirm.is_none());
        assert!(app.confirmed().is_empty(), "nothing ran");
        // Closed list: Enter asks before reopening.
        app.close(vec![Session {
            id: "s-2".into(),
            closed: true,
            ..Default::default()
        }]);
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(asked(&app), Some(Verb::Reopen { .. })));
    }

    fn asked(app: &App) -> Option<&Verb> {
        app.confirm.as_ref().map(|c| &c.verb)
    }

    #[test]
    fn a_confirmed_close_repeats_the_command_with_its_token() {
        let mut app = fix();
        app.m.sessions = vec![model::Session {
            session_id: "s-1".into(),
            ..Default::default()
        }];
        app.load();
        app.view = View::Sessions;
        let close = |keep_pane| Verb::Close {
            session: "s-1".into(),
            keep_pane,
        };
        for (k, keep) in [('y', false), ('k', true)] {
            app.on_key(key('x'));
            app.on_key(key(k));
            app.land();
            assert_eq!(app.status, "done");
            assert!(app.confirm.is_none());
            assert_eq!(app.confirmed().last(), Some(&close(keep)), "{k}");
        }
        let c = app.api.commands.lock().unwrap();
        assert_eq!(c.last().unwrap().confirm.as_deref(), Some("tok"));
    }

    #[test]
    fn k_confirms_nothing_but_a_close() {
        let mut app = fix();
        app.view = View::Sessions;
        app.on_key(key('L'));
        app.on_key(key('k'));
        assert_eq!(app.status, "cancelled");
        assert!(app.confirmed().is_empty());
    }

    #[test]
    fn pins_say_the_state_they_want() {
        let mut app = fix();
        let mut t = crate::core::tests::jt("a");
        t.ticket_key = Some("T-1".into());
        app.m.tasks = vec![t];
        app.load();
        app.on_key(key('*'));
        app.land();
        let want = |pinned| Verb::Pin {
            task: app.snap.tasks[0].task_ref(),
            target: PinTarget::Ticket,
            pinned,
        };
        assert_eq!(app.api.commands.lock().unwrap()[0].verb, want(true));
        let mut st = (*app.api.state()).clone();
        st.rev += 1;
        st.tasks[0].pins.ticket = true;
        let off = want(false);
        *app.api.state.lock().unwrap() = Arc::new(st);
        app.sync();
        app.on_key(key('*'));
        app.land();
        assert_eq!(app.api.commands.lock().unwrap()[1].verb, off);
    }

    /// What the TUI may name of the crate: the API, and nothing behind it.
    #[test]
    fn the_tui_names_no_engine_module() {
        let behind = [
            "core",
            "engine",
            "routing",
            "actions",
            "delivery",
            "store",
            "sessions",
            "history",
            "conversation",
            "config",
            "prompts",
            "baton",
            "adapter",
            "pr",
            "review",
            "notes",
            "worktrees",
            "panes",
            "hook",
            "turns",
            "model",
            "project",
            "transcript",
            "cache",
        ];
        for (file, text) in [
            ("app.rs", include_str!("app.rs")),
            ("ui.rs", include_str!("ui.rs")),
            ("theme.rs", include_str!("theme.rs")),
        ] {
            let code = text.split("#[cfg(test)]").next().unwrap();
            for m in behind {
                let named = format!("crate::{m}");
                let hit = code.match_indices(&named).any(|(i, _)| {
                    !code[i + named.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
                });
                assert!(!hit, "{file} names {named}");
            }
            assert!(!code.contains("use crate::{"), "{file}: one module per use");
        }
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    #[test]
    fn index_keys_map_both_ways() {
        let labels: Vec<_> = (0..INDEX_KEYS + 1).map(index_label).collect();
        assert_eq!(&labels[..2], ["0", "1"]);
        assert_eq!(&labels[9..12], ["9", "M-a", "M-b"]);
        assert_eq!(labels[35], "M-z");
        assert_eq!(labels[36], "");
        for i in 0..INDEX_KEYS {
            let k = if i < 10 {
                key(char::from_digit(i as u32, 10).unwrap())
            } else {
                alt((b'a' + (i - 10) as u8) as char)
            };
            assert_eq!(index_of_key(&k), Some(i), "{}", index_label(i));
        }
        // Plain letters, Ctrl-digits and Meta capitals are not index keys.
        assert_eq!(index_of_key(&key('a')), None);
        let ctrl = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL);
        assert_eq!(index_of_key(&ctrl), None);
        assert_eq!(index_of_key(&alt('A')), None);
    }

    #[test]
    fn index_keys_jump_to_the_displayed_row() {
        let mut app = fix();
        let calls = app.calls.clone();
        let s = |id: &str, wt: &str, target: &str, pid: i64| model::Session {
            session_id: id.into(),
            worktree: wt.into(),
            tmux_target: Some(target.into()),
            claude_pid: Some(pid),
            branch: Some(id.into()),
            status: "idle".into(),
            ..Default::default()
        };
        // Twelve rows, so M-a and M-b exist; row 1 and row 11 are the two real panes.
        let mut list: Vec<model::Session> = (0..12)
            .map(|i| s(&format!("x-{i}"), "/nowhere", "gone:9.9", 999))
            .collect();
        list[1] = s("orch", "/ws", "o:1.2", 21);
        list[11] = s("task", "/wt/a", "a:1.1", 11);
        app.m.sessions = list;
        app.load();
        app.pane("orch", "%2");
        app.pane("task", "%1");
        app.view = View::Sessions;
        app.on_key(key('1'));
        assert_eq!(app.ssel, 1);
        app.on_key(alt('b'));
        assert_eq!(app.ssel, 11);
        assert_eq!(app.view, View::Sessions);
        assert_eq!(
            *calls.lock().unwrap(),
            ["switch-client %2", "switch-client %1"]
        );
        // Out of range: says so, moves nothing.
        app.on_key(alt('z'));
        assert_eq!(app.status, "no session M-z");
        assert_eq!(app.ssel, 11);
        // `M-d` is a row here, not the Decisions view.
        app.on_key(alt('d'));
        assert_eq!(app.view, View::Sessions);
        assert_eq!(app.status, "no session M-d");
        // Indices follow the filter: `0` is now the first match.
        app.on_key(key('/'));
        typed(&mut app, "task");
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        calls.lock().unwrap().clear();
        app.on_key(key('0'));
        assert_eq!(*calls.lock().unwrap(), ["switch-client %1"]);
        // Closed list: the key asks before reopening, like Enter.
        app.on_key(esc());
        app.close(vec![Session {
            id: "c-1".into(),
            closed: true,
            ..Default::default()
        }]);
        app.on_key(key('0'));
        assert!(matches!(asked(&app), Some(Verb::Reopen { .. })));
        assert!(app.confirmed().is_empty(), "nothing ran");
    }

    #[test]
    fn digits_keep_their_meaning_outside_sessions() {
        let mut app = fix();
        let calls = app.calls.clone();
        app.m.sessions = vec![model::Session {
            session_id: "orch".into(),
            worktree: "/ws".into(),
            tmux_target: Some("o:1.2".into()),
            claude_pid: Some(21),
            ..Default::default()
        }];
        app.load();
        app.view = View::Decisions;
        app.on_key(key('0'));
        app.on_key(alt('s'));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn prompt_rows_jump_to_their_pane() {
        let mut app = fix();
        let calls = app.calls.clone();
        let mut t = crate::core::tests::jt("a");
        t.session = Some(model::Session {
            session_id: "s".into(),
            worktree: "/wt/a".into(),
            tmux_target: Some("a:1.1".into()),
            claude_pid: Some(11),
            status: "needs_permission".into(),
            prompt: Some(crate::model::Prompt {
                kind: "permission".into(),
                text: "May I?\nBash: make it".into(),
                ..Default::default()
            }),
            ..Default::default()
        });
        crate::prompts::apply(&mut t);
        app.m.sessions = t.session.iter().cloned().collect();
        app.m.tasks = vec![t];
        app.load();
        app.pane("s", "%1");
        app.view = View::Decisions;
        for k in ['a', '1', 'Q'] {
            app.on_key(key(k));
            assert!(
                app.input.is_none() && app.status.contains("its pane"),
                "{k}"
            );
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(*calls.lock().unwrap(), ["switch-client %1"]);
        assert_eq!(app.view, View::Decisions);
    }

    /// Two legacy notes on one task: two rows, and input on either names the one it is on.
    #[test]
    fn legacy_rows_are_answered_by_key() {
        let mut app = fix();
        let mut t = crate::core::tests::jt("a");
        let notes = "DECISION (19:11Z): one?\nDECISION (19:18Z): two?";
        t.decisions = crate::notes::pending_decisions(notes).unwrap();
        let keys: Vec<_> = t.decisions.iter().map(|d| d.key().to_string()).collect();
        app.m.tasks = vec![t];
        app.load();
        app.view = View::Decisions;
        for (row, want) in keys.iter().enumerate() {
            app.dsel = row;
            app.on_key(key('a'));
            let target = app.input.take().unwrap().target;
            assert!(
                matches!(target, InputTarget::Task { decision: Some(k), ask: false, .. } if k == *want)
            );
        }
        app.on_key(key('Q'));
        assert!(app.input.is_none() && app.status.contains("no tracker decision"));
    }

    fn esc() -> KeyEvent {
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn typed(app: &mut App, s: &str) {
        s.chars().for_each(|c| app.on_key(key(c)));
    }

    fn shown(app: &App) -> Vec<String> {
        app.visible_tasks().iter().map(|t| t.id.clone()).collect()
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
        let mut app = fix();
        app.load_snap(titled(&[
            ("a-1", "Fix Login"),
            ("b-2", "docs"),
            ("c-3", "login page"),
        ]));
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
        let mut app = fix();
        app.load_snap(titled(&[
            ("a-1", "alpha"),
            ("b-2", "beta"),
            ("c-3", "gamma"),
        ]));
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
        app.load_snap(titled(&[("b-2", "beta"), ("d-4", "delta")]));
        assert_eq!(app.current().unwrap().id, "b-2");
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
        let mut app = fix();
        app.load_snap(titled(&[("a-1", "x"), ("b-2", "xy"), ("c-3", "xyz")]));
        app.sel = 2;
        app.on_key(key('/'));
        typed(&mut app, "xy");
        assert_eq!(app.current().unwrap().id, "c-3");
        typed(&mut app, "zz");
        assert!(app.current().is_none());
        app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.current().unwrap().id, "c-3", "first row of [c-3]");
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.current().unwrap().id, "c-3");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.current().unwrap().id, "b-2", "Up moves while typing");
        // Locking an empty filter clears it.
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.tfilter.is_on());
    }

    #[test]
    fn sessions_filter_on_branch_and_task() {
        let mut app = fix();
        let s = |id: &str, br: &str| model::Session {
            session_id: id.into(),
            branch: Some(br.into()),
            worktree: format!("/wt/{id}"),
            ..Default::default()
        };
        app.m.sessions = vec![s("s-1", "feature-x"), s("s-2", "bugfix-y")];
        app.load();
        app.view = View::Sessions;
        app.on_key(key('/'));
        typed(&mut app, "bugfix");
        let ids: Vec<_> = app.sessions_sorted().iter().map(|s| s.id.clone()).collect();
        assert_eq!(ids, ["s-2"]);
        // A filter on the Viewer has no meaning: '/' there is its own search.
        app.on_key(esc());
        app.view = View::Viewer;
        app.viewer = Some(Viewer::new("t".into(), entries(), None));
        app.on_key(key('/'));
        assert!(app.viewer.as_ref().unwrap().searching);
        assert!(!app.sfilter.editing);
    }

    /// `A` looks at the latest answer only: an older one is not sent over one still on its way.
    #[test]
    fn resend_takes_only_the_latest_unreceived_answer() {
        let mut app = scoped_app();
        let t = app.snap.tasks[0].clone();
        let d = |id: &str, resendable| Delivery {
            id: id.into(),
            context: t.context.clone(),
            task: t.id.clone(),
            state: DeliveryState::Delivered,
            resendable,
            ..Default::default()
        };
        app.open_detail(t.id.clone());
        app.snap = Arc::new(State {
            deliveries: vec![d("old", true), d("new", false)],
            ..(*app.snap).clone()
        });
        app.resend();
        assert!(app.status.contains("nothing to resend"), "{}", app.status);
        app.snap = Arc::new(State {
            deliveries: vec![d("old", true), d("new", true)],
            ..(*app.snap).clone()
        });
        app.resend();
        assert_eq!(app.status, "sending…");
        // Opening the detail also refreshes it and reads its timeline, on their own threads.
        let sent = |app: &Fix| -> Vec<Verb> {
            let c = app.api.commands.lock().unwrap().clone();
            let verbs = c.into_iter().map(|c| c.verb);
            verbs.filter(|v| matches!(v, Verb::Resend { .. })).collect()
        };
        while sent(&app).is_empty() {
            app.land();
        }
        let want = Verb::Resend {
            delivery: "new".into(),
        };
        assert_eq!(sent(&app), [want]);
    }

    /// Opening a detail reads its registry once per snapshot (from the fake tracker in tests);
    /// moving between tasks in the wide layout does not.
    #[test]
    fn opening_a_detail_reads_its_registry_once_per_snapshot() {
        let mut app = scoped_app();
        let id = app.snap.tasks[0].id.clone();
        app.open_detail(id.clone());
        assert!(app.registry_read.contains(&id));
        app.registry_read.clear();
        app.registry_read.insert("other".into());
        app.follow_list();
        assert!(!app.registry_read.contains(&id), "cursor moves never read");
        app.load();
        assert!(app.registry_read.is_empty(), "a snapshot clears it");
    }

    #[test]
    fn mouse_starts_off() {
        assert!(!fix().mouse);
    }

    #[test]
    fn wrapping() {
        assert_eq!(wrap("abcdefghij", 8), vec!["abcdefgh", "ij"]);
        assert_eq!(wrap("a\n\nb", 8), vec!["a", "", "b"]);
    }

    fn scoped_app() -> Fix {
        let mut app = fix();
        app.m.contexts = vec![ctx("alpha"), ctx("beta")];
        let t = |id: &str, c: &str| {
            let mut t = crate::core::tests::jt(id);
            t.context = c.into();
            t
        };
        app.m.tasks = vec![t("b-1", "beta"), t("a-1", "alpha"), t("b-2", "beta")];
        app.load();
        app
    }

    fn ids(v: Vec<&Task>) -> Vec<&str> {
        v.into_iter().map(|t| t.id.as_str()).collect()
    }

    #[test]
    fn all_contexts_never_interleaves_and_keeps_registry_order() {
        let app = scoped_app();
        assert_eq!(ids(app.visible_tasks()), ["a-1", "b-1", "b-2"]);
    }

    #[test]
    fn mode_and_context_keys() {
        let mut app = scoped_app();
        // M with no context picked yet opens the picker; Enter picks and returns.
        press(&mut app, KeyCode::Char('M'));
        assert_eq!(app.view, View::Contexts);
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Enter);
        assert_eq!(
            (app.view, &app.scope),
            (View::Tasks, &Scope::One("beta".into()))
        );
        assert_eq!(ids(app.visible_tasks()), ["b-1", "b-2"]);
        // ] steps (wrapping); M goes back to all, and again to the last context.
        press(&mut app, KeyCode::Char(']'));
        assert_eq!(app.scope, Scope::One("alpha".into()));
        press(&mut app, KeyCode::Char('M'));
        assert_eq!(app.scope, Scope::All);
        press(&mut app, KeyCode::Char('M'));
        assert_eq!(app.scope, Scope::One("alpha".into()));
        // t, d and s reach their views from anywhere but the viewer, Sessions included.
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.view, View::Tasks);
        // Esc leaves the picker without changing the scope.
        press(&mut app, KeyCode::Char('C'));
        press(&mut app, KeyCode::Char('j'));
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            (app.view, &app.scope),
            (View::Tasks, &Scope::One("alpha".into()))
        );
    }

    #[test]
    fn selection_keeps_its_item_across_a_scope_change() {
        let mut app = scoped_app();
        app.sel = 2; // b-2
        app.set_scope(Scope::One("beta".into()));
        assert_eq!(ids(app.visible_tasks())[app.sel], "b-2");
        app.set_scope(Scope::All);
        assert_eq!(ids(app.visible_tasks())[app.sel], "b-2");
    }

    #[test]
    fn counts_follow_the_scope_and_flag_elsewhere() {
        let mut app = scoped_app();
        app.m.tasks[1].badge = model::Badge::NeedsYou; // a-1
        app.load();
        assert_eq!(app.needs_elsewhere(), 0);
        app.set_scope(Scope::One("beta".into()));
        assert_eq!(app.needs_elsewhere(), 1);
        assert_eq!(app.tasks().len(), 2);
    }

    /// alpha knows you as "Me"; beta cannot say who you are.
    fn owned_app() -> Fix {
        let mut app = scoped_app();
        app.m.me.insert("alpha".into(), "Me".into());
        let t = |id: &str, who: Option<&str>| {
            let mut t = crate::core::tests::jt(id);
            t.context = "alpha".into();
            t.task.assignee = who.map(Into::into);
            t
        };
        let mut asking = t("a-asks", Some("Someone"));
        asking.badge = crate::model::Badge::NeedsYou;
        let mut review = t("a-review", Some("Someone"));
        review.review_waiting = Some("review requested".into());
        app.m.tasks.extend([
            t("a-mine", Some(" me ")),
            t("a-theirs", Some("Someone")),
            asking,
            review,
        ]);
        app.load();
        app
    }

    #[test]
    fn mine_hides_only_other_peoples_tasks_and_counts_them() {
        let mut app = owned_app();
        // Unassigned a-1 stays; beta (unknown) shows everyone; needs-you and reviews always show.
        assert_eq!(
            ids(app.visible_tasks()),
            ["a-1", "a-mine", "a-asks", "a-review", "b-1", "b-2"]
        );
        assert_eq!((app.hidden(None), app.hidden(Some("alpha"))), (1, 1));
        assert_eq!(app.unknown_me(), ["beta"]);
        app.set_scope(Scope::One("alpha".into()));
        assert!(app.unknown_me().is_empty());
        app.set_scope(Scope::One("beta".into()));
        assert_eq!(app.hidden(None), 0, "the hidden count follows the scope");
    }

    #[test]
    fn u_toggles_everyone_and_keeps_the_selection() {
        let mut app = owned_app();
        app.sel = 3; // a-review
        press(&mut app, KeyCode::Char('u'));
        assert!(app.everyone);
        assert_eq!(app.hidden(None), 0);
        assert!(ids(app.visible_tasks()).contains(&"a-theirs"));
        assert_eq!(ids(app.visible_tasks())[app.sel], "a-review");
        press(&mut app, KeyCode::Char('u'));
        assert!(!app.everyone);
        assert_eq!(ids(app.visible_tasks())[app.sel], "a-review");
        assert!(app.status.contains("1 hidden"));
        // Not a key on Sessions.
        press(&mut app, KeyCode::Char('s'));
        press(&mut app, KeyCode::Char('u'));
        assert!(!app.everyone);
    }

    #[test]
    fn sessions_belong_to_their_task_or_workspace_context() {
        let mut app = scoped_app();
        let s = |wt: &str| model::Session {
            session_id: wt.into(),
            worktree: wt.into(),
            ..Default::default()
        };
        app.m.tasks[0].branches = vec![crate::model::Branch {
            worktree: "/wt/b-1".into(),
            ..Default::default()
        }];
        app.m.sessions = vec![s("/elsewhere"), s("/wt/b-1"), s("/ws/alpha")];
        app.m.tasks[0].session = Some(s("/wt/b-1"));
        app.load();
        let ctx: Vec<_> = app
            .sessions_sorted()
            .iter()
            .map(|s| (s.id.clone(), s.context.as_deref()))
            .collect();
        assert_eq!(
            ctx,
            [
                ("/ws/alpha".to_string(), Some("alpha")),
                ("/wt/b-1".to_string(), Some("beta")),
                ("/elsewhere".to_string(), None),
            ]
        );
        app.set_scope(Scope::One("beta".into()));
        assert_eq!(app.sessions_sorted().len(), 1);
    }
}
