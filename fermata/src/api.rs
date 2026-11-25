//! The seam between fermata's engine and its clients: the `Api` trait and every type crossing it.
//! Nothing here knows about tmux, HTTP or ratatui. See "Client and server" in DESIGN.md.
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::broadcast;

/// The version in the URL prefix. Within it, fields are only ever added.
pub const VERSION: &str = "v1";

pub trait Api: Send + Sync {
    /// The whole dashboard, from memory. Never blocks on a source.
    fn state(&self) -> Arc<State>;
    /// May read files, tmux or (`Conversation`) the tracker: call it off a UI thread when slow.
    fn query(&self, q: Query) -> Result<Answer, Error>;
    /// May block on the tracker, tmux or a reload.
    fn command(&self, c: Command) -> Result<Reply, Error>;
    /// What changed, never the change itself; a lagging receiver re-reads everything.
    fn subscribe(&self) -> broadcast::Receiver<Event>;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Server,
    Task(TaskRef),
    Timeline(TaskRef),
    Conversation(TaskRef),
    Sessions {
        closed: bool,
    },
    Log {
        session: String,
    },
    Pane {
        session: String,
    },
    Transcript {
        reference: String,
        around: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Answer {
    Server(ServerInfo),
    Task(Box<TaskDetail>),
    Timeline(Timeline),
    Conversation(Conversation),
    Sessions(Sessions),
    Log(Log),
    Pane(Pane),
    Transcript(Transcript),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Command {
    pub verb: Verb,
    /// The token of an earlier `needs_confirm`, once the user said yes.
    pub confirm: Option<String>,
    /// A repeat of one seen in the last 10 minutes returns the first result.
    pub request_id: Option<String>,
}

impl From<Verb> for Command {
    fn from(verb: Verb) -> Self {
        Command {
            verb,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verb {
    /// Everything, or one task.
    Refresh {
        force: bool,
        task: Option<TaskRef>,
    },
    Answer {
        task: TaskRef,
        text: String,
        decision: Option<String>,
        ask: bool,
    },
    Resend {
        delivery: String,
    },
    /// Sets, does not toggle.
    Pin {
        task: TaskRef,
        target: PinTarget,
        pinned: bool,
    },
    Send {
        session: String,
        text: String,
    },
    Describe {
        session: String,
        text: String,
    },
    Close {
        session: String,
        keep_pane: bool,
    },
    Reopen {
        session: String,
    },
    SnapshotSave,
    SnapshotRestore,
}

impl Default for Verb {
    fn default() -> Self {
        Verb::Refresh {
            force: false,
            task: None,
        }
    }
}

impl Verb {
    /// The verb and its target, without any text: what is logged.
    pub fn target(&self) -> String {
        match self {
            Verb::Refresh { task: Some(t), .. } => format!("refresh {t}"),
            Verb::Refresh { .. } => "refresh".into(),
            Verb::Answer { task, .. } => format!("answer {task}"),
            Verb::Resend { delivery } => format!("resend {delivery}"),
            Verb::Pin { task, .. } => format!("pin {task}"),
            Verb::Send { session, .. } => format!("send {session}"),
            Verb::Describe { session, .. } => format!("describe {session}"),
            Verb::Close { session, .. } => format!("close {session}"),
            Verb::Reopen { session } => format!("reopen {session}"),
            Verb::SnapshotSave => "snapshot save".into(),
            Verb::SnapshotRestore => "snapshot restore".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinTarget {
    Ticket,
    Epic,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Reply {
    /// One line for the user.
    pub summary: String,
    /// The delivery an answer or a resend created, when it was recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
}

impl Reply {
    pub fn new(summary: impl Into<String>) -> Self {
        Reply {
            summary: summary.into(),
            delivery: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    BadRequest,
    Unauthorized,
    Forbidden,
    ReadOnly,
    NotFound,
    NeedsConfirm,
    Gone,
    SourceFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Error {
    pub code: Code,
    pub message: String,
    /// `needs_confirm` only: repeat the command with this once the user said yes to `message`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

impl Error {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        Error {
            code,
            message: message.into(),
            confirm: None,
        }
    }
    pub fn not_found(what: impl std::fmt::Display) -> Self {
        Error::new(Code::NotFound, format!("no {what}"))
    }
    pub fn source(e: impl std::fmt::Display) -> Self {
        Error::new(Code::SourceFailed, e.to_string())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Section {
    Status,
    Contexts,
    Tasks,
    Decisions,
    Sessions,
    Deliveries,
    Health,
    Notices,
}

impl Section {
    pub const ALL: [Section; 8] = [
        Section::Status,
        Section::Contexts,
        Section::Tasks,
        Section::Decisions,
        Section::Sessions,
        Section::Deliveries,
        Section::Health,
        Section::Notices,
    ];
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Event {
    pub instance: String,
    pub rev: u64,
    pub sections: Vec<Section>,
}

/// A task's identity: ids are unique only within a context's tracker.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskRef {
    pub context: String,
    pub id: String,
}

impl TaskRef {
    pub fn new(context: impl Into<String>, id: impl Into<String>) -> Self {
        TaskRef {
            context: context.into(),
            id: id.into(),
        }
    }
}

impl std::fmt::Display for TaskRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.context, self.id)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    /// Nothing read yet.
    #[default]
    Empty,
    /// The cached first paint.
    Preview,
    Loaded,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct State {
    pub instance: String,
    pub rev: u64,
    pub phase: Phase,
    pub loading: bool,
    /// When the last full load finished; null before the first.
    pub loaded_at: Option<String>,
    pub errors: Vec<String>,
    pub contexts: Vec<Context>,
    /// In the engine's urgency order across contexts.
    pub tasks: Vec<Task>,
    /// The Decisions view's rows, in order.
    pub decisions: Vec<DecisionRef>,
    pub sessions: Vec<Session>,
    /// Oldest first.
    pub deliveries: Vec<Delivery>,
    pub health: Vec<Source>,
    /// The latest things the engine did or noticed, oldest first.
    pub notices: Vec<Notice>,
}

impl State {
    pub fn task(&self, r: &TaskRef) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|t| t.id == r.id && t.context == r.context)
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// The sections that differ from `o`.
    pub fn changed(&self, o: &State) -> Vec<Section> {
        let mut v = vec![];
        let mut add = |s, differs: bool| {
            if differs {
                v.push(s)
            }
        };
        add(
            Section::Status,
            (self.phase, self.loading, &self.loaded_at, &self.errors)
                != (o.phase, o.loading, &o.loaded_at, &o.errors),
        );
        add(Section::Contexts, self.contexts != o.contexts);
        add(Section::Tasks, self.tasks != o.tasks);
        add(Section::Decisions, self.decisions != o.decisions);
        add(Section::Sessions, self.sessions != o.sessions);
        add(Section::Deliveries, self.deliveries != o.deliveries);
        add(Section::Health, self.health != o.health);
        add(Section::Notices, self.notices != o.notices);
        v
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Context {
    /// Empty in the last entry, which stands for sessions belonging to no context.
    pub name: String,
    /// Who you are in its tracker; null when unknown, and then every task counts as yours.
    pub me: Option<String>,
    /// Its task list is the last good one, served after a failed read.
    pub stale: Option<Stale>,
    pub counts: Counts,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    pub shown: usize,
    pub mine: usize,
    pub needs_you: usize,
    pub open_decisions: usize,
    pub state_unknown: usize,
    /// Active tasks not shown as rows.
    pub backlog: usize,
    pub sessions: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Stale {
    pub as_of: String,
    pub error: String,
}

impl Stale {
    pub fn text(&self) -> String {
        format!(
            "as of {}, refresh failed: {}",
            crate::util::short_ts(&self.as_of),
            self.error
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Badge {
    NeedsYou,
    ReviewRequested,
    ReviewUnknown,
    Overdue,
    CiFailing,
    ReadyToMerge,
    WaitingOnCi,
    StateStale,
    Stalled,
    WorkerRunning,
    Interactive,
    Working,
    InReview,
    Blocked,
    WaitingUntil,
    Paused,
    DoneCleanup,
    Parked,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Task {
    pub context: String,
    pub id: String,
    pub title: String,
    pub assignee: Option<String>,
    pub priority: Option<i64>,
    pub labels: Vec<String>,
    pub parent: Option<String>,
    pub badge: Badge,
    /// The badge in words.
    pub badge_text: String,
    /// Its place in the engine's order within its context, from 0.
    pub rank: usize,
    /// Its place in the context's pins, from 0; null when not pinned.
    pub pin_rank: Option<usize>,
    /// Which of its links are pinned; what a `pin` command would change.
    pub pins: Pins,
    pub mine: bool,
    pub needs_you: bool,
    /// Declared in the tracker; null when none was.
    pub state: Option<Declared>,
    pub kept_open: bool,
    /// What a parked task waits on, as one line.
    pub waiting: Option<String>,
    pub decisions: Vec<Decision>,
    pub session_id: Option<String>,
    pub branches: Vec<Branch>,
    pub workers: Vec<Worker>,
    pub progress: Progress,
    pub ticket_key: Option<String>,
    pub ticket_url: Option<String>,
    pub ticket: Option<Ticket>,
    pub ticket_error: Option<String>,
    pub ticket_stale: Option<Stale>,
    pub pr: Option<Pr>,
    pub pr_error: Option<String>,
    pub pr_stale: Option<Stale>,
    /// Why a review of somebody else's PR waits on you; null when none does.
    pub review_waiting: Option<String>,
    /// Why a review's state could not be read: it may be waiting.
    pub review_unknown: Option<String>,
    pub review_stale: Option<Stale>,
    pub last_change: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Pins {
    pub ticket: bool,
    pub epic: bool,
}

impl Task {
    pub fn task_ref(&self) -> TaskRef {
        TaskRef::new(&self.context, &self.id)
    }

    pub fn worktrees(&self) -> Vec<&str> {
        self.branches
            .iter()
            .map(|b| b.worktree.as_str())
            .filter(|w| !w.is_empty())
            .collect()
    }

    /// Decisions waiting on you (not those you answered).
    pub fn open_decisions(&self) -> impl Iterator<Item = &Decision> {
        self.decisions
            .iter()
            .filter(|d| d.status == DecisionStatus::Open)
    }

    pub fn epic_url(&self) -> Option<String> {
        self.ticket.as_ref()?.epic.as_ref()?.url.clone()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Declared {
    pub state: String,
    pub on: Option<String>,
    pub until: Option<String>,
    pub why: Option<String>,
    pub next: Option<String>,
    pub by: Option<String>,
    pub at: Option<String>,
    /// One line for a row.
    pub text: String,
    /// The badge it would show if nothing outranked it.
    pub badge: Badge,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    #[default]
    Open,
    /// You answered; waiting on the orchestrator to act.
    Answered,
    Resolved,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Decision {
    /// Stable within its task.
    pub key: String,
    /// The tracker's id; null for one answered by its `key` alone.
    pub id: Option<String>,
    pub text: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
    pub status: DecisionStatus,
    pub asked_at: Option<String>,
    pub answer: Option<String>,
    pub answered_at: Option<String>,
    /// Your question about it, while nobody has responded.
    pub pending_question: Option<String>,
    /// Open in the session's pane right now, as this kind of prompt.
    pub prompt: Option<String>,
    /// A session's prompt that no tracker decision stands for.
    pub prompt_row: bool,
    /// Only the pane can answer it: `answer` would not.
    pub pane_only: bool,
    /// A DECISION note on a task without the `needs-decision` label.
    pub label_missing: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DecisionRef {
    pub context: String,
    pub task: String,
    /// The decision's `key`.
    pub decision: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Branch {
    pub repo: String,
    pub branch: String,
    pub worktree: String,
    pub status: String,
    pub pr: Option<String>,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Worker {
    pub description: String,
    pub finished: bool,
    pub modified: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Progress {
    pub status_line: Option<String>,
    /// `[done, total]`.
    pub checklist: Option<(usize, usize)>,
    pub done: Vec<String>,
    pub next: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Ticket {
    pub key: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub url: Option<String>,
    pub epic: Option<Epic>,
    pub extra_links: Vec<Labelled>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Epic {
    pub key: String,
    pub title: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Labelled {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Pr {
    pub number: String,
    pub url: Option<String>,
    pub state: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub checks: Checks,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Checks {
    pub pass: usize,
    pub fail: usize,
    pub pending: usize,
    pub failing_url: Option<String>,
    pub latest_url: Option<String>,
    /// The counts in short form.
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Session {
    pub id: String,
    /// Its task's context, else the one its worktree belongs to; null for neither.
    pub context: Option<String>,
    pub task: Option<TaskRef>,
    /// Runs in its context's workspace.
    pub orchestrator: bool,
    pub worktree: String,
    pub branch: Option<String>,
    pub status: String,
    pub status_message: Option<String>,
    pub needs_input: bool,
    /// The dialog open in its pane: an answer would be held until it closes.
    pub dialog: Option<String>,
    pub description: Option<String>,
    pub last_activity: Option<String>,
    pub tool_calls: Option<u64>,
    /// Where it was recorded as running; may be out of date.
    pub tmux: String,
    /// Its pane now, for display; `GET /sessions/{id}/pane` resolves it exactly.
    pub pane: String,
    pub closed: bool,
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    #[default]
    Delivered,
    Queued,
    Held,
    NoteOnly,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Delivery {
    pub id: String,
    pub context: String,
    pub task: String,
    pub text: String,
    /// `session`, `orchestrator` or `note`.
    pub route: String,
    pub session_id: Option<String>,
    pub pane: String,
    pub state: DeliveryState,
    pub detail: String,
    pub created_at: String,
    pub sent_at: Option<String>,
    pub received_at: Option<String>,
    pub received_via: Option<String>,
    pub superseded_by: Option<String>,
    pub resend_of: Option<String>,
    pub decision: Option<String>,
    /// `answer` or `question`, for a reply to a tracker decision.
    pub kind: Option<String>,
    /// What the user should see.
    pub label: String,
    /// Failed, or unacknowledged for too long.
    pub alarm: bool,
    /// `POST /deliveries/{id}/resend` would send it again.
    pub resendable: bool,
}

impl Delivery {
    /// Typed, not yet seen by its session, and not replaced by a resend.
    pub fn pending(&self) -> bool {
        matches!(self.state, DeliveryState::Delivered | DeliveryState::Queued)
            && self.received_at.is_none()
            && self.superseded_by.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Source {
    /// A context name; empty for a global source.
    pub scope: String,
    pub source: String,
    pub degraded: bool,
    /// The state in words.
    pub state: String,
    pub last_ok: Option<String>,
    pub last_error: Option<SourceError>,
    /// When the data now served was fetched.
    pub data_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SourceError {
    pub at: String,
    pub kind: String,
    pub key: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Notice {
    /// Rises by one per notice within an instance.
    pub seq: u64,
    pub at: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ServerInfo {
    pub api: String,
    pub instance: String,
    /// Version, commit and build time, as `fermata --version` prints them.
    pub build: String,
    pub version: String,
    pub commit: String,
    pub dry_run: bool,
    pub read_only: bool,
    pub uptime_s: u64,
    /// Where this fermata reads and writes.
    pub paths: Vec<NamedPath>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NamedPath {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub description: String,
    /// Oldest first.
    pub deliveries: Vec<Delivery>,
    /// Where an answer would go now.
    pub route: Route,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteKind {
    Session,
    Orchestrator,
    #[default]
    Note,
}

/// Where an answer would go now.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Route {
    pub kind: RouteKind,
    pub session_id: Option<String>,
    /// The pane's label; null for a note.
    pub pane: Option<String>,
    /// The dialog an answer would wait behind.
    pub dialog: Option<String>,
    /// Why it is only a note.
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Timeline {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Item {
    pub ts: Option<String>,
    pub source: String,
    pub title: String,
    pub body: String,
    /// A reference for `GET /transcripts/{ref}`.
    pub transcript: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Conversation {
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    User,
    #[default]
    Assistant,
    Tool,
    ToolResult,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Entry {
    pub kind: EntryKind,
    pub ts: Option<String>,
    /// One line (tool calls) or the full text (turns).
    pub head: String,
    pub body: String,
    /// Shown instead of the kind's own tag.
    pub label: Option<String>,
    pub link: Option<Link>,
    /// Collapses to `head` whatever its kind.
    pub folded: bool,
}

impl Entry {
    pub fn collapsible(&self) -> bool {
        self.folded || matches!(self.kind, EntryKind::Tool | EntryKind::ToolResult)
    }
}

/// The transcript behind an entry, and the time to open it at.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Link {
    pub transcript: String,
    pub ts: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Sessions {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Log {
    /// The end of the log; earlier lines are left out.
    pub text: String,
    /// The session's transcript, when it has one.
    pub transcript: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Pane {
    /// tmux's stable id.
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// Bytes left out before the first entry.
    pub skipped_bytes: u64,
    /// The entry `around` fell on.
    pub at: Option<usize>,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn s(v: &str) -> Option<String> {
        Some(v.into())
    }

    const AT: &str = "2026-01-02T03:04:05Z";

    fn stale() -> Stale {
        Stale {
            as_of: AT.into(),
            error: "timed out".into(),
        }
    }

    fn delivery() -> Delivery {
        Delivery {
            id: "d-1".into(),
            context: "work".into(),
            task: "work-1".into(),
            text: "go".into(),
            route: "session".into(),
            session_id: s("s-1"),
            pane: "main:1.0".into(),
            state: DeliveryState::NoteOnly,
            detail: "why".into(),
            created_at: AT.into(),
            sent_at: s(AT),
            received_at: s(AT),
            received_via: s("hook"),
            superseded_by: s("d-2"),
            resend_of: s("d-0"),
            decision: s("q1"),
            kind: s("answer"),
            label: "received".into(),
            alarm: false,
            resendable: true,
        }
    }

    fn task() -> Task {
        Task {
            context: "work".into(),
            id: "work-1".into(),
            title: "Add a thing".into(),
            assignee: s("me"),
            priority: Some(1),
            labels: vec!["x".into()],
            parent: s("work-0"),
            badge: Badge::NeedsYou,
            badge_text: "needs you".into(),
            rank: 0,
            pin_rank: Some(0),
            pins: Pins {
                ticket: true,
                epic: false,
            },
            mine: true,
            needs_you: true,
            state: Some(Declared {
                state: "parked".into(),
                on: s("ci"),
                until: s(AT),
                why: s("why"),
                next: s("next"),
                by: s("me"),
                at: s(AT),
                text: "parked".into(),
                badge: Badge::Parked,
            }),
            kept_open: false,
            waiting: s("on ci"),
            decisions: vec![Decision {
                key: "q1".into(),
                id: s("q1"),
                text: "Which?".into(),
                options: vec!["a".into()],
                recommendation: s("a"),
                status: DecisionStatus::Answered,
                asked_at: s(AT),
                answer: s("a"),
                answered_at: s(AT),
                pending_question: s("why?"),
                prompt: s("permission"),
                prompt_row: false,
                pane_only: false,
                label_missing: false,
            }],
            session_id: s("s-1"),
            branches: vec![Branch {
                repo: "app".into(),
                branch: "b".into(),
                worktree: "/w/b".into(),
                status: "open".into(),
                pr: s("7"),
                pr_url: s("https://example.com/7"),
            }],
            workers: vec![Worker {
                description: "tests".into(),
                finished: true,
                modified: s(AT),
            }],
            progress: Progress {
                status_line: s("half way"),
                checklist: Some((1, 2)),
                done: vec!["one".into()],
                next: vec!["two".into()],
            },
            ticket_key: s("K-1"),
            ticket_url: s("https://example.com/K-1"),
            ticket: Some(Ticket {
                key: "K-1".into(),
                title: s("Thing"),
                status: s("Open"),
                assignee: s("me"),
                url: s("https://example.com/K-1"),
                epic: Some(Epic {
                    key: "K-0".into(),
                    title: s("Epic"),
                    url: s("https://example.com/K-0"),
                }),
                extra_links: vec![Labelled {
                    label: "doc".into(),
                    url: "https://example.com/doc".into(),
                }],
            }),
            ticket_error: s("boom"),
            ticket_stale: Some(stale()),
            pr: Some(Pr {
                number: "7".into(),
                url: s("https://example.com/7"),
                state: "OPEN".into(),
                is_draft: false,
                review_decision: s("APPROVED"),
                checks: Checks {
                    pass: 1,
                    fail: 2,
                    pending: 3,
                    failing_url: s("https://example.com/f"),
                    latest_url: s("https://example.com/l"),
                    summary: "1/6".into(),
                },
            }),
            pr_error: s("boom"),
            pr_stale: Some(stale()),
            review_waiting: s("requested"),
            review_unknown: s("unread"),
            review_stale: Some(stale()),
            last_change: s(AT),
        }
    }

    fn session() -> Session {
        Session {
            id: "s-1".into(),
            context: s("work"),
            task: Some(TaskRef::new("work", "work-1")),
            orchestrator: false,
            worktree: "/w/b".into(),
            branch: s("b"),
            status: "waiting".into(),
            status_message: s("asks"),
            needs_input: true,
            dialog: s("permission"),
            description: s("the thing"),
            last_activity: s(AT),
            tool_calls: Some(3),
            tmux: "main:1.0".into(),
            pane: "main:1.0".into(),
            closed: true,
            closed_at: s(AT),
        }
    }

    /// Every type a client can be sent, each field set. No `..Default`: a new field must be added
    /// here, to the fixture and to API.md.
    pub fn every() -> Value {
        let state = State {
            instance: "i1".into(),
            rev: 41,
            phase: Phase::Loaded,
            loading: false,
            loaded_at: s(AT),
            errors: vec!["boom".into()],
            contexts: vec![Context {
                name: "work".into(),
                me: s("me"),
                stale: Some(stale()),
                counts: Counts {
                    shown: 1,
                    mine: 1,
                    needs_you: 1,
                    open_decisions: 1,
                    state_unknown: 0,
                    backlog: 2,
                    sessions: 1,
                },
            }],
            tasks: vec![task()],
            decisions: vec![DecisionRef {
                context: "work".into(),
                task: "work-1".into(),
                decision: "q1".into(),
            }],
            sessions: vec![session()],
            deliveries: vec![delivery()],
            health: vec![Source {
                scope: "work".into(),
                source: "tracker".into(),
                degraded: true,
                state: "failing".into(),
                last_ok: s(AT),
                last_error: Some(SourceError {
                    at: AT.into(),
                    kind: "timeout".into(),
                    key: "list".into(),
                    message: "timed out".into(),
                }),
                data_at: s(AT),
            }],
            notices: vec![Notice {
                seq: 1,
                at: AT.into(),
                text: "held answer sent".into(),
            }],
        };
        let entry = Entry {
            kind: EntryKind::ToolResult,
            ts: s(AT),
            head: "ls".into(),
            body: "out".into(),
            label: s("orchestrator"),
            link: Some(Link {
                transcript: "t1".into(),
                ts: s(AT),
            }),
            folded: true,
        };
        json!({
            "GET /server": Answer::Server(ServerInfo {
                api: VERSION.into(),
                instance: "i1".into(),
                build: "fermata 1 (abc)".into(),
                version: "1".into(),
                commit: "abc".into(),
                dry_run: false,
                read_only: true,
                uptime_s: 5,
                paths: vec![NamedPath { name: "log".into(), path: "/tmp/log".into() }],
            }),
            "GET /state": state,
            "GET /tasks/{context}/{id}": Answer::Task(Box::new(TaskDetail {
                task: task(),
                description: "Do it".into(),
                deliveries: vec![delivery()],
                route: Route {
                    kind: RouteKind::Session,
                    session_id: s("s-1"),
                    pane: s("main:1.0"),
                    dialog: s("permission"),
                    reason: s("why"),
                },
            })),
            "GET /tasks/{context}/{id}/timeline": Answer::Timeline(Timeline {
                items: vec![Item {
                    ts: s(AT),
                    source: "note".into(),
                    title: "t".into(),
                    body: "b".into(),
                    transcript: s("t1"),
                }],
            }),
            "GET /tasks/{context}/{id}/conversation":
                Answer::Conversation(Conversation { entries: vec![entry.clone()] }),
            "GET /sessions": Answer::Sessions(Sessions { sessions: vec![session()] }),
            "GET /sessions/{id}/log":
                Answer::Log(Log { text: "log".into(), transcript: s("t1") }),
            "GET /sessions/{id}/pane":
                Answer::Pane(Pane { id: "%1".into(), label: "main:1.0".into() }),
            "GET /transcripts/{ref}": Answer::Transcript(Transcript {
                entries: vec![entry],
                skipped_bytes: 10,
                at: Some(0),
            }),
            "event": Event { instance: "i1".into(), rev: 41, sections: Section::ALL.to_vec() },
            "reply": Reply { summary: "sent".into(), delivery: Some(delivery()) },
            "error": json!({"error": Error {
                code: Code::NeedsConfirm,
                message: "close it?".into(),
                confirm: s("abc"),
            }}),
            "values": {
                "code": [
                    Code::BadRequest, Code::Unauthorized, Code::Forbidden, Code::ReadOnly,
                    Code::NotFound, Code::NeedsConfirm, Code::Gone, Code::SourceFailed,
                ],
                "phase": [Phase::Empty, Phase::Preview, Phase::Loaded],
                "badge": [
                    Badge::NeedsYou, Badge::ReviewRequested, Badge::ReviewUnknown, Badge::Overdue,
                    Badge::CiFailing, Badge::ReadyToMerge, Badge::WaitingOnCi, Badge::StateStale,
                    Badge::Stalled, Badge::WorkerRunning, Badge::Interactive, Badge::Working,
                    Badge::InReview, Badge::Blocked, Badge::WaitingUntil, Badge::Paused,
                    Badge::DoneCleanup, Badge::Parked, Badge::Unknown,
                ],
                "decision status":
                    [DecisionStatus::Open, DecisionStatus::Answered, DecisionStatus::Resolved],
                "delivery state": [
                    DeliveryState::Delivered, DeliveryState::Queued, DeliveryState::Held,
                    DeliveryState::NoteOnly, DeliveryState::Failed,
                ],
                "route kind": [RouteKind::Session, RouteKind::Orchestrator, RouteKind::Note],
                "entry kind": [
                    EntryKind::User, EntryKind::Assistant, EntryKind::Tool, EntryKind::ToolResult,
                ],
            },
        })
    }

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/api-v1.json");

    /// The contract. Fields may be added (`FERMATA_BLESS=1`), never renamed or removed.
    #[test]
    fn the_shape_is_pinned() {
        let now = serde_json::to_string_pretty(&every()).unwrap() + "\n";
        if std::env::var_os("FERMATA_BLESS").is_some() {
            std::fs::write(FIXTURE, &now).unwrap();
        }
        let was = std::fs::read_to_string(FIXTURE).unwrap();
        assert!(now == was, "the API's shape differs from {FIXTURE}");
    }

    #[test]
    fn the_documentation_names_every_field_and_value() {
        fn names(v: &Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                Value::Object(m) => {
                    for (k, v) in m {
                        out.insert(k.clone());
                        names(v, out);
                    }
                }
                Value::Array(a) => a.iter().for_each(|v| names(v, out)),
                _ => {}
            }
        }
        let doc = include_str!("../API.md");
        let mut all = std::collections::BTreeSet::new();
        for (what, v) in every().as_object().unwrap() {
            if what == "values" {
                for v in v.as_object().unwrap().values() {
                    for v in v.as_array().unwrap() {
                        all.insert(v.as_str().unwrap().to_string());
                    }
                }
            } else {
                names(v, &mut all);
            }
        }
        all.extend(Section::ALL.map(|s| json!(s).as_str().unwrap().to_string()));
        let missing: Vec<_> = all
            .iter()
            .filter(|n| !doc.contains(&format!("`{n}`")))
            .collect();
        assert!(missing.is_empty(), "not in API.md: {missing:?}");
    }
}
