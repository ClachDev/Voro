use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, CockpitRow, Mode, Screen};

const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

pub fn draw(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Cockpit => draw_cockpit(frame, app),
        Screen::Tasks => draw_tasks(frame, app),
    }
    draw_mode(frame, app);
}

fn draw_mode(frame: &mut Frame, app: &App) {
    match &app.mode {
        Mode::Normal => {}
        Mode::Weights { sel } => {
            let items: Vec<ListItem> = app
                .projects
                .iter()
                .map(|p| ListItem::new(format!("{}  {}", p.weight, p.name)))
                .collect();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Weights — press 0-5, esc to close"),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
        }
        Mode::AddProject {
            name,
            path,
            on_path,
        } => {
            let area = popup_area(frame, 56, 4);
            let field = |label: &str, value: &str, active: bool| {
                let style = if active {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Line::from(vec![
                    Span::raw(format!("{label}: ")),
                    Span::styled(format!("{value}▏"), style),
                ])
            };
            let para = Paragraph::new(vec![
                field("name", name, !*on_path),
                field("path", path, *on_path),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("New project — tab to switch, ⏎ to save"),
            );
            frame.render_widget(para, area);
        }
        Mode::PickProject { sel } => {
            let items: Vec<ListItem> = app
                .projects
                .iter()
                .map(|p| ListItem::new(p.name.clone()))
                .collect();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Project for the new task"),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
        }
        Mode::Transition {
            task_id,
            actions,
            sel,
        } => {
            let items: Vec<ListItem> = actions
                .iter()
                .map(|a| ListItem::new(crate::app::action_label(a)))
                .collect();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 48, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Transition task {task_id}")),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
        }
        Mode::Prompt { kind, buffer, .. } => {
            let area = popup_area(frame, 64, 3);
            let para = Paragraph::new(Line::from(vec![Span::raw(format!("{buffer}▏"))])).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} — ⏎ to submit, esc to cancel", kind.title())),
            );
            frame.render_widget(para, area);
        }
        Mode::Score {
            task_id,
            state,
            breakdown,
        } => {
            let b = breakdown;
            let mut lines = vec![
                Line::from(format!("weight          {:>6}", b.weight)),
                Line::from(format!(
                    "priority        {:>6}  (value {})",
                    b.priority.to_string(),
                    b.priority_value
                )),
                Line::from(format!("base w × p      {:>6.1}", b.base)),
                Line::from(format!("age             {:>6.1} days", b.age_days)),
                Line::from(format!(
                    "age bonus       {:>6.2}  (0.1/day, cap 2)",
                    b.age_bonus
                )),
                Line::from(Span::styled(
                    format!("total           {:>6.2}", b.total),
                    Style::new().bold().fg(Color::Yellow),
                )),
            ];
            if !matches!(
                state,
                voro_core::TaskState::Ready
                    | voro_core::TaskState::NeedsInput
                    | voro_core::TaskState::Review
            ) {
                lines.push(Line::from(Span::styled(
                    format!("({state} tasks are not scheduled)"),
                    Style::new().dim(),
                )));
            }
            let height = lines.len() as u16 + 2;
            let area = popup_area(frame, 44, height);
            let para = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Score of task {task_id}")),
            );
            frame.render_widget(para, area);
        }
    }
}

fn draw_cockpit(frame: &mut Frame, app: &App) {
    let inbox_rows = app.inbox.len() + usize::from(app.proposed > 0);
    let inbox_height = (inbox_rows as u16 + 2).clamp(3, 12);
    let [header, inbox, focus, running, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(inbox_height),
        Constraint::Min(5),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_inbox(frame, app, inbox);
    draw_focus(frame, app, focus);
    draw_running(frame, app, running);
    draw_status(frame, app, status);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled("voro", Style::new().bold()), Span::raw("  ")];
    for p in &app.projects {
        let style = if p.weight == 0 {
            Style::new().dim()
        } else {
            Style::new()
        };
        spans.push(Span::styled(format!("{}:{}  ", p.name, p.weight), style));
    }
    frame.render_widget(Line::from(spans), area);
}

fn score_span(total: f64) -> Span<'static> {
    Span::styled(format!("{total:5.1} "), Style::new().fg(Color::Yellow))
}

fn draw_inbox(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        let item = match row {
            CockpitRow::Inbox(idx) => {
                let c = &app.inbox[*idx];
                let mut spans = vec![
                    score_span(c.score.total),
                    Span::raw(format!(
                        "{} {:11} {}: {}",
                        c.task.priority, c.task.state, c.project_name, c.task.title
                    )),
                ];
                if let Some(q) = &c.task.question {
                    spans.push(Span::styled(
                        format!("  — {q}"),
                        Style::new().fg(Color::Cyan),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
            CockpitRow::Triage => ListItem::new(Line::from(Span::styled(
                format!("      triage {} proposed task(s)  ⏎", app.proposed),
                Style::new().fg(Color::Magenta),
            ))),
            _ => continue,
        };
        if i == app.cockpit_sel {
            selected = Some(items.len());
        }
        items.push(item);
    }
    let empty = items.is_empty();
    let mut state = ListState::default().with_selected(selected);
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Inbox"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, area, &mut state);
    if empty {
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        frame.render_widget(Paragraph::new("nothing needs you").dim(), inner);
    }
}

fn draw_focus(frame: &mut Frame, app: &App, area: Rect) {
    let is_selected = matches!(
        app.cockpit_rows.get(app.cockpit_sel),
        Some(CockpitRow::Focus)
    );
    let border_style = if is_selected {
        Style::new().fg(Color::Yellow)
    } else {
        Style::new()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title("Focus");

    let content = match &app.focus {
        Some(c) => {
            let mut lines = vec![
                Line::from(Span::styled(
                    c.task.title.clone(),
                    Style::new().bold().add_modifier(if is_selected {
                        Modifier::REVERSED
                    } else {
                        Modifier::empty()
                    }),
                )),
                Line::from(vec![
                    score_span(c.score.total),
                    Span::raw(format!(
                        "{} · {} · task {} · weight {}",
                        c.project_name, c.task.priority, c.task.id, c.score.weight
                    )),
                ]),
                Line::default(),
            ];
            lines.extend(c.task.body.lines().map(|l| Line::from(l.to_string())));
            Paragraph::new(lines).wrap(Wrap { trim: false })
        }
        None => Paragraph::new("no ready tasks").dim(),
    };
    frame.render_widget(content.block(block), area);
}

fn draw_running(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        if let CockpitRow::Running(idx) = row {
            let r = &app.running[*idx];
            if i == app.cockpit_sel {
                selected = Some(items.len());
            }
            items.push(ListItem::new(format!(
                "{} ({}): {}",
                r.task.id, r.project, r.task.title
            )));
        }
    }
    let mut state = ListState::default().with_selected(selected);
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Running"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_tasks(frame: &mut Frame, app: &App) {
    let [list_area, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    let items: Vec<ListItem> = app
        .all
        .iter()
        .map(|r| {
            let closed = r.task.state.is_terminal();
            let style = if closed || r.weight == 0 {
                Style::new().dim()
            } else {
                Style::new()
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{:4} {:11} {} w{} {:14} {}",
                    r.task.id, r.task.state, r.task.priority, r.weight, r.project, r.task.title
                ),
                style,
            )))
        })
        .collect();
    let mut state = ListState::default().with_selected(if app.all.is_empty() {
        None
    } else {
        Some(app.tasks_sel)
    });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("All tasks"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, list_area, &mut state);
    draw_status(frame, app, status);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.status {
        Some(msg) => Line::from(Span::styled(msg.clone(), Style::new().fg(Color::Red))),
        None => Line::from(Span::styled(
            "q quit · tab screen · j/k move · ⏎ open · n new · e edit · s state · x score · w weights · P project",
            Style::new().dim(),
        )),
    };
    frame.render_widget(line, area);
}

/// A centred popup rect, cleared of what is beneath it.
pub fn popup_area(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    frame.render_widget(Clear, rect);
    rect
}
