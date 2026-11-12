//! Rendering. Pure functions of `App` state.
use crate::app::{App, View};
use crate::model::{Badge, JoinedTask};
use crate::sessions;
use crate::util::{parse_ts, rel_age, truncate};
use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};

fn badge_style(b: Badge) -> Style {
    let c = match b {
        Badge::NeedsYou => Color::Red,
        Badge::CiFailing => Color::LightRed,
        Badge::ReadyToMerge => Color::Green,
        Badge::WaitingOnCi => Color::Yellow,
        Badge::WorkerRunning => Color::Cyan,
        Badge::Interactive => Color::Blue,
        Badge::Parked => Color::DarkGray,
        Badge::Idle => Color::Gray,
    };
    Style::default()
        .fg(c)
        .add_modifier(if b == Badge::NeedsYou {
            Modifier::BOLD
        } else {
            Modifier::empty()
        })
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
            if on {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            },
        )
    };
    let needs = app.tasks().iter().filter(|t| t.needs_you()).count();
    let mut spans = vec![
        Span::styled("fermata ", Style::default().add_modifier(Modifier::BOLD)),
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
        spans.push(Span::styled(
            format!("{needs} need you "),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    if app.dry_run {
        spans.push(Span::styled(
            " DRY-RUN ",
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    }
    if app.loading {
        spans.push(Span::styled(
            " loading… ",
            Style::default().fg(Color::Yellow),
        ));
    } else if !app.snap.loaded_at.is_empty() {
        spans.push(Span::styled(
            format!(" refreshed {} ago ", age(Some(&app.snap.loaded_at))),
            Style::default().fg(Color::DarkGray),
        ));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let [l1, l2] = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    if let Some(inp) = &app.input {
        let line = Line::from(vec![
            Span::styled(
                format!("{} › ", inp.label),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw(inp.buf.clone()),
            Span::styled("█", Style::default().fg(Color::Cyan)),
        ]);
        f.render_widget(Paragraph::new(line), l1);
        f.render_widget(
            Paragraph::new("Enter send · Esc cancel · Ctrl-U clear")
                .style(Style::default().fg(Color::DarkGray)),
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
            Style::default().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::default()
        };
        f.render_widget(Paragraph::new(app.status.clone()).style(st), l1);
    }
    let keys = match app.view {
        View::Tasks => {
            "j/k move · Enter detail · o/e/p/c open ticket/epic/PR/CI · w window · : input · * pin ticket · + pin epic · r refresh · ? help · q quit"
        }
        View::Detail => {
            "o/e/p/c open · w window · a/: answer · 1-9 option · j/k timeline · Enter open item · r refresh · * / + pin · Esc back"
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
    f.render_widget(
        Paragraph::new(keys).style(Style::default().fg(Color::DarkGray)),
        l2,
    );
}

fn draw_tasks(f: &mut Frame, app: &App, area: Rect) {
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
                Cell::from(pin).style(Style::default().fg(Color::Yellow)),
                Cell::from(t.badge.label()).style(badge_style(t.badge)),
                Cell::from(t.task.id.clone()),
                Cell::from(t.task.title.clone()),
                Cell::from(t.ticket_key.clone().unwrap_or_default()),
                Cell::from(pr_cell(t)),
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
        .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let title = if app.tasks().is_empty() && !app.loading {
        " Tasks — none in progress ".to_string()
    } else {
        format!(" Tasks ({}) ", app.tasks().len())
    };
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(title))
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("› ");
    let mut st = TableState::default().with_selected(Some(app.sel));
    f.render_stateful_widget(table, area, &mut st);
}

fn kv<'a>(k: &'a str, v: String, vs: Style) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{k:>9} "), Style::default().fg(Color::DarkGray)),
        Span::styled(v, vs),
    ])
}

fn detail_lines(t: &JoinedTask, router: &crate::routing::Router) -> Vec<Line<'static>> {
    let mut l: Vec<Line<'static>> = vec![];
    let plain = Style::default();
    l.push(Line::from(vec![
        Span::styled(
            format!("{}  ", t.task.id),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(t.badge.label().to_string(), badge_style(t.badge)),
        Span::raw(format!("  [{}]", t.context)),
        Span::styled(
            if t.pin_rank.is_some() {
                "  ★ pinned"
            } else {
                ""
            }
            .to_string(),
            Style::default().fg(Color::Yellow),
        ),
    ]));
    l.push(Line::from(t.task.title.clone()));
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
            Style::default().fg(Color::Gray),
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
        l.push(Line::styled(
            "DECISION",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        l.push(Line::from(d.text.clone()));
        for (i, o) in d.options.iter().enumerate() {
            l.push(Line::styled(
                format!("  {}. {o}", i + 1),
                Style::default().fg(Color::Cyan),
            ));
        }
        if let Some(r) = &d.recommendation {
            l.push(Line::styled(
                format!("  rec: {r}"),
                Style::default().fg(Color::Green),
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
            l.push(kv("done", d.clone(), Style::default().fg(Color::DarkGray)));
        }
        for n in &p.next {
            l.push(kv("next", n.clone(), plain));
        }
    }
    l
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let Some(t) = app.current() else {
        f.render_widget(Paragraph::new("task no longer in progress — Esc"), area);
        return;
    };
    let lines = detail_lines(t, &app.router);
    let h = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(5))
        .max(5);
    let [top, bot] = Layout::vertical([Constraint::Length(h), Constraint::Min(3)]).areas(area);
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
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "{:<11} ",
                        i.ts.as_deref()
                            .map(crate::util::short_ts)
                            .unwrap_or_default()
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<14} ", i.source),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(i.title.clone()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::bordered().title(format!(" Timeline ({}) ", app.timeline.len())))
        .highlight_style(Style::default().bg(Color::DarkGray));
    let mut st = ListState::default().with_selected(Some(app.tsel));
    f.render_stateful_widget(list, bot, &mut st);
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
    let items: Vec<ListItem> = ds
        .iter()
        .map(|t| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    if t.pin_rank.is_some() { "★ " } else { "  " },
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(
                    format!("{} ", t.task.id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(t.task.title.clone()),
            ]))
        })
        .collect();
    let mut st = ListState::default().with_selected(Some(app.dsel));
    f.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title(format!(" Decisions ({}) ", ds.len())))
            .highlight_style(Style::default().bg(Color::DarkGray)),
        left,
        &mut st,
    );
    if let Some(t) = ds.get(app.dsel) {
        f.render_widget(
            Paragraph::new(detail_lines(t, &app.router))
                .wrap(Wrap { trim: false })
                .block(Block::bordered()),
            right,
        );
    }
}

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let rows: Vec<Row> = app
        .sessions_sorted()
        .into_iter()
        .map(|s| {
            let st = match s.status.as_str() {
                "needs_input" | "needs_permission" => Style::default().fg(Color::Red),
                "working" => Style::default().fg(Color::Yellow),
                _ => Style::default().fg(Color::Gray),
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
                Cell::from(s.branch.clone().unwrap_or_default()),
                Cell::from(task),
                Cell::from(s.description.clone().unwrap_or_default()),
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
    .style(Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED));
    let mut st = TableState::default().with_selected(Some(app.ssel));
    f.render_stateful_widget(
        Table::new(rows, widths)
            .header(header)
            .block(Block::bordered().title(if app.show_closed {
                " Closed sessions (ccmon) — Enter reopen · e describe · X live "
            } else {
                " Sessions (ccmon) — X closed "
            }))
            .row_highlight_style(Style::default().bg(Color::DarkGray)),
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
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
                (crate::transcript::Kind::Assistant, true) => Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                (crate::transcript::Kind::Tool | crate::transcript::Kind::ToolResult, _) => {
                    Style::default().fg(Color::DarkGray)
                }
                _ => Style::default(),
            };
            if matches.contains(&n) {
                st = st.bg(Color::Yellow).fg(Color::Black);
            }
            if n == v.cursor {
                st = st.bg(Color::DarkGray);
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
