//! The engine's data as API types: the one place that decides what clients see.
use crate::api;
use crate::config::{Context, Priority, context_of_path};
use crate::core::Snapshot;
use crate::delivery::{self, State as DState};
use crate::health;
use crate::history;
use crate::model::{self, JoinedTask};
use crate::routing::Route;
use crate::sessions::same_path;
use crate::transcript;
use chrono::{DateTime, Utc};
use std::collections::HashMap;

/// Everything a state document is made from.
pub struct Parts<'a> {
    pub snap: &'a Snapshot,
    pub deliveries: &'a [delivery::Delivery],
    pub health: &'a [health::Row],
    /// Pane label by session id.
    pub panes: &'a HashMap<String, String>,
    pub phase: api::Phase,
    pub loading: bool,
    pub notices: Vec<api::Notice>,
    pub now: DateTime<Utc>,
}

/// The document without `instance` and `rev`, which the engine sets.
pub fn state(p: &Parts) -> api::State {
    let s = p.snap;
    let tasks: Vec<api::Task> = {
        let mut next: HashMap<&str, usize> = HashMap::new();
        s.tasks
            .iter()
            .map(|t| {
                let rank = next.entry(&t.context).or_default();
                let ctx = s.contexts.iter().find(|c| c.name == t.context);
                let out = task(t, ctx, s.me.get(&t.context).map(String::as_str), *rank);
                *rank += 1;
                out
            })
            .collect()
    };
    let sessions: Vec<api::Session> = s
        .sessions
        .iter()
        .map(|x| session(x, s, p.panes.get(&x.session_id).cloned()))
        .collect();
    let decisions = s
        .contexts
        .iter()
        .flat_map(|c| tasks.iter().filter(move |t| t.context == c.name))
        .flat_map(|t| {
            t.decisions.iter().map(|d| api::DecisionRef {
                context: t.context.clone(),
                task: t.id.clone(),
                decision: d.key.clone(),
            })
        })
        .collect();
    api::State {
        instance: String::new(),
        rev: 0,
        phase: p.phase,
        loading: p.loading,
        loaded_at: (!s.loaded_at.is_empty()).then(|| s.loaded_at.clone()),
        errors: s.errors.clone(),
        contexts: contexts(s, &tasks, &sessions),
        decisions,
        deliveries: deliveries(p.deliveries, &s.sessions, p.now),
        health: p.health.iter().map(source).collect(),
        notices: p.notices.clone(),
        tasks,
        sessions,
    }
}

fn contexts(s: &Snapshot, tasks: &[api::Task], sessions: &[api::Session]) -> Vec<api::Context> {
    let one = |name: &str| {
        let ours = || tasks.iter().filter(|t| t.context == name);
        api::Context {
            name: name.into(),
            me: s.me.get(name).cloned(),
            stale: s.tracker_stale.get(name).map(stale),
            counts: api::Counts {
                shown: ours().count(),
                mine: ours().filter(|t| t.mine).count(),
                needs_you: ours().filter(|t| t.needs_you).count(),
                open_decisions: ours().map(|t| t.open_decisions().count()).sum(),
                state_unknown: ours().filter(|t| t.badge == api::Badge::Unknown).count(),
                backlog: s.backlog.get(name).copied().unwrap_or(0),
                sessions: sessions
                    .iter()
                    .filter(|x| x.context.as_deref().unwrap_or("") == name)
                    .count(),
            },
        }
    };
    let mut v: Vec<_> = s.contexts.iter().map(|c| one(&c.name)).collect();
    v.push(one(""));
    v
}

fn stale(s: &model::Stale) -> api::Stale {
    api::Stale {
        as_of: s.as_of.clone(),
        error: s.error.clone(),
    }
}

pub fn badge(b: model::Badge) -> api::Badge {
    use model::Badge as M;
    match b {
        M::NeedsYou => api::Badge::NeedsYou,
        M::ReviewRequested => api::Badge::ReviewRequested,
        M::ReviewUnknown => api::Badge::ReviewUnknown,
        M::Overdue => api::Badge::Overdue,
        M::CiFailing => api::Badge::CiFailing,
        M::ReadyToMerge => api::Badge::ReadyToMerge,
        M::WaitingOnCi => api::Badge::WaitingOnCi,
        M::StateStale => api::Badge::StateStale,
        M::Stalled => api::Badge::Stalled,
        M::WorkerRunning => api::Badge::WorkerRunning,
        M::Interactive => api::Badge::Interactive,
        M::Working => api::Badge::Working,
        M::InReview => api::Badge::InReview,
        M::Blocked => api::Badge::Blocked,
        M::WaitingUntil => api::Badge::WaitingUntil,
        M::Paused => api::Badge::Paused,
        M::DoneCleanup => api::Badge::DoneCleanup,
        M::Parked => api::Badge::Parked,
        M::Unknown => api::Badge::Unknown,
    }
}

/// The pins a task's ticket and epic would be, when it has them.
pub fn pin_of(t: &JoinedTask, target: api::PinTarget) -> Option<Priority> {
    match target {
        api::PinTarget::Ticket => t.ticket_key.clone().map(|k| Priority {
            ticket: Some(k),
            epic: None,
        }),
        api::PinTarget::Epic => {
            let e = t.ticket.as_ref()?.epic.as_ref()?;
            Some(Priority {
                epic: Some(e.key.clone()),
                ticket: None,
            })
        }
    }
}

fn pins(t: &JoinedTask, ctx: Option<&Context>) -> api::Pins {
    let list = ctx
        .and_then(|c| c.adapter.as_ref())
        .map(|a| a.priorities.as_slice())
        .unwrap_or_default();
    let has = |x| pin_of(t, x).is_some_and(|p| list.contains(&p));
    api::Pins {
        ticket: has(api::PinTarget::Ticket),
        epic: has(api::PinTarget::Epic),
    }
}

pub fn task(t: &JoinedTask, ctx: Option<&Context>, me: Option<&str>, rank: usize) -> api::Task {
    api::Task {
        context: t.context.clone(),
        id: t.task.id.clone(),
        title: t.task.title.clone(),
        assignee: t.task.assignee.clone(),
        priority: t.task.priority,
        labels: t.task.labels.clone(),
        parent: t.task.parent.clone(),
        badge: badge(t.badge),
        badge_text: t.badge.label().into(),
        rank,
        pin_rank: t.pin_rank,
        pins: pins(t, ctx),
        mine: t.is_mine(me),
        needs_you: t.needs_you(),
        state: t.task.state.as_ref().map(|s| api::Declared {
            state: s.state.clone(),
            on: s.on.clone(),
            until: s.until.clone(),
            why: s.why.clone(),
            next: s.next.clone(),
            by: s.by.clone(),
            at: s.at.clone(),
            text: s.text(),
            badge: badge(s.badge()),
        }),
        kept_open: t.kept_open(),
        waiting: t.waiting_text(),
        decisions: t.decisions.iter().map(decision).collect(),
        session_id: t.session.as_ref().map(|s| s.session_id.clone()),
        branches: t
            .branches
            .iter()
            .map(|b| api::Branch {
                repo: b.repo.clone(),
                branch: b.branch.clone(),
                worktree: b.worktree.clone(),
                status: b.status.clone(),
                pr: b.pr.clone(),
                pr_url: b.pr_url.clone(),
            })
            .collect(),
        workers: t
            .workers
            .iter()
            .map(|w| api::Worker {
                description: w.description.clone(),
                finished: w.finished,
                modified: w.modified.clone(),
            })
            .collect(),
        progress: api::Progress {
            status_line: t.progress.status_line.clone(),
            checklist: t.progress.checklist,
            done: t.progress.done.clone(),
            next: t.progress.next.clone(),
        },
        ticket_key: t.ticket_key.clone(),
        ticket_url: t.ticket_url.clone(),
        ticket: t.ticket.as_ref().map(|i| api::Ticket {
            key: i.key.clone(),
            title: i.title.clone(),
            status: i.status.clone(),
            assignee: i.assignee.clone(),
            url: i.url.clone(),
            epic: i.epic.as_ref().map(|e| api::Epic {
                key: e.key.clone(),
                title: e.title.clone(),
                url: e.url.clone(),
            }),
            extra_links: i
                .extra_links
                .iter()
                .map(|l| api::Labelled {
                    label: l.label.clone(),
                    url: l.url.clone(),
                })
                .collect(),
        }),
        ticket_error: t.ticket_error.as_ref().map(|e| e.to_string()),
        ticket_stale: t.ticket_stale.as_ref().map(stale),
        pr: t.pr.as_ref().map(|p| api::Pr {
            number: p.number.clone(),
            url: p.url.clone(),
            state: p.state.clone(),
            is_draft: p.is_draft,
            review_decision: p.review_decision.clone(),
            checks: api::Checks {
                pass: p.checks.pass,
                fail: p.checks.fail,
                pending: p.checks.pending,
                failing_url: p.checks.failing_url.clone(),
                latest_url: p.checks.latest_url.clone(),
                summary: p.checks.summary(),
            },
        }),
        pr_error: t.pr_error.clone(),
        pr_stale: t.pr_stale.as_ref().map(stale),
        review_waiting: t.review_waiting.clone(),
        review_unknown: t.review_unknown.clone(),
        review_stale: t.review_stale.as_ref().map(stale),
        last_change: t.last_change.clone(),
    }
}

fn decision(d: &model::Decision) -> api::Decision {
    api::Decision {
        key: d.key().to_string(),
        id: d.id.clone(),
        text: d.text.clone(),
        options: d.options.clone(),
        recommendation: d.recommendation.clone(),
        status: match d.status {
            model::DecisionStatus::Open => api::DecisionStatus::Open,
            model::DecisionStatus::Answered => api::DecisionStatus::Answered,
            model::DecisionStatus::Resolved => api::DecisionStatus::Resolved,
        },
        asked_at: d.asked_at.clone(),
        answer: d.answer.clone(),
        answered_at: d.answered_at.clone(),
        pending_question: d.pending_question.clone(),
        prompt: d.prompt.clone(),
        prompt_row: d.is_prompt_row(),
        pane_only: d.pane_only(),
        label_missing: d.label_missing,
    }
}

/// A session's task: the first whose worktree it runs in.
pub fn task_of_session<'a>(s: &model::Session, snap: &'a Snapshot) -> Option<&'a JoinedTask> {
    snap.tasks
        .iter()
        .find(|t| t.worktrees().iter().any(|w| same_path(&s.worktree, w)))
}

pub fn session(s: &model::Session, snap: &Snapshot, pane: Option<String>) -> api::Session {
    let t = task_of_session(s, snap);
    // Its task's context; else the one baton resolves its directory to.
    let context = t.map(|t| t.context.clone()).or_else(|| {
        context_of_path(&snap.contexts, std::path::Path::new(&s.worktree)).map(|c| c.name.clone())
    });
    api::Session {
        id: s.session_id.clone(),
        context,
        task: t.map(|t| api::TaskRef::new(&t.context, &t.task.id)),
        orchestrator: snap
            .contexts
            .iter()
            .any(|c| same_path(&s.worktree, &c.workspace.to_string_lossy())),
        worktree: s.worktree.clone(),
        branch: s.branch.clone(),
        status: s.status.clone(),
        status_message: s.status_message.clone(),
        needs_input: s.needs_input(),
        waiting_on: match t {
            Some(t) if t.open_decisions().any(|d| !d.is_prompt_row()) => None,
            t => crate::prompts::waiting_on(s, t.map(|t| t.task.notes.as_str())),
        },
        dialog: delivery::open_prompt(s),
        description: s.description.clone(),
        last_activity: s.last_activity.clone(),
        tool_calls: s.tool_calls,
        tmux: s.tmux_label().to_string(),
        pane: pane.unwrap_or_else(|| s.tmux_target.clone().unwrap_or_default()),
        closed: s.closed,
        closed_at: s.closed_at.clone(),
    }
}

/// Every delivery, oldest first. Only a task's latest unreceived one may be resent: resending an
/// older answer would put it after a newer one.
pub fn deliveries(
    all: &[delivery::Delivery],
    live: &[model::Session],
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<api::Delivery> {
    let mut v: Vec<_> = all.iter().map(|d| delivery(d, live, now)).collect();
    let mut seen = std::collections::HashSet::new();
    for (d, raw) in v.iter_mut().zip(all).rev() {
        if (raw.state == DState::Failed && raw.superseded_by.is_none()) || raw.pending() {
            d.resendable &= seen.insert((&raw.context, &raw.task));
        }
    }
    v
}

fn delivery(d: &delivery::Delivery, live: &[model::Session], now: DateTime<Utc>) -> api::Delivery {
    let v = d.view(live.iter().find(|s| s.session_id == d.session_id), now);
    api::Delivery {
        id: d.id.clone(),
        context: d.context.clone(),
        task: d.task.clone(),
        text: d.text.clone(),
        route: d.route.clone(),
        session_id: (!d.session_id.is_empty()).then(|| d.session_id.clone()),
        pane: d.pane.clone(),
        state: match d.state {
            DState::Delivered => api::DeliveryState::Delivered,
            DState::Queued => api::DeliveryState::Queued,
            DState::Held => api::DeliveryState::Held,
            DState::NoteOnly => api::DeliveryState::NoteOnly,
            DState::Failed => api::DeliveryState::Failed,
        },
        detail: d.detail.clone(),
        created_at: d.created_at.clone(),
        sent_at: d.sent_at.clone(),
        received_at: d.received_at.clone(),
        received_via: d.received_via.clone(),
        superseded_by: d.superseded_by.clone(),
        resend_of: d.resend_of.clone(),
        decision: d.decision.clone(),
        kind: d.kind.clone(),
        resendable: v.alarm && (d.state == DState::Failed || d.pending()),
        label: v.label,
        alarm: v.alarm,
    }
}

pub fn source(r: &health::Row) -> api::Source {
    let at = |t: &DateTime<Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    api::Source {
        scope: r.scope.clone(),
        source: r.source.label().into(),
        degraded: r.degraded,
        state: r.state(),
        last_ok: r.health.last_ok.as_ref().map(at),
        last_error: r.health.last_error.as_ref().map(|e| api::SourceError {
            at: at(&e.at),
            kind: e.kind.clone(),
            key: e.key.clone(),
            message: e.message.clone(),
        }),
        data_at: r.health.data_at.as_ref().map(at),
    }
}

pub fn route(r: &Route, pane: impl Fn(&model::Session) -> String) -> api::Route {
    let to = |kind, s: &model::Session| api::Route {
        kind,
        session_id: Some(s.session_id.clone()),
        pane: Some(pane(s)),
        dialog: delivery::open_prompt(s),
        reason: None,
    };
    match r {
        Route::Session(s) => to(api::RouteKind::Session, s),
        Route::Orchestrator(s) => to(api::RouteKind::Orchestrator, s),
        Route::NoteOnly { reason } => api::Route {
            reason: Some(reason.clone()),
            ..Default::default()
        },
    }
}

/// `name` turns a transcript path into the reference clients see.
pub fn item(i: history::Item, name: impl Fn(&str) -> String) -> api::Item {
    api::Item {
        ts: i.ts,
        source: i.source,
        title: i.title,
        body: i.body,
        transcript: i.transcript.as_deref().map(name),
    }
}

pub fn entry(e: transcript::Entry, name: impl Fn(&str) -> String) -> api::Entry {
    api::Entry {
        kind: match e.kind {
            transcript::Kind::User => api::EntryKind::User,
            transcript::Kind::Assistant => api::EntryKind::Assistant,
            transcript::Kind::Tool => api::EntryKind::Tool,
            transcript::Kind::ToolResult => api::EntryKind::ToolResult,
        },
        ts: e.ts,
        head: e.head,
        body: e.body,
        label: e.label,
        link: e.link.map(|l| api::Link {
            transcript: name(&l.path),
            ts: l.ts,
        }),
        folded: e.folded,
    }
}
