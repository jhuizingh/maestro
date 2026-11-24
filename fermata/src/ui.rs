//! Rendering. Pure functions of `App` state.
use crate::app::{App, DecisionRow, Filter, Scope, View};
use crate::model::{Badge, Decision, DecisionStatus, JoinedTask};
use crate::sessions;
use crate::theme::{self, Marks};
use crate::util::{parse_ts, rel_age, truncate};
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};

/// Below this many columns the detail replaces its list; at or above it they sit side by side.
pub const WIDE: u16 = 140;

/// The wide layout's (list, detail) column widths, or None for the narrow layout.
pub fn split(width: u16) -> Option<(u16, u16)> {
    if width < WIDE {
        return None;
    }
    let list = (width * 3 / 10).clamp(40, 56);
    Some((list, width - list))
}

fn age(ts: Option<&str>) -> String {
    ts.and_then(parse_ts)
        .map(|t| rel_age(t, Utc::now()))
        .unwrap_or_else(|| "-".into())
}

fn pr_cell(t: &JoinedTask) -> String {
    match &t.pr {
        Some(p) => {
            let st = match (p.state.as_str(), p.is_draft) {
                ("OPEN", true) => "draft",
                ("OPEN", false) => "open",
                ("MERGED", _) => "merged",
                ("CLOSED", _) => "closed",
                _ => "?",
            };
            format!("#{} {st} {}", p.number, p.checks.summary())
        }
        None if t.pr_error.is_some() => "pr error".into(),
        None => "-".into(),
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.width = f.area().width;
    let [head, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .areas(f.area());
    draw_header(f, app, head);
    match app.view {
        View::Tasks => draw_tasks(f, app, body),
        View::Detail => draw_detail(f, app, body),
        View::Decisions => draw_decisions(f, app, body),
        View::Sessions => draw_sessions(f, app, body),
        View::Viewer => draw_viewer(f, app, body),
        View::Help => draw_help(f, body),
        View::Contexts => draw_contexts(f, app, body),
        View::Health => draw_health(f, app, body),
    }
    draw_footer(f, app, foot);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let tab = |v: View, name: &str| {
        let on = app.view == v || (v == View::Tasks && app.view == View::Detail);
        Span::styled(
            format!(" {name} "),
            if on { theme::tab_on() } else { theme::muted() },
        )
    };
    let needs = app.tasks().iter().filter(|t| t.needs_you()).count();
    let mut spans = vec![
        Span::styled("fermata ", theme::bold()),
        tab(View::Tasks, "t Tasks"),
        tab(
            View::Decisions,
            &format!("d Decisions ({})", app.open_decisions()),
        ),
        tab(
            View::Sessions,
            &format!("s Sessions ({})", app.live_sessions()),
        ),
        Span::raw("  "),
        Span::styled(format!("{} ", scope_label(app)), theme::accent()),
        Span::styled(format!("{}  ", owner_label(app)), theme::muted()),
    ];
    if needs > 0 {
        spans.push(Span::styled(format!("{needs} need you "), theme::alert()));
    }
    let unknown = app.state_unknown();
    if unknown > 0 {
        spans.push(Span::styled(
            format!("{unknown} state unknown "),
            theme::badge(Badge::Unknown),
        ));
    }
    let backlog = app.backlog(None);
    if backlog > 0 {
        spans.push(Span::styled(format!("backlog {backlog} "), theme::muted()));
    }
    let elsewhere = app.needs_elsewhere();
    if elsewhere > 0 {
        spans.push(Span::styled(
            format!("+{elsewhere} elsewhere "),
            Style::default().fg(theme::ALERT),
        ));
    }
    let stale = &app.snap.tracker_stale;
    match stale.iter().next() {
        Some((c, st)) if stale.len() == 1 => spans.push(Span::styled(
            format!("⚠ {c} tasks as of {} ", crate::util::short_ts(&st.as_of)),
            theme::alert(),
        )),
        Some(_) => spans.push(Span::styled(
            format!("⚠ {} contexts stale ", stale.len()),
            theme::alert(),
        )),
        None => {}
    }
    let degraded = app.loader.health.degraded();
    if degraded > 0 {
        spans.push(Span::styled(
            format!("⚠ {} degraded · H ", plural(degraded, "source", "sources")),
            theme::alert(),
        ));
    }
    if app.dry_run {
        spans.push(Span::styled(" DRY-RUN ", theme::banner()));
    }
    if app.loading {
        spans.push(Span::styled(" loading… ", Style::default().fg(theme::WARN)));
    } else if !app.snap.loaded_at.is_empty() {
        spans.push(Span::styled(
            format!(" refreshed {} ago ", age(Some(&app.snap.loaded_at))),
            theme::dim(),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [l1, l2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    if let Some(inp) = &app.input {
        let line = Line::from(vec![
            Span::styled(format!("{} › ", inp.label), theme::accent()),
            Span::raw(inp.buf.clone()),
            Span::styled("█", theme::accent()),
        ]);
        f.render_widget(Paragraph::new(line), l1);
        f.render_widget(
            Paragraph::new("Enter send · Esc cancel · Ctrl-U clear").style(theme::dim()),
            l2,
        );
        return;
    }
    if let Some(flt) = app.filter().filter(|f| f.editing) {
        f.render_widget(Paragraph::new(format!("/{}█", flt.query)), l1);
        f.render_widget(
            Paragraph::new("type to filter · Enter lock · Esc clear · Up/Down move · Ctrl-U clear")
                .style(theme::dim()),
            l2,
        );
        return;
    }
    if let (View::Viewer, Some(v)) = (app.view, app.viewer.as_ref())
        && v.searching
    {
        f.render_widget(Paragraph::new(format!("/{}█", v.query)), l1);
    } else {
        let st = if app.confirm.is_some() {
            theme::banner()
        } else {
            Style::default()
        };
        f.render_widget(Paragraph::new(app.status.clone()).style(st), l1);
    }
    let keys = match app.view {
        View::Tasks => {
            "j/k move · / filter · Enter detail · o/e/p/c open ticket/epic/PR/CI · w session · : input · * / + pin · r refresh · u mine/everyone · M mode · C context · ? help · q quit"
        }
        View::Detail if app.wide() && app.list_focus => {
            "j/k list · Tab/Enter timeline · o/e/p/c open · w session · a/: answer · Q ask · A resend · 1-9 option · r refresh · * / + pin · Esc back"
        }
        View::Detail if app.wide() => {
            "j/k timeline · J/K item text · Enter open · Tab list · o/e/p/c open · w session · a/: answer · Q ask · A resend · 1-9 option · r refresh · Esc back"
        }
        View::Detail => {
            "o/e/p/c open · w session · a/: answer · Q ask · A resend · 1-9 option · j/k timeline · J/K item text · Enter open item · r refresh · * / + pin · Esc back"
        }
        View::Decisions => {
            "j/k move · / filter · 1-9 pick option · a answer · Q ask · Enter detail · o/p open · Esc back"
        }
        View::Sessions => {
            "0-9/M-a..z jump · / filter · Enter jump/reopen · r reply · e describe · x close · X closed list · S snapshot · L restore · l log · v transcript · o task"
        }
        View::Viewer => {
            "j/k/Space/b scroll · Enter expand tool · / search · n/N next/prev · t transcript · g/G top/end · Esc back"
        }
        View::Help => "Esc back",
        View::Health => "R re-ask every source · Esc back",
        View::Contexts => "j/k move · Enter show only this context · Esc back",
    };
    let keys = format!("{keys} · m mouse {}", if app.mouse { "on" } else { "off" });
    f.render_widget(Paragraph::new(keys).style(theme::dim()), l2);
}

/// A list's title: the count, as `shown/total` plus the filter when one is on.
fn list_title(name: &str, shown: usize, total: usize, flt: &Filter) -> String {
    if flt.is_on() {
        format!(" {name} ({shown}/{total}) · /{} ", flt.query)
    } else {
        format!(" {name} ({total}) ")
    }
}

/// The scope, as the header and the list titles name it.
fn scope_label(app: &App) -> String {
    match &app.scope {
        Scope::All => "all contexts".into(),
        Scope::One(c) => format!("context: {c}"),
    }
}

/// Whose tasks: `everyone`, or `mine` with what the filter hides and where it cannot apply.
fn owner_label(app: &App) -> String {
    if app.everyone {
        return "· everyone".into();
    }
    let mut s = "· mine".to_string();
    let hidden = app.hidden(None);
    if hidden > 0 {
        s.push_str(&format!(" · {hidden} hidden"));
    }
    for c in app.unknown_me() {
        s.push_str(&format!(" · everyone in {c} (who you are is unknown)"));
    }
    s
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn heading_text(name: &str, counts: &str) -> String {
    format!("── {name} · {counts} ──")
}

/// Rows for a scoped list. In all-contexts mode each context's block (the items arrive
/// grouped) opens with a heading; the second value is the row index of item `sel`.
fn with_headings<T: ?Sized, R>(
    app: &App,
    items: &[&T],
    ctx_of: impl Fn(&T) -> Option<String>,
    heading: impl Fn(&str, &[&T]) -> R,
    row: impl Fn(&T) -> R,
    sel: usize,
) -> (Vec<R>, usize) {
    let (mut out, mut at, mut i) = (vec![], sel, 0);
    while i < items.len() {
        let c = ctx_of(items[i]);
        let j = i + items[i..].iter().take_while(|x| ctx_of(x) == c).count();
        if app.grouped() {
            if i <= sel {
                at += 1;
            }
            out.push(heading(c.as_deref().unwrap_or("other"), &items[i..j]));
        }
        out.extend(items[i..j].iter().map(|x| row(x)));
        i = j;
    }
    (out, at)
}

/// A table heading row: the text in column `at`, a rule through every other column.
fn heading_row(text: String, cols: usize, at: usize) -> Row<'static> {
    let rule = "─".repeat(200);
    Row::new((0..cols).map(|i| {
        if i == at {
            Cell::from(Line::from(vec![
                Span::styled(text.clone(), theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(format!(" {rule}"), theme::dim()),
            ]))
        } else {
            Cell::from(rule.clone()).style(theme::dim())
        }
    }))
}

fn heading_item(text: String) -> ListItem<'static> {
    ListItem::new(Line::from(Span::styled(
        text,
        theme::accent().add_modifier(Modifier::BOLD),
    )))
}

fn task_counts(ts: &[&JoinedTask], hidden: usize, backlog: usize) -> String {
    let needs = ts.iter().filter(|t| t.needs_you()).count();
    let unknown = ts.iter().filter(|t| t.badge == Badge::Unknown).count();
    let mut s = plural(ts.len(), "task", "tasks");
    if needs > 0 {
        s.push_str(&format!(" · {needs} need you"));
    }
    if unknown > 0 {
        s.push_str(&format!(" · {unknown} state unknown"));
    }
    if hidden > 0 {
        s.push_str(&format!(" · {hidden} hidden"));
    }
    if backlog > 0 {
        s.push_str(&format!(" · backlog {backlog}"));
    }
    s
}

/// The task rows shown in a list column (the decisions list or the wide detail's column).
fn task_list(app: &App, ts: &[&JoinedTask], sel: usize) -> (Vec<ListItem<'static>>, usize) {
    with_headings(
        app,
        ts,
        |t| Some(t.context.clone()),
        |c, b| {
            heading_item(heading_text(
                c,
                &task_counts(b, app.hidden(Some(c)), app.backlog(Some(c))),
            ))
        },
        |t| list_row(t, &app.marks),
        sel,
    )
}

/// How long after an answer the orchestrator not acting on it is shown as a warning.
const ANSWER_WAIT_WARN: chrono::Duration = chrono::Duration::minutes(30);

/// A decision's state for display: who it is waiting on, and for how long.
fn decision_state(d: &Decision, now: chrono::DateTime<Utc>) -> (String, Style) {
    let age = |ts: &Option<String>| ts.as_deref().and_then(parse_ts);
    let ago = |ts: &Option<String>| age(ts).map(|t| format!(" {} ago", rel_age(t, now)));
    match d.status {
        DecisionStatus::Answered => {
            let late = age(&d.answered_at).is_some_and(|t| now - t >= ANSWER_WAIT_WARN);
            let st = if late {
                Style::default().fg(theme::WARN)
            } else {
                theme::muted()
            };
            let a = ago(&d.answered_at).unwrap_or_default();
            (format!("answered{a}, waiting on the orchestrator"), st)
        }
        DecisionStatus::Resolved => ("resolved".into(), theme::dim()),
        DecisionStatus::Open if d.pending_question.is_some() => (
            "open · your question is waiting on a response".into(),
            Style::default().fg(theme::WARN),
        ),
        DecisionStatus::Open if d.id.is_none() => ("open · legacy note".into(), theme::alert()),
        DecisionStatus::Open => {
            let a = ago(&d.asked_at)
                .map(|a| format!(", asked{a}"))
                .unwrap_or_default();
            (format!("open{a}"), theme::alert())
        }
    }
}

/// Decision rows grouped like the task list: the task, then the question and its state.
fn decision_list(app: &App, ds: &[DecisionRow], sel: usize) -> (Vec<ListItem<'static>>, usize) {
    let refs: Vec<&DecisionRow> = ds.iter().collect();
    let now = Utc::now();
    with_headings(
        app,
        &refs,
        |r| Some(r.task.context.clone()),
        |c, b| heading_item(heading_text(c, &plural(b.len(), "decision", "decisions"))),
        |r| {
            let (t, d) = (r.task, r.decision);
            let (state, st) = decision_state(d, now);
            let dot = if d.is_open() {
                theme::alert()
            } else {
                theme::muted()
            };
            let mut spans = vec![
                Span::styled(" ● ", dot),
                Span::styled(
                    format!("{} ", t.task.id),
                    theme::id().add_modifier(Modifier::BOLD),
                ),
            ];
            spans.extend(app.marks.spans(&truncate(&d.text, 60), Style::default()));
            if !d.is_open() || d.pending_question.is_some() {
                spans.push(Span::styled(format!("  {state}"), st));
            }
            ListItem::new(Line::from(spans))
        },
        sel,
    )
}

/// The context picker: one row per context, with what it holds.
fn draw_contexts(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .snap
        .contexts
        .iter()
        .map(|c| {
            let ts: Vec<&JoinedTask> = app
                .snap
                .tasks
                .iter()
                .filter(|t| t.context == c.name)
                .collect();
            // Shown under the current mine/everyone setting, with what it hides.
            let hidden = app.hidden(Some(&c.name));
            let shown = match ts.len() - hidden {
                n if hidden > 0 => format!("{n} (+{hidden})"),
                n => n.to_string(),
            };
            let sessions = app
                .snap
                .sessions
                .iter()
                .filter(|s| app.session_context(s) == Some(c.name.as_str()))
                .count();
            let cur = app.scope == Scope::One(c.name.clone());
            let needs = ts.iter().filter(|t| t.needs_you()).count();
            Row::new(vec![
                Cell::from(if cur { "•" } else { " " }).style(theme::accent()),
                Cell::from(c.name.clone()).style(theme::bold()),
                Cell::from(shown),
                Cell::from(if needs > 0 {
                    needs.to_string()
                } else {
                    "-".into()
                })
                .style(if needs > 0 {
                    theme::alert()
                } else {
                    theme::muted()
                }),
                Cell::from(
                    ts.iter()
                        .map(|t| t.open_decisions().count())
                        .sum::<usize>()
                        .to_string(),
                ),
                Cell::from(sessions.to_string()),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(1),
        Constraint::Min(16),
        Constraint::Length(11),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(9),
    ];
    let header = Row::new(["", "context", "tasks", "need you", "decisions", "sessions"])
        .style(theme::header_row());
    let mut st = TableState::default().with_selected(Some(app.csel));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .block(
                Block::bordered()
                    .border_style(theme::border(true))
                    .title(format!(
                        " Contexts ({}) — pick one to show only it ",
                        app.snap.contexts.len()
                    )),
            )
            .row_highlight_style(theme::selected())
            .highlight_symbol("› "),
        area,
        &mut st,
    );
}

fn draw_tasks(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.marks;
    let shown = app.visible_tasks();
    let row = |t: &JoinedTask| {
        let pin = if t.pin_rank.is_some() { "★" } else { " " };
        let check = t
            .progress
            .checklist
            .map(|(d, n)| format!("{d}/{n}"))
            .unwrap_or_default();
        Row::new(vec![
            Cell::from(pin).style(theme::pin()),
            Cell::from(t.badge.label()).style(theme::badge(t.badge)),
            Cell::from(t.task.id.clone()).style(theme::id()),
            Cell::from(Line::from(m.spans(&title_cell(t), Style::default()))),
            Cell::from(t.ticket_key.clone().unwrap_or_default()).style(theme::id()),
            Cell::from(Line::from(m.spans(&pr_cell(t), Style::default()))),
            Cell::from(check),
            Cell::from(age(t.last_change.as_deref())),
        ])
    };
    let (rows, at) = with_headings(
        app,
        &shown,
        |t| Some(t.context.clone()),
        |c, b| {
            heading_row(
                heading_text(
                    c,
                    &task_counts(b, app.hidden(Some(c)), app.backlog(Some(c))),
                ),
                8,
                3,
            )
        },
        row,
        app.sel,
    );
    let widths = [
        Constraint::Length(1),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Min(20),
        Constraint::Length(11),
        Constraint::Length(20),
        Constraint::Length(6),
        Constraint::Length(4),
    ];
    let header = Row::new(["", "state", "task", "title", "ticket", "PR", "list", "age"])
        .style(theme::header_row());
    let name = format!("Tasks · {}", scope_label(app));
    let title = if app.tasks().is_empty() && !app.loading {
        format!(" {name} — none in flight ")
    } else {
        list_title(&name, shown.len(), app.tasks().len(), &app.tfilter)
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut st = TableState::default().with_selected((!shown.is_empty()).then_some(at));
    f.render_stateful_widget(table, area, &mut st);
}

/// A parked or overdue row carries what it waits on after the title; a review, why it waits or
/// why its state is unknown.
fn title_cell(t: &JoinedTask) -> String {
    if let Some(why) = &t.review_waiting {
        return format!("{} — {why}", t.task.title);
    }
    if let Some(why) = &t.review_unknown {
        return format!("{} — review status unknown: {why}", t.task.title);
    }
    let mut tail: Vec<String> = vec![];
    if let Some(st) = &t.task.state {
        tail.push(st.text());
    }
    match t.waiting_text() {
        Some(w) if matches!(t.badge, Badge::Parked | Badge::Overdue) => tail.push(w),
        _ => {}
    }
    if tail.is_empty() {
        t.task.title.clone()
    } else {
        format!("{} — {}", t.task.title, tail.join(" · "))
    }
}

fn detail_lines(
    t: &JoinedTask,
    focus: Option<&str>,
    tasks_stale: Option<&crate::model::Stale>,
    router: &crate::routing::Router,
    m: &Marks,
) -> Vec<Line<'static>> {
    let kv = |k: &str, v: String, vs: Style| -> Line<'static> {
        let mut spans = vec![Span::styled(format!("{k:>9} "), theme::dim())];
        spans.extend(m.spans(&v, vs));
        Line::from(spans)
    };
    let mut l: Vec<Line<'static>> = vec![];
    let plain = Style::default();
    l.push(Line::from(vec![
        Span::styled(
            format!("{}  ", t.task.id),
            theme::id().add_modifier(Modifier::BOLD),
        ),
        Span::styled(t.badge.label().to_string(), theme::badge(t.badge)),
        Span::raw(format!("  [{}]", t.context)),
        Span::styled(
            if t.pin_rank.is_some() {
                "  ★ pinned"
            } else {
                ""
            }
            .to_string(),
            theme::pin(),
        ),
    ]));
    l.push(Line::from(m.spans(&t.task.title, plain)));
    if let Some(st) = tasks_stale {
        l.push(kv("tasks", truncate(&st.text(), 100), theme::muted()));
    }
    if let Some(st) = &t.task.state {
        let mut v = st.text();
        if st.by.is_some() || st.at.is_some() {
            v.push_str(&format!(
                "  (set{}{})",
                st.by
                    .as_deref()
                    .map(|b| format!(" by {b}"))
                    .unwrap_or_default(),
                st.at
                    .as_deref()
                    .map(|a| format!(" {}", crate::util::short_ts(a)))
                    .unwrap_or_default()
            ));
        }
        l.push(kv("state", v, theme::badge(st.badge())));
    }
    if t.kept_open() {
        let w = t
            .waiting_text()
            .unwrap_or_else(|| "no reason recorded".into());
        let st = theme::badge(t.badge);
        l.push(kv("parked", w, st));
    }
    let ticket = match (&t.ticket_key, &t.ticket, &t.ticket_error) {
        (Some(k), Some(i), _) => format!(
            "{k} — {} [{}]",
            i.title.clone().unwrap_or_default(),
            i.status.clone().unwrap_or_default()
        ),
        (Some(k), None, Some(e)) => format!("{k} ({})", truncate(&e.to_string(), 80)),
        (Some(k), None, None) => k.clone(),
        _ => "-".into(),
    };
    l.push(kv("ticket", ticket, plain));
    if let (Some(_), Some(st)) = (&t.ticket, &t.ticket_stale) {
        l.push(kv("", truncate(&st.text(), 100), theme::muted()));
    }
    if let Some(e) = t.ticket.as_ref().and_then(|i| i.epic.as_ref()) {
        l.push(kv(
            "epic",
            format!("{} — {}", e.key, e.title.clone().unwrap_or_default()),
            plain,
        ));
    }
    let pr = match (&t.pr, &t.pr_error) {
        (Some(p), _) => format!(
            "{} · {}{} · review {} · checks {}",
            pr_cell(t),
            p.state,
            if p.is_draft { " (draft)" } else { "" },
            p.review_decision.clone().unwrap_or_else(|| "-".into()),
            p.checks.summary()
        ),
        (None, Some(e)) => format!("error: {}", truncate(e, 80)),
        _ => "-".into(),
    };
    l.push(kv("PR", pr, plain));
    if let (Some(_), Some(st)) = (&t.pr, &t.pr_stale) {
        l.push(kv("", truncate(&st.text(), 100), theme::muted()));
    }
    if let Some(st) = &t.review_stale {
        l.push(kv("review", truncate(&st.text(), 100), theme::muted()));
    }
    for b in &t.branches {
        l.push(kv(
            "branch",
            format!("{}:{} [{}] {}", b.repo, b.branch, b.status, b.worktree),
            theme::muted(),
        ));
    }
    if let Some(s) = &t.session {
        l.push(kv(
            "session",
            format!(
                "{} {} — {}",
                router.pane_label(s),
                s.status,
                s.status_message.clone().unwrap_or_default()
            ),
            plain,
        ));
    }
    if let Some(x) = t.ticket.as_ref() {
        for e in &x.extra_links {
            l.push(kv("link", format!("{} {}", e.label, e.url), plain));
        }
    }
    let now = Utc::now();
    let many = t.decisions.len() > 1;
    for d in &t.decisions {
        let (state, st) = decision_state(d, now);
        l.push(Line::from(""));
        let mark = if many && focus == Some(d.key()) {
            "▶ "
        } else {
            ""
        };
        let head = match &d.id {
            Some(id) => format!("{mark}DECISION {id}"),
            None if d.label_missing => format!("{mark}DECISION (label missing: no needs-decision)"),
            None => format!("{mark}DECISION"),
        };
        let hs = if d.is_open() {
            theme::alert()
        } else {
            theme::muted()
        };
        l.push(Line::from(vec![
            Span::styled(head, hs),
            Span::styled(format!("  {state}"), st),
        ]));
        l.push(Line::from(m.spans(&d.text, plain)));
        for (i, o) in d.options.iter().enumerate() {
            l.push(Line::from(
                m.spans(&format!("  {}. {o}", i + 1), theme::accent()),
            ));
        }
        if let Some(r) = &d.recommendation {
            let good = Style::default().fg(theme::GOOD);
            l.push(Line::from(m.spans(&format!("  rec: {r}"), good)));
        }
        if let Some(q) = &d.pending_question {
            l.push(Line::from(
                m.spans(&format!("  you asked: {q}"), theme::muted()),
            ));
        }
        if let Some(a) = &d.answer {
            l.push(Line::from(
                m.spans(&format!("  your answer: {a}"), theme::muted()),
            ));
        }
    }
    let p = &t.progress;
    if p.status_line.is_some() || p.checklist.is_some() {
        l.push(Line::from(""));
        if let Some(s) = &p.status_line {
            l.push(kv("status", s.clone(), plain));
        }
        if let Some((d, n)) = p.checklist {
            l.push(kv("checklist", format!("{d}/{n}"), plain));
        }
        for d in &p.done {
            l.push(kv("done", d.clone(), theme::dim()));
        }
        for n in &p.next {
            l.push(kv("next", n.clone(), plain));
        }
    }
    l
}

/// How many of a task's routed answers the detail lists.
const ANSWERS_SHOWN: usize = 3;

/// The task's latest routed answers and what became of each; see "Answer delivery".
fn answer_lines(app: &App, task: &str) -> Vec<Line<'static>> {
    let ds = app.deliveries_for(task);
    if ds.is_empty() {
        return vec![];
    }
    let at = chrono::Utc::now();
    let mut l = vec![Line::from("")];
    for d in ds.iter().take(ANSWERS_SHOWN) {
        let v = d.view(app.live_session(&d.session_id), at);
        let st = if v.alarm {
            theme::alert()
        } else {
            theme::muted()
        };
        let when = chrono::DateTime::parse_from_rfc3339(&d.created_at)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_default();
        let resend = if v.alarm { " · A resends" } else { "" };
        l.push(Line::from(vec![
            Span::styled(format!("{:>9} ", "answer"), theme::dim()),
            Span::raw(format!("{when} {} ", truncate(&d.text, 50))),
            Span::styled(format!("— {}{resend}", v.label), st),
        ]));
    }
    if ds.len() > ANSWERS_SHOWN {
        l.push(Line::styled(
            format!("{:>9} +{} earlier", "", ds.len() - ANSWERS_SHOWN),
            theme::dim(),
        ));
    }
    l
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let (list_area, area) = match split(area.width) {
        Some((l, _)) => {
            let [a, b] =
                Layout::horizontal([Constraint::Length(l), Constraint::Min(1)]).areas(area);
            (Some(a), b)
        }
        None => (None, area),
    };
    // Focus only shows in the wide layout, where there is something to choose between.
    let (list_focus, detail_focus) = match list_area {
        Some(_) => (app.list_focus, !app.list_focus),
        // Narrow: j/k always drive the timeline.
        None => (false, true),
    };
    if let Some(la) = list_area {
        draw_list_column(f, app, la, list_focus);
    }
    let Some(t) = app.current() else {
        f.render_widget(Paragraph::new("task no longer in progress — Esc"), area);
        return;
    };
    let m = &app.marks;
    let focus = app.current_decision().map(|d| d.key().to_string());
    let stale = app.snap.tracker_stale.get(&t.context);
    let mut lines = detail_lines(t, focus.as_deref(), stale, &app.router, m);
    lines.extend(answer_lines(app, &t.task.id));
    let h = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(5))
        .max(5);
    // Leave the timeline and its preview at least half the height.
    let h = h.min((area.height / 2).max(5));
    let [top, bot] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(area);
    let [bot, preview] =
        Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(bot);
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Task ")),
        top,
    );
    let items: Vec<ListItem> = app
        .timeline
        .iter()
        .map(|i| {
            let mut spans = vec![
                Span::styled(
                    format!(
                        "{:<11} ",
                        i.ts.as_deref()
                            .map(crate::util::short_ts)
                            .unwrap_or_default()
                    ),
                    theme::dim(),
                ),
                Span::styled(format!("{:<14} ", i.source), theme::accent()),
            ];
            spans.extend(m.spans(&i.title, Style::default()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::bordered()
                .border_style(theme::border(detail_focus))
                .title(format!(" Timeline ({}) ", app.timeline.len())),
        )
        .highlight_style(theme::selected());
    let mut st = ListState::default().with_selected(Some(app.tsel));
    f.render_stateful_widget(list, bot, &mut st);
    draw_preview(f, app, preview);
}

/// The full text of the highlighted timeline item, wrapped; `J`/`K` scroll it.
fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    let Some(i) = app.timeline.get(app.tsel) else {
        f.render_widget(Block::bordered().title(" Item "), area);
        return;
    };
    let body = if i.body.trim().is_empty() {
        i.transcript.clone().unwrap_or_default()
    } else {
        i.body.clone()
    };
    // A title that is just the body's start, shortened, is not repeated above it.
    let short = i.title.trim().trim_end_matches('…').trim_end();
    let mut lines = vec![];
    if body.trim().is_empty() || !body.trim_start().starts_with(short) {
        lines.push(Line::from(app.marks.spans(&i.title, theme::bold())));
        lines.push(Line::from(""));
    }
    lines.extend(
        body.lines()
            .map(|l| Line::from(app.marks.spans(l, Style::default()))),
    );
    let ts =
        i.ts.as_deref()
            .map(crate::util::short_ts)
            .unwrap_or_default();
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.pscroll, 0))
            .block(
                Block::bordered()
                    .title(format!(" {} {ts} ", i.source))
                    .title_bottom(" J/K scroll · Enter open "),
            ),
        area,
    );
}

/// One compact row of a list column: pin, badge dot, id, title.
fn list_row(t: &JoinedTask, m: &Marks) -> ListItem<'static> {
    let mut spans = vec![
        Span::styled(if t.pin_rank.is_some() { "★" } else { " " }, theme::pin()),
        Span::styled("● ", theme::badge(t.badge)),
        Span::styled(
            format!("{} ", t.task.id),
            theme::id().add_modifier(Modifier::BOLD),
        ),
    ];
    spans.extend(m.spans(&t.task.title, Style::default()));
    ListItem::new(Line::from(spans))
}

/// The wide detail's left column: the list the detail was opened from.
fn draw_list_column(f: &mut Frame, app: &App, area: Rect, focused: bool) {
    let (items, sel, title): (Vec<ListItem>, usize, String) = match app.detail_from {
        View::Decisions => {
            let ds = app.visible_decisions();
            let name = format!("Decisions · {}", scope_label(app));
            let title = list_title(&name, ds.len(), app.decisions().len(), &app.dfilter);
            let (items, at) = decision_list(app, &ds, app.dsel);
            (items, at, title)
        }
        _ => {
            let ts = app.origin_list();
            let name = format!("Tasks · {}", scope_label(app));
            let title = list_title(&name, ts.len(), app.tasks().len(), &app.tfilter);
            let (items, at) = task_list(app, &ts, app.sel);
            (items, at, title)
        }
    };
    let mut st = ListState::default().with_selected(Some(sel));
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered()
                    .border_style(theme::border(focused))
                    .title(title),
            )
            .highlight_style(theme::selected()),
        area,
        &mut st,
    );
}

fn draw_decisions(f: &mut Frame, app: &App, area: Rect) {
    let total = app.decisions().len();
    let ds = app.visible_decisions();
    let name = format!("Decisions · {}", scope_label(app));
    if total == 0 {
        f.render_widget(
            Paragraph::new("No decisions.").block(Block::bordered().title(format!(" {name} "))),
            area,
        );
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
    let (items, at) = decision_list(app, &ds, app.dsel);
    let mut st = ListState::default().with_selected((!ds.is_empty()).then_some(at));
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered()
                    .border_style(theme::border(true))
                    .title(list_title(&name, ds.len(), total, &app.dfilter)),
            )
            .highlight_style(theme::selected()),
        left,
        &mut st,
    );
    if let Some(r) = ds.get(app.dsel) {
        let t = r.task;
        f.render_widget(
            Paragraph::new({
                let mut l = detail_lines(
                    t,
                    Some(r.decision.key()),
                    app.snap.tracker_stale.get(&t.context),
                    &app.router,
                    &app.marks,
                );
                l.extend(answer_lines(app, &t.task.id));
                l
            })
            .wrap(Wrap { trim: false })
            .block(Block::bordered()),
            right,
        );
    }
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.marks;
    let shown = app.sessions_sorted();
    let n = shown.len();
    let row =
        |s: &crate::model::Session| {
            let st = match s.status.as_str() {
                "needs_input" | "needs_permission" => Style::default().fg(theme::ALERT),
                "working" => Style::default().fg(theme::WARN),
                _ => theme::muted(),
            };
            let repo = std::path::Path::new(&s.worktree)
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            let task =
                app.task_for_session(s)
                    .map(|t| t.task.id.clone())
                    .unwrap_or_else(|| {
                        if app.snap.contexts.iter().any(|c| {
                            sessions::same_path(&s.worktree, &c.workspace.to_string_lossy())
                        }) {
                            "orchestrator".into()
                        } else {
                            String::new()
                        }
                    });
            let i = shown.iter().position(|x| std::ptr::eq(*x, s));
            Row::new(vec![
                Cell::from(i.map(crate::app::index_label).unwrap_or_default()).style(theme::dim()),
                Cell::from(if s.closed {
                    "closed".into()
                } else {
                    s.status.clone()
                })
                .style(st),
                Cell::from(if s.closed {
                    s.tmux_label().to_string()
                } else {
                    app.router.pane_label(s)
                }),
                Cell::from(repo),
                Cell::from(Line::from(
                    m.spans(&s.branch.clone().unwrap_or_default(), Style::default()),
                )),
                Cell::from(task.clone()).style(if task == "orchestrator" {
                    theme::muted()
                } else {
                    theme::id()
                }),
                Cell::from(Line::from(m.spans(
                    &s.description.clone().unwrap_or_default(),
                    Style::default(),
                ))),
                Cell::from(age(s.last_activity.as_deref().or(s.closed_at.as_deref()))),
                Cell::from(s.tool_calls.map(|n| n.to_string()).unwrap_or_default()),
            ])
        };
    let (rows, at) = with_headings(
        app,
        &shown,
        |s| app.session_context(s).map(String::from),
        |c, b| {
            heading_row(
                heading_text(c, &plural(b.len(), "session", "sessions")),
                9,
                2,
            )
        },
        row,
        app.ssel,
    );
    let widths = [
        Constraint::Length(3),
        Constraint::Length(16),
        Constraint::Length(32),
        Constraint::Length(30),
        Constraint::Length(28),
        Constraint::Length(12),
        Constraint::Min(12),
        Constraint::Length(4),
        Constraint::Length(6),
    ];
    let header = Row::new([
        "",
        "status",
        "tmux",
        "worktree",
        "branch",
        "task",
        "description",
        "age",
        "calls",
    ])
    .style(theme::header_row());
    let mut st = TableState::default().with_selected((n > 0).then_some(at));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(if app.show_closed {
                format!(
                    "{}— Enter reopen · e describe · X live ",
                    list_title(
                        &format!("Closed sessions · {}", scope_label(app)),
                        n,
                        app.session_total(),
                        &app.sfilter
                    )
                )
            } else {
                format!(
                    "{}— X closed ",
                    list_title(
                        &format!("Sessions · {}", scope_label(app)),
                        n,
                        app.session_total(),
                        &app.sfilter
                    )
                )
            }))
            .row_highlight_style(theme::selected()),
        area,
        &mut st,
    );
}

fn draw_viewer(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(v) = app.viewer.as_mut() else {
        return;
    };
    if v.width != area.width {
        v.rebuild(area.width);
    }
    let h = area.height.saturating_sub(2) as usize;
    v.fit(h);
    let matches: std::collections::HashSet<usize> = v.matches.iter().copied().collect();
    let lines: Vec<Line> = v
        .lines
        .iter()
        .enumerate()
        .skip(v.scroll)
        .take(h)
        .map(|(n, (text, i, head))| {
            let e = &v.entries[*i];
            let mut st = match (e.kind.clone(), head) {
                (crate::transcript::Kind::User, true) => Style::default()
                    .fg(theme::GOOD)
                    .add_modifier(Modifier::BOLD),
                (crate::transcript::Kind::Assistant, true) => Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD),
                (crate::transcript::Kind::Tool | crate::transcript::Kind::ToolResult, _) => {
                    theme::dim()
                }
                _ => Style::default(),
            };
            if matches.contains(&n) {
                st = st.patch(theme::banner());
            }
            if n == v.cursor {
                st = st.patch(theme::selected());
            }
            Line::styled(text.clone(), st)
        })
        .collect();
    let title = format!(
        " {} — {}/{}{} ",
        truncate(&v.title, 60),
        v.cursor + 1,
        v.lines.len(),
        if v.query.is_empty() {
            String::new()
        } else {
            format!(" · /{} ({} hits)", v.query, v.matches.len())
        }
    );
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(title)),
        area,
    );
}

/// The build, where fermata reads and writes, then one row per source.
fn draw_health(f: &mut Frame, app: &App, area: Rect) {
    use crate::health;
    let paths = health::paths(app.loader.baton.as_deref());
    let [top, table] = Layout::vertical([
        Constraint::Length(paths.len() as u16 + 3),
        Constraint::Min(3),
    ])
    .areas(area);
    let mut lines = vec![Line::styled(health::build_line(), theme::bold())];
    lines.extend(paths.iter().map(|(k, v)| {
        Line::from(vec![
            Span::styled(format!("{k:<6} "), theme::muted()),
            Span::raw(v.clone()),
        ])
    }));
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Diagnostics ")),
        top,
    );
    let now = Utc::now();
    let all = app.loader.health.rows();
    let degraded = all.iter().filter(|r| r.degraded).count();
    let rows: Vec<Row> = all
        .iter()
        .map(|r| {
            let c = health::cells(r, now);
            let state = if r.degraded {
                theme::alert()
            } else {
                theme::muted()
            };
            Row::new(vec![
                Cell::from(c[0].clone()).style(theme::bold()),
                Cell::from(c[1].clone()),
                Cell::from(c[2].clone()).style(state),
                Cell::from(c[3].clone()),
                Cell::from(c[4].clone()),
                Cell::from(c[5].clone()),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(12),
        Constraint::Length(15),
        Constraint::Length(20),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Min(20),
    ];
    let title = match degraded {
        0 => " Sources — all ok ".to_string(),
        n => format!(" Sources — {} degraded ", plural(n, "source", "sources")),
    };
    f.render_widget(
        Table::new(rows, widths)
            .header(Row::new(health::HEADERS).style(theme::header_row()))
            .block(Block::bordered().title(title)),
        table,
    );
}

fn draw_help(f: &mut Frame, area: Rect) {
    let text = "\
Views:   t tasks · d decisions · s sessions · H health · ? help · q quit · Esc back · R full reload
Scope:   M all contexts ↔ one context · C pick the context · [ ] previous/next context
         all contexts: each context's items in their own block, under a heading
         u mine ↔ everyone (Tasks): mine = assigned to you, unassigned, or waiting on you
Tasks:   j/k move · Enter detail · o ticket · e epic · p PR · c CI run · r reload
         w go to the task's session (else a new window in its worktree) · W new window in worktree
         : or i  type an instruction/answer for the selected task · * pin ticket · + pin epic
Detail:  same open keys · a answer · 1-9 pre-fill an option · j/k timeline · Enter open item · r refresh
         A resend the latest answer that failed or was not received
         J/K scroll the highlighted item's full text (the panel under the timeline)
         wide (140+ cols): list beside detail · j/k move the list · Tab/Enter timeline · Tab list
Decide:  1-9 pre-fill option N into the input bar · a free-form answer · Enter detail
Session: Enter jump (tmux switch-client) · r quick reply · l session log · v transcript · o its task
         0-9, M-a..M-z  act on the row with that key, as Enter (M- needs Option sent as Meta)
         e describe · x close (confirm: y kill claude + pane, k kill claude only) · X closed list
         (Enter there reopens with claude --resume, confirmed) · S save snapshot · L restore (confirmed)
Filter:  / on Tasks, Decisions or Sessions narrows the list as you type · Enter locks it
         Esc (typing or locked) shows the whole list again · Up/Down move while typing
Mouse:   m toggles mouse capture (off by default so text selection works)
Viewer:  j/k scroll · Enter expand/collapse tool call · / search · n/N next/prev · t open transcript

Routing: input goes to the task's live session pane if there is one, else to the orchestrator
pane prefixed [fermata <id>], and is always recorded as a tracker note (ANSWER …, with what
happened to it). A non-Claude pane whose session is 'working' asks for confirmation first. A
pane at a dialog (AskUserQuestion, permission) is not typed into: the answer is held and sent
when the dialog closes. The detail shows each answer's state; 'not yet received' means the
session never saw it (A resends). --dry-run logs instead of sending/noting.";
    f.render_widget(
        Paragraph::new(text)
            .block(Block::bordered().title(format!(" Help — {} ", crate::health::build_line()))),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tests::jt;

    #[test]
    fn pr_cells() {
        let mut t = jt("x");
        assert_eq!(pr_cell(&t), "-");
        t.pr = Some(crate::model::PrInfo {
            number: "7".into(),
            state: "OPEN".into(),
            is_draft: true,
            ..Default::default()
        });
        assert_eq!(pr_cell(&t), "#7 draft no checks");
    }

    #[test]
    fn layout_by_width() {
        assert_eq!(split(80), None);
        assert_eq!(split(WIDE - 1), None);
        assert_eq!(split(WIDE), Some((42, 98)));
        assert_eq!(split(160), Some((48, 112)));
        assert_eq!(split(240), Some((56, 184)));
    }

    fn screen(app: &mut App, w: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(w, 30)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn detail_layout_switches_with_width() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap.tasks = vec![jt("ctx-1"), jt("ctx-2")];
        app.view = View::Detail;
        app.detail_id = Some("ctx-2".into());
        app.sel = 1;
        let wide = screen(&mut app, 160);
        assert!(wide.contains("Tasks · all contexts (2)") && wide.contains("ctx-1"));
        let narrow = screen(&mut app, 120);
        assert!(!narrow.contains("Tasks · all contexts (2)") && !narrow.contains("ctx-1"));
        assert_eq!(app.width, 120);
        assert!(screen(&mut app, 160).contains("Tasks · all contexts (2)"));
        assert_eq!((app.detail_id.as_deref(), app.sel), (Some("ctx-2"), 1));
    }

    #[test]
    fn timeline_preview_shows_the_highlighted_item_in_full() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap.tasks = vec![jt("ctx-1")];
        app.view = View::Detail;
        app.detail_id = Some("ctx-1".into());
        let item = |title: &str, body: &str| crate::history::Item {
            ts: None,
            source: "note".into(),
            title: title.into(),
            body: body.into(),
            transcript: None,
            sort_ts: None,
        };
        app.timeline = vec![
            item("first", "first body line one\nfirst body line two"),
            item("second", "second body, much longer"),
        ];
        let s = screen(&mut app, 100);
        assert!(s.contains("first body line two") && !s.contains("second body"));
        app.tsel = 1;
        let s = screen(&mut app, 100);
        assert!(s.contains("second body, much longer") && !s.contains("first body"));
    }

    #[test]
    fn ids_and_prs_are_coloured_in_the_task_list() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        let mut t = jt("ctx-9");
        t.pr = Some(crate::model::PrInfo {
            number: "42".into(),
            state: "OPEN".into(),
            ..Default::default()
        });
        app.snap.tasks = vec![t];
        app.marks = Marks::from_tasks(&app.snap.tasks);
        let mut term = Terminal::new(TestBackend::new(140, 10)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let fg_of = |needle: &str| {
            let cells = buf.content();
            let n: Vec<String> = needle.chars().map(String::from).collect();
            let at = (0..cells.len())
                .find(|&i| {
                    n.iter()
                        .enumerate()
                        .all(|(k, ch)| cells.get(i + k).is_some_and(|c| c.symbol() == ch))
                })
                .unwrap();
            cells[at].fg
        };
        assert_eq!(fg_of("ctx-9"), theme::ID);
        assert_eq!(fg_of("#42"), theme::PR);
    }

    #[test]
    fn renders_without_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        let mut t = jt("ctx-1");
        t.badge = Badge::NeedsYou;
        t.decisions = vec![
            crate::model::Decision {
                text: "ship?".into(),
                options: vec!["yes".into()],
                recommendation: Some("yes".into()),
                label_missing: true,
                ..Default::default()
            },
            crate::model::Decision {
                id: Some("d0000001".into()),
                text: "rename?".into(),
                status: DecisionStatus::Answered,
                answered_at: Some("2026-01-01T00:00:00Z".into()),
                ..Default::default()
            },
        ];
        app.snap.tasks.push(t);
        let mut term = Terminal::new(TestBackend::new(120, 30)).unwrap();
        for v in [
            View::Tasks,
            View::Decisions,
            View::Sessions,
            View::Help,
            View::Detail,
        ] {
            app.view = v;
            app.detail_id = Some("ctx-1".into());
            term.draw(|f| draw(f, &mut app)).unwrap();
        }
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("DECISION"));
        assert!(text.contains("label missing"));
        assert!(text.contains("DECISION d0000001"));
        assert!(text.contains("waiting on the orchestrator"));

        app.loader.health.fail(
            "ctx",
            crate::health::Source::Tracker,
            "list",
            "timeout",
            "timed out",
        );
        app.view = View::Health;
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("⚠ 1 source degraded · H"));
        assert!(text.contains("Sources — 1 source degraded"));
        assert!(text.contains("timeout [list]: timed out"));
    }

    #[test]
    fn filter_shows_in_title_and_footer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap.tasks = vec![jt("ctx-1"), jt("ctx-2"), jt("other-3")];
        app.tfilter = Filter {
            query: "ctx".into(),
            editing: true,
        };
        let mut term = Terminal::new(TestBackend::new(160, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("Tasks · all contexts (2/3) · /ctx"));
        assert!(text.contains("/ctx█"));
        assert!(!text.contains("other-3"));
    }

    #[test]
    fn sessions_rows_start_with_their_index_key() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        app.snap.sessions = (0..12)
            .map(|i| crate::model::Session {
                session_id: format!("s-{i}"),
                branch: Some(format!("branch-{i:02}")),
                worktree: format!("/wt/{i}"),
                ..Default::default()
            })
            .collect();
        app.view = View::Sessions;
        let mut term = Terminal::new(TestBackend::new(160, 20)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let lines: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let row = |br: &str| lines.iter().find(|l| l.contains(br)).unwrap().clone();
        assert!(
            row("branch-00")
                .trim_start_matches(['│', ' '])
                .starts_with("0 ")
        );
        assert!(
            row("branch-09")
                .trim_start_matches(['│', ' '])
                .starts_with("9 ")
        );
        assert!(
            row("branch-11")
                .trim_start_matches(['│', ' '])
                .starts_with("M-b")
        );
    }

    fn two_contexts(app: &mut App) {
        let ctx = |n: &str| crate::config::Context {
            name: n.into(),
            workspace: format!("/ws/{n}").into(),
            tracker_dir: None,
            github_owner: None,
            home: None,
            member_repos: vec![],
            ctx_json: String::new(),
            adapter: None,
            adapter_error: None,
        };
        app.snap.contexts = vec![ctx("alpha"), ctx("beta")];
        let t = |id: &str, c: &str| {
            let mut t = jt(id);
            t.context = c.into();
            t
        };
        // Sorted by urgency, interleaving the contexts, as the loader leaves them.
        app.snap.tasks = vec![t("b-1", "beta"), t("a-1", "alpha"), t("b-2", "beta")];
    }

    #[test]
    fn all_contexts_mode_groups_tasks_under_headings() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        two_contexts(&mut app);
        let text = screen(&mut app, 160);
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} not shown"));
        assert!(text.contains("all contexts"));
        assert!(at("alpha · 1 task") < at("a-1"));
        assert!(at("a-1") < at("beta · 2 tasks"));
        assert!(at("beta · 2 tasks") < at("b-1") && at("b-1") < at("b-2"));
    }

    #[test]
    fn mine_mode_says_what_it_hides_and_where_it_cannot_apply() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        two_contexts(&mut app);
        app.snap.me.insert("alpha".into(), "Me".into());
        let mut theirs = jt("a-2");
        theirs.context = "alpha".into();
        theirs.task.assignee = Some("Someone".into());
        app.snap.tasks.push(theirs);
        let text = screen(&mut app, 200);
        assert!(text.contains("· mine · 1 hidden · everyone in beta (who you are is unknown)"));
        assert!(text.contains("alpha · 1 task · 1 state unknown · 1 hidden"));
        assert!(!text.contains("a-2"));
        app.everyone = true;
        let text = screen(&mut app, 200);
        assert!(text.contains("· everyone") && text.contains("a-2"));
        assert!(text.contains("alpha · 2 tasks · 2 state unknown ──"));
    }

    #[test]
    fn declared_state_and_backlog_are_shown() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        two_contexts(&mut app);
        let t = &mut app.snap.tasks[0];
        t.task.state = Some(crate::model::TaskState {
            state: "paused".into(),
            why: Some("user stopped the CI wait".into()),
            ..Default::default()
        });
        t.badge = Badge::Paused;
        app.snap.backlog.insert("alpha".into(), 7);
        let text = screen(&mut app, 220);
        assert!(text.contains("paused: user stopped the CI wait"));
        assert!(text.contains("backlog 7"));
    }

    #[test]
    fn one_context_mode_shows_only_that_context_without_headings() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut app = App::new(true, tx).unwrap();
        two_contexts(&mut app);
        app.set_scope(Scope::One("beta".into()));
        let text = screen(&mut app, 160);
        assert!(text.contains("context: beta") && text.contains("b-1") && text.contains("b-2"));
        assert!(!text.contains("a-1") && !text.contains("── beta"));
    }
}
