//! Rendering. Pure functions of `App` state.
use crate::app::{App, View};
use crate::model::{Badge, JoinedTask};
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
            &format!("d Decisions ({})", app.decisions().len()),
        ),
        tab(
            View::Sessions,
            &format!("s Sessions ({})", app.snap.sessions.len()),
        ),
        Span::raw("  "),
    ];
    if needs > 0 {
        spans.push(Span::styled(format!("{needs} need you "), theme::alert()));
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
            "j/k move · Enter detail · o/e/p/c open ticket/epic/PR/CI · w window · : input · * pin ticket · + pin epic · r refresh · ? help · q quit"
        }
        View::Detail if app.wide() && app.list_focus => {
            "j/k list · Tab/Enter timeline · o/e/p/c open · w window · a/: answer · 1-9 option · r refresh · * / + pin · Esc back"
        }
        View::Detail if app.wide() => {
            "j/k timeline · J/K item text · Enter open · Tab list · o/e/p/c open · w window · a/: answer · 1-9 option · r refresh · Esc back"
        }
        View::Detail => {
            "o/e/p/c open · w window · a/: answer · 1-9 option · j/k timeline · J/K item text · Enter open item · r refresh · * / + pin · Esc back"
        }
        View::Decisions => {
            "j/k move · 1-9 pick option · a answer · Enter detail · o/p open · Esc back"
        }
        View::Sessions => {
            "Enter jump/reopen · r reply · e describe · x close · X closed list · S snapshot · L restore · l log · v transcript · o task"
        }
        View::Viewer => {
            "j/k/Space/b scroll · Enter expand tool · / search · n/N next/prev · t transcript · g/G top/end · Esc back"
        }
        View::Help => "Esc back",
    };
    let keys = format!("{keys} · m mouse {}", if app.mouse { "on" } else { "off" });
    f.render_widget(Paragraph::new(keys).style(theme::dim()), l2);
}

fn draw_tasks(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.marks;
    let rows: Vec<Row> = app
        .tasks()
        .iter()
        .map(|t| {
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
        })
        .collect();
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
    let title = if app.tasks().is_empty() && !app.loading {
        " Tasks — none in progress ".to_string()
    } else {
        format!(" Tasks ({}) ", app.tasks().len())
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(theme::selected())
        .highlight_symbol("› ");
    let mut st = TableState::default().with_selected(Some(app.sel));
    f.render_stateful_widget(table, area, &mut st);
}

/// A parked or overdue row carries what it waits on after the title.
fn title_cell(t: &JoinedTask) -> String {
    match t.waiting_text() {
        Some(w) if matches!(t.badge, Badge::Parked | Badge::Overdue) => {
            format!("{} — {w}", t.task.title)
        }
        _ => t.task.title.clone(),
    }
}

fn detail_lines(t: &JoinedTask, router: &crate::routing::Router, m: &Marks) -> Vec<Line<'static>> {
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
        (Some(k), None, Some(e)) => format!("{k} ({e})"),
        (Some(k), None, None) => k.clone(),
        _ => "-".into(),
    };
    l.push(kv("ticket", ticket, plain));
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
    if let Some(d) = &t.decision {
        l.push(Line::from(""));
        l.push(Line::styled("DECISION", theme::alert()));
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
    let lines = detail_lines(t, &app.router, m);
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
            let ds = app.decisions();
            let title = format!(" Decisions ({}) ", ds.len());
            (
                ds.into_iter().map(|t| list_row(t, &app.marks)).collect(),
                app.dsel,
                title,
            )
        }
        _ => (
            app.tasks()
                .iter()
                .map(|t| list_row(t, &app.marks))
                .collect(),
            app.sel,
            format!(" Tasks ({}) ", app.tasks().len()),
        ),
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
    let ds = app.decisions();
    if ds.is_empty() {
        f.render_widget(
            Paragraph::new("Nothing is waiting on you.")
                .block(Block::bordered().title(" Decisions ")),
            area,
        );
        return;
    }
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(area);
    let items: Vec<ListItem> = ds.iter().map(|t| list_row(t, &app.marks)).collect();
    let mut st = ListState::default().with_selected(Some(app.dsel));
    f.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered()
                    .border_style(theme::border(true))
                    .title(format!(" Decisions ({}) ", ds.len())),
            )
            .highlight_style(theme::selected()),
        left,
        &mut st,
    );
    if let Some(t) = ds.get(app.dsel) {
        f.render_widget(
            Paragraph::new(detail_lines(t, &app.router, &app.marks))
                .wrap(Wrap { trim: false })
                .block(Block::bordered()),
            right,
        );
    }
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let m = &app.marks;
    let rows: Vec<Row> = app
        .sessions_sorted()
        .into_iter()
        .map(|s| {
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
            Row::new(vec![
                Cell::from(if s.closed {
                    "closed".into()
                } else {
                    s.status.clone()
                })
                .style(st),
                Cell::from(if s.closed {
                    s.tmux_target.clone().unwrap_or_default()
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
        })
        .collect();
    let widths = [
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
    let mut st = TableState::default().with_selected(Some(app.ssel));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(if app.show_closed {
                " Closed sessions (ccmon) — Enter reopen · e describe · X live "
            } else {
                " Sessions (ccmon) — X closed "
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

fn draw_help(f: &mut Frame, area: Rect) {
    let text = "\
Views:   t tasks · d decisions · s sessions · ? help · q quit · Esc back · R full reload
Tasks:   j/k move · Enter detail · o ticket · e epic · p PR · c CI run · w tmux window in worktree
         : or i  type an instruction/answer for the selected task · * pin ticket · + pin epic
Detail:  same open keys · a answer · 1-9 pre-fill an option · j/k timeline · Enter open item · r refresh
         J/K scroll the highlighted item's full text (the panel under the timeline)
         wide (140+ cols): list beside detail · j/k move the list · Tab/Enter timeline · Tab list
Decide:  1-9 pre-fill option N into the input bar · a free-form answer · Enter detail
Session: Enter jump (tmux switch-client) · r quick reply · l ccmon log · v transcript · o its task
         e describe · x close (confirm: y kill claude + pane, k kill claude only) · X closed list
         (Enter there reopens with claude --resume, confirmed) · S save snapshot · L restore (confirmed)
Mouse:   m toggles mouse capture (off by default so text selection works)
Viewer:  j/k scroll · Enter expand/collapse tool call · / search · n/N next/prev · t open transcript

Routing: input goes to the task's live session pane if there is one, else to the orchestrator
pane prefixed [fermata <id>], and is always recorded as a tracker note (ANSWER …). A pane whose
session is 'working' asks for confirmation first. --dry-run logs instead of sending/noting.";
    f.render_widget(
        Paragraph::new(text).block(Block::bordered().title(" Help ")),
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
        assert!(wide.contains(" Tasks (2) ") && wide.contains("ctx-1"));
        let narrow = screen(&mut app, 120);
        assert!(!narrow.contains(" Tasks (2) ") && !narrow.contains("ctx-1"));
        assert_eq!(app.width, 120);
        assert!(screen(&mut app, 160).contains(" Tasks (2) "));
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
        t.decision = Some(crate::model::Decision {
            text: "ship?".into(),
            options: vec!["yes".into()],
            recommendation: Some("yes".into()),
        });
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
    }
}
