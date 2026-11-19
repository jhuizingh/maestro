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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TicketError {
    pub kind: TicketErrorKind,
    pub message: Option<String>,
    /// Only for `Timeout`: the budget that ran out, in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
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
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Progress {
    pub status_line: Option<String>,
    pub checklist: Option<(usize, usize)>,
    pub done: Vec<String>,
    pub next: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Decision {
    pub text: String,
    pub options: Vec<String>,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Badge {
    NeedsYou,
    ReviewRequested,
    Overdue,
    CiFailing,
    ReadyToMerge,
    WaitingOnCi,
    WorkerRunning,
    Interactive,
    Parked,
    Idle,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::NeedsYou => "needs you",
            Badge::ReviewRequested => "review requested",
            Badge::Overdue => "overdue",
            Badge::CiFailing => "CI failing",
            Badge::ReadyToMerge => "ready to merge",
            Badge::WaitingOnCi => "waiting on CI",
            Badge::WorkerRunning => "worker running",
            Badge::Interactive => "interactive",
            Badge::Parked => "parked",
            Badge::Idle => "idle",
        }
    }
}

/// A live or closed ccmon session record.
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
    #[serde(skip_deserializing, default)]
    pub closed: bool,
}

impl Session {
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
    pub pr: Option<PrInfo>,
    pub pr_error: Option<String>,
    pub progress: Progress,
    pub decision: Option<Decision>,
    pub session: Option<Session>,
    pub workers: Vec<Worker>,
    pub badge: Badge,
    pub pin_rank: Option<usize>,
    pub last_change: Option<String>,
    /// Why a review of somebody else's PR is waiting on you (`pr-review.sh rank`); None if not.
    pub review_waiting: Option<String>,
}

impl JoinedTask {
    pub fn worktrees(&self) -> Vec<&str> {
        self.branches
            .iter()
            .map(|b| b.worktree.as_str())
            .filter(|w| !w.is_empty())
            .collect()
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

    /// Kept open and past its `until` date. A date that does not parse is never overdue.
    pub fn overdue_on(&self, today: chrono::NaiveDate) -> bool {
        self.kept_open()
            && self
                .waiting()
                .and_then(|(_, u)| chrono::NaiveDate::parse_from_str(u?, "%Y-%m-%d").ok())
                .is_some_and(|d| d < today)
    }

    pub fn epic_url(&self) -> Option<String> {
        self.ticket.as_ref()?.epic.as_ref()?.url.clone()
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
    fn check_summary() {
        let c = Checks {
            pass: 3,
            fail: 1,
            ..Default::default()
        };
        assert_eq!(c.summary(), "✗1 ✓3");
    }
}
