use serde::{Deserialize, Deserializer, Serialize};

/// Accept a JSON string or number (the branch registry stores `pr` as either).
fn opt_string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

fn opt_nonempty<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Option::<String>::deserialize(d)?.filter(|s| !s.trim().is_empty()))
}

fn null_as_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// A tracker task object, as emitted by `tracker.sh list/get`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub title: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub description: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub notes: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(default)]
    pub priority: Option<i64>,
    /// Who the task is assigned to; `None` when unassigned (null or empty).
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub assignee: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub labels: Vec<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    /// The declared state (`tracker.sh state`); None when none was declared.
    #[serde(default)]
    pub state: Option<TaskState>,
    /// Filled by fermata from `decisions` for `needs-decision` tasks; `None` when not read.
    #[serde(default)]
    pub decisions: Option<Vec<Decision>>,
}

/// A declared task state, from baton's closed vocabulary (`references/declared-state.md`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TaskState {
    #[serde(default, deserialize_with = "null_as_default")]
    pub state: String,
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub on: Option<String>,
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub until: Option<String>,
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub why: Option<String>,
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub next: Option<String>,
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub by: Option<String>,
    #[serde(default, deserialize_with = "opt_nonempty")]
    pub at: Option<String>,
}

impl TaskState {
    /// The badge a declared state shows when no world fact outranks it.
    pub fn badge(&self) -> Badge {
        match self.state.as_str() {
            "waiting-on-me" => Badge::NeedsYou,
            "working" => Badge::Working,
            "waiting-on-review" => Badge::InReview,
            "blocked" => Badge::Blocked,
            "waiting-until" => Badge::WaitingUntil,
            "paused" => Badge::Paused,
            "done-pending-cleanup" => Badge::DoneCleanup,
            _ => Badge::Unknown,
        }
    }

    /// Its `until`, when it parses as `YYYY-MM-DD`.
    pub fn until_date(&self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(self.until.as_deref()?, "%Y-%m-%d").ok()
    }

    /// One line for a row: "blocked on X until D", "paused: why", "waiting-on-review PR #1".
    pub fn text(&self) -> String {
        let name = match self.badge() {
            Badge::Unknown => "state ?",
            _ => self.state.as_str(),
        };
        let mut s = name.to_string();
        if let Some(on) = &self.on {
            s.push_str(if self.state == "blocked" { " on " } else { " " });
            s.push_str(on);
        }
        if let Some(u) = &self.until {
            s.push_str(&format!(" until {u}"));
        }
        if let Some(w) = &self.why {
            s.push_str(&format!(": {w}"));
        }
        if let Some(n) = &self.next {
            s.push_str(&format!(" (next: {n})"));
        }
        s
    }
}

impl Task {
    pub fn has_label(&self, l: &str) -> bool {
        self.labels.iter().any(|x| x == l)
    }
}

/// A branch registry entry from `tracker.sh list-branches`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Branch {
    #[serde(default, deserialize_with = "null_as_default")]
    pub repo: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub branch: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub worktree: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub pr: Option<String>,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub updated: Option<String>,
    /// `yes` when `baton:finish` left the task open on purpose.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub keep_task_open: Option<String>,
    /// What a parked task is waiting on, and the date (`YYYY-MM-DD`) to look again by.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub waiting_on: Option<String>,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub until: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct LinkRef {
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtraLink {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub url: String,
}

/// Why `info` produced no TicketInfo. See "When `info` fails" in DESIGN.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketErrorKind {
    NotFound,
    Unavailable,
    /// fermata killed the adapter at its `timeout`; never sent by an adapter.
    Timeout,
    Error,
}

impl TicketErrorKind {
    /// The kinds an adapter may send; anything else reads as `Error`.
    pub fn from_adapter(s: Option<&str>) -> Self {
        match s {
            Some("not_found") => Self::NotFound,
            Some("unavailable") => Self::Unavailable,
            _ => Self::Error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TicketError {
    pub kind: TicketErrorKind,
    pub message: Option<String>,
    /// Only for `Timeout`: the budget that ran out, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

impl TicketError {
    pub fn new(kind: TicketErrorKind, message: impl Into<String>) -> Self {
        let m = message.into();
        let m = m.lines().map(str::trim).find(|l| !l.is_empty());
        Self {
            kind,
            message: m.map(str::to_string),
            timeout_secs: None,
        }
    }

    pub fn label(&self) -> String {
        match self.kind {
            TicketErrorKind::NotFound => "ticket not found".into(),
            TicketErrorKind::Unavailable => "ticket source unavailable".into(),
            TicketErrorKind::Timeout => match self.timeout_secs {
                Some(s) => format!("adapter timed out after {s}s"),
                None => "adapter timed out".into(),
            },
            TicketErrorKind::Error => "adapter error".into(),
        }
    }
}

impl std::fmt::Display for TicketError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.message {
            Some(m) => write!(f, "{}: {m}", self.label()),
            None => f.write_str(&self.label()),
        }
    }
}

/// A last good value served after a failed refresh. See "Failures, staleness and logging".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stale {
    /// When the value was last fetched successfully (RFC 3339).
    pub as_of: String,
    /// Why the latest refresh failed.
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

/// What an adapter's `info` command prints. Every field except `key` is optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TicketInfo {
    pub key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub epic: Option<LinkRef>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub extra_links: Vec<ExtraLink>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Checks {
    pub pass: usize,
    pub fail: usize,
    pub pending: usize,
    pub failing_url: Option<String>,
    pub latest_url: Option<String>,
}

impl Checks {
    pub fn summary(&self) -> String {
        let total = self.pass + self.fail + self.pending;
        if total == 0 {
            return "no checks".into();
        }
        if self.fail > 0 {
            format!("✗{} ✓{}", self.fail, self.pass)
        } else if self.pending > 0 {
            format!("…{} ✓{}", self.pending, self.pass)
        } else {
            format!("✓{}", self.pass)
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PrInfo {
    pub number: String,
    pub url: Option<String>,
    pub state: String,
    pub is_draft: bool,
    pub review_decision: Option<String>,
    pub checks: Checks,
    /// `{state, reviewRequests, latestReviews}` as gh gave it: `pr-review.sh rank`'s input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Progress {
    pub status_line: Option<String>,
    pub checklist: Option<(usize, usize)>,
    pub done: Vec<String>,
    pub next: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    /// Waiting on you.
    #[default]
    Open,
    /// You answered; waiting on the orchestrator to act.
    Answered,
    Resolved,
}

/// A question waiting on the user: a tracker decision (`id` set) or a legacy DECISION note.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    /// The tracker's decision id; `None` for a legacy note, answered by an ANSWER note.
    #[serde(default)]
    pub id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub recommendation: Option<String>,
    /// A legacy `DECISION` note at the end of the notes on a task without `needs-decision`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub label_missing: bool,
    #[serde(default)]
    pub status: DecisionStatus,
    #[serde(default)]
    pub asked_at: Option<String>,
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub answered_at: Option<String>,
    /// Your question about it, while the orchestrator has not responded.
    #[serde(default)]
    pub pending_question: Option<String>,
    /// Every answer, question and response, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replies: Vec<Reply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// Open in the session's pane right now, and as what kind of prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// One reply on a tracker decision.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Reply {
    /// `answer`, `question` or `response`.
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

impl Decision {
    /// Stable within a task: the decision id, "prompt" or "legacy".
    pub fn key(&self) -> &str {
        let other = if self.prompt.is_some() {
            "prompt"
        } else {
            "legacy"
        };
        self.id.as_deref().unwrap_or(other)
    }

    /// A session's prompt that no tracker decision stands for.
    pub fn is_prompt_row(&self) -> bool {
        self.id.is_none() && self.prompt.is_some()
    }

    /// A dialog only its pane can answer.
    pub fn pane_only(&self) -> bool {
        self.is_prompt_row() && self.prompt.as_deref() != Some(crate::prompts::INPUT)
    }

    pub fn is_open(&self) -> bool {
        self.status == DecisionStatus::Open
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
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
    Unknown,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::NeedsYou => "needs you",
            Badge::ReviewRequested => "review requested",
            Badge::ReviewUnknown => "review ?",
            Badge::Overdue => "overdue",
            Badge::CiFailing => "CI failing",
            Badge::ReadyToMerge => "ready to merge",
            Badge::WaitingOnCi => "waiting on CI",
            Badge::WorkerRunning => "worker running",
            Badge::Interactive => "interactive",
            Badge::StateStale => "state stale",
            Badge::Stalled => "stalled",
            Badge::Working => "working",
            Badge::InReview => "in review",
            Badge::Blocked => "blocked",
            Badge::WaitingUntil => "waiting until",
            Badge::Paused => "paused",
            Badge::DoneCleanup => "done, cleanup",
            Badge::Parked => "parked",
            Badge::Unknown => "state unknown",
        }
    }
}

fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| serde_json::from_value(v).ok()))
}

/// What a session's pane is asking right now (`prompts`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Prompt {
    /// `question`, `plan`, `permission` or `elicitation`.
    pub kind: String,
    pub text: String,
    pub options: Vec<String>,
    pub at: String,
    /// The tracker decisions that mirror it.
    pub decisions: Vec<String>,
    /// The subagent that asked; empty for the main one.
    pub agent: String,
}

/// A live or closed session record (see `store`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Session {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub claude_pid: Option<i64>,
    #[serde(default)]
    pub tmux_target: Option<String>,
    #[serde(default)]
    pub tmux_session: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub worktree: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub status: String,
    #[serde(default)]
    pub status_message: Option<String>,
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub last_tool: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub last_activity: Option<String>,
    #[serde(default)]
    pub session_start: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<u64>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub closed_at: Option<String>,
    /// The dialog open in the pane (`AskUserQuestion`, `permission`, …); typing would answer it.
    #[serde(default)]
    pub prompt_open: Option<String>,
    /// What that dialog asks; unreadable is `None`, never a lost session.
    #[serde(default, deserialize_with = "lenient")]
    pub prompt: Option<Prompt>,
    /// The end of the session's last message, from its last stop.
    #[serde(default, deserialize_with = "lenient")]
    pub last_message: Option<String>,
    #[serde(default, deserialize_with = "lenient")]
    pub idle_since: Option<String>,
    #[serde(skip_deserializing, default)]
    pub closed: bool,
}

impl Session {
    /// Where it runs, as recorded: `tmux_target`, else (closed records keep only this) the
    /// tmux session name.
    pub fn tmux_label(&self) -> &str {
        self.tmux_target
            .as_deref()
            .filter(|t| !t.is_empty())
            .or(self.tmux_session.as_deref())
            .unwrap_or_default()
    }

    pub fn needs_input(&self) -> bool {
        matches!(self.status.as_str(), "needs_input" | "needs_permission")
    }
}

/// A background worker (subagent) whose description names a task.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Worker {
    pub description: String,
    pub transcript: String,
    pub finished: bool,
    pub modified: Option<String>,
}

/// A task joined with everything fermata knows about it.
#[derive(Debug, Clone, Serialize)]
pub struct JoinedTask {
    pub context: String,
    pub task: Task,
    pub branches: Vec<Branch>,
    pub ticket_key: Option<String>,
    pub ticket_url: Option<String>,
    pub ticket: Option<TicketInfo>,
    pub ticket_error: Option<TicketError>,
    /// Set when `ticket` is the last good value and the latest refresh failed.
    pub ticket_stale: Option<Stale>,
    pub pr: Option<PrInfo>,
    pub pr_error: Option<String>,
    /// Set when `pr` is the last good state and the latest refresh failed.
    pub pr_stale: Option<Stale>,
    pub progress: Progress,
    /// Unresolved decisions, open and answered, oldest first.
    pub decisions: Vec<Decision>,
    pub session: Option<Session>,
    pub workers: Vec<Worker>,
    pub badge: Badge,
    pub pin_rank: Option<usize>,
    pub last_change: Option<String>,
    /// Why a review of somebody else's PR is waiting on you (`pr-review.sh rank`); None if not.
    pub review_waiting: Option<String>,
    /// Why a review's state could not be read; it may be waiting, so it sorts with the waiting.
    pub review_unknown: Option<String>,
    /// Set when the review verdict is the last known one, or was ranked from stale PR data.
    pub review_stale: Option<Stale>,
}

impl JoinedTask {
    pub fn worktrees(&self) -> Vec<&str> {
        self.branches
            .iter()
            .map(|b| b.worktree.as_str())
            .filter(|w| !w.is_empty())
            .collect()
    }

    /// Decisions waiting on you (not those you answered).
    pub fn open_decisions(&self) -> impl Iterator<Item = &Decision> {
        self.decisions.iter().filter(|d| d.is_open())
    }

    pub fn needs_you(&self) -> bool {
        self.badge == Badge::NeedsYou
    }

    /// Shown under "mine": assigned to `me`, unassigned, or waiting on you whoever holds it.
    /// An unknown `me` (`None`) counts everything as yours; fermata never guesses.
    pub fn is_mine(&self, me: Option<&str>) -> bool {
        let Some(me) = me else { return true };
        let Some(a) = self.task.assignee.as_deref() else {
            return true;
        };
        a.trim().to_lowercase() == me.trim().to_lowercase()
            || self.needs_you()
            || self.review_waiting.is_some()
    }

    /// Left open on purpose: the `keep-task-open` label, or a registry entry saying so.
    pub fn kept_open(&self) -> bool {
        self.task.has_label("keep-task-open")
            || self
                .branches
                .iter()
                .any(|b| b.keep_task_open.as_deref() == Some("yes"))
    }

    /// The newest registry entry's `waiting_on` and `until`, when it recorded either.
    pub fn waiting(&self) -> Option<(Option<&str>, Option<&str>)> {
        self.branches
            .iter()
            .rev()
            .find(|b| b.waiting_on.is_some() || b.until.is_some())
            .map(|b| (b.waiting_on.as_deref(), b.until.as_deref()))
    }

    /// "waiting on X until D", or the half that was recorded.
    pub fn waiting_text(&self) -> Option<String> {
        Some(match self.waiting()? {
            (Some(w), Some(u)) => format!("waiting on {w} until {u}"),
            (Some(w), None) => format!("waiting on {w}"),
            (None, Some(u)) => format!("waiting until {u}"),
            (None, None) => return None,
        })
    }

    /// Kept open and past its registry `until`, or a declared `blocked` / `waiting-until` past
    /// its `until`. A date that does not parse is never overdue.
    pub fn overdue_on(&self, today: chrono::NaiveDate) -> bool {
        let declared = self.task.state.as_ref().is_some_and(|s| {
            matches!(s.state.as_str(), "blocked" | "waiting-until")
                && s.until_date().is_some_and(|d| d < today)
        });
        declared
            || self.kept_open()
                && self
                    .waiting()
                    .and_then(|(_, u)| chrono::NaiveDate::parse_from_str(u?, "%Y-%m-%d").ok())
                    .is_some_and(|d| d < today)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn assignee_blank_or_null_is_unassigned() {
        let t = |j: &str| serde_json::from_str::<super::Task>(j).unwrap().assignee;
        assert_eq!(t(r#"{"id":"x","assignee":"Pat"}"#).as_deref(), Some("Pat"));
        assert_eq!(t(r#"{"id":"x","assignee":""}"#), None);
        assert_eq!(t(r#"{"id":"x","assignee":null}"#), None);
        assert_eq!(t(r#"{"id":"x"}"#), None);
    }

    use super::*;

    #[test]
    fn closed_records_label_by_tmux_session() {
        let mut s = Session {
            tmux_session: Some("work".into()),
            ..Default::default()
        };
        assert_eq!(s.tmux_label(), "work");
        s.tmux_target = Some("work:1.2".into());
        assert_eq!(s.tmux_label(), "work:1.2");
    }

    #[test]
    fn task_parses_tracker_shape() {
        let j =
            r#"{"id":"ctx-1","title":"t","notes":null,"labels":["a"],"priority":2,"parent":null}"#;
        let t: Task = serde_json::from_str(j).unwrap();
        assert_eq!(t.id, "ctx-1");
        assert!(t.has_label("a"));
        assert_eq!(t.notes, "");
    }

    #[test]
    fn branch_pr_number_or_string() {
        let b: Branch = serde_json::from_str(r#"{"repo":"r","pr":4537}"#).unwrap();
        assert_eq!(b.pr.as_deref(), Some("4537"));
        let b: Branch = serde_json::from_str(r#"{"repo":"r","pr":"12"}"#).unwrap();
        assert_eq!(b.pr.as_deref(), Some("12"));
        let b: Branch = serde_json::from_str(r#"{"repo":"r"}"#).unwrap();
        assert_eq!(b.pr, None);
    }

    #[test]
    fn branch_parked_fields() {
        let b: Branch = serde_json::from_str(
            r#"{"repo":"r","keep_task_open":"yes","waiting_on":"vendor fix","until":"2026-05-20"}"#,
        )
        .unwrap();
        assert_eq!(b.keep_task_open.as_deref(), Some("yes"));
        assert_eq!(b.waiting_on.as_deref(), Some("vendor fix"));
        assert_eq!(b.until.as_deref(), Some("2026-05-20"));
        let b: Branch =
            serde_json::from_str(r#"{"repo":"r","waiting_on":"","until":null}"#).unwrap();
        assert_eq!((b.waiting_on, b.until), (None, None));
    }

    #[test]
    fn ticket_info_ignores_unknown() {
        let t: TicketInfo =
            serde_json::from_str(r#"{"key":"X-1","weird":1,"epic":{"key":"X-0"}}"#).unwrap();
        assert_eq!(t.epic.unwrap().key, "X-0");
    }

    #[test]
    fn declared_state_parses_and_reads() {
        let t: Task = serde_json::from_str(
            r#"{"id":"x","state":{"state":"blocked","on":"Security sign-off","until":"2026-12-01","why":null,"by":"Pat","at":"2026-11-23T10:00:00Z"}}"#,
        )
        .unwrap();
        let s = t.state.unwrap();
        assert_eq!(s.badge(), Badge::Blocked);
        assert_eq!(s.text(), "blocked on Security sign-off until 2026-12-01");
        let t: Task = serde_json::from_str(r#"{"id":"x","state":null}"#).unwrap();
        assert_eq!(t.state, None);
        let t: Task = serde_json::from_str(r#"{"id":"x","state":{"state":"unknown"}}"#).unwrap();
        assert_eq!(t.state.as_ref().unwrap().badge(), Badge::Unknown);
        assert_eq!(t.state.unwrap().text(), "state ?");
    }

    #[test]
    fn check_summary() {
        let c = Checks {
            pass: 3,
            fail: 1,
            ..Default::default()
        };
        assert_eq!(c.summary(), "✗1 ✓3");
    }
}
