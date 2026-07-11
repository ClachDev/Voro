use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use voro_core::{Event, ScoreBreakdown, TaskState};

use crate::app::{App, CockpitRow, Mode, Screen, TaskRow};

const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// The canonical rendering of a task identifier, right-aligned for list columns.
fn task_ref(id: i64) -> String {
    format!("{:>4}", format!("#{id}"))
}

pub fn draw(frame: &mut Frame, app: &App) {
    match app.screen {
        Screen::Cockpit => draw_cockpit(frame, app),
        Screen::Tasks => draw_tasks(frame, app),
        Screen::Projects => draw_projects(frame, app),
    }
    draw_mode(frame, app);
}

fn draw_mode(frame: &mut Frame, app: &App) {
    match &app.mode {
        Mode::Normal => {}
        Mode::AddProject {
            name,
            path,
            on_path,
            editing,
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
            let title = match editing {
                Some(id) => format!("Edit project #{id} — tab to switch, ⏎ to save"),
                None => "New project — tab to switch, ⏎ to save".to_string(),
            };
            let para = Paragraph::new(vec![
                field("name", name, !*on_path),
                field("path", path, *on_path),
            ])
            .block(Block::default().borders(Borders::ALL).title(title));
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
                        .title(format!("Transition #{task_id}")),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
        }
        Mode::Prompt { kind, buffer, .. } => {
            // The buffer is usually one line, but a RejectWork prompt can be
            // pre-filled with a PR's multi-line review comments (DESIGN.md
            // §11c), so render every line and grow the box to fit.
            let mut lines: Vec<Line> = buffer
                .split('\n')
                .map(|l| Line::from(l.to_string()))
                .collect();
            match lines.last_mut() {
                Some(last) => last.spans.push(Span::raw("▏")),
                None => lines.push(Line::from("▏")),
            }
            let height = (lines.len() as u16 + 2).clamp(3, 20);
            let area = popup_area(frame, 72, height);
            let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} — ⏎ to submit, esc to cancel", kind.title())),
            );
            frame.render_widget(para, area);
        }
        Mode::LinkPr { buffer, .. } => {
            let area = popup_area(frame, 72, 3);
            let line = Line::from(vec![Span::raw(buffer.as_str()), Span::raw("▏")]);
            let para = Paragraph::new(line).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Link PR (URL or owner/repo#n) — ⏎ to submit, esc to cancel"),
            );
            frame.render_widget(para, area);
        }
        Mode::ConfirmPr {
            task_id,
            branch,
            title,
        } => {
            let lines = vec![
                Line::from(vec![
                    Span::raw("push branch "),
                    Span::styled(format!("`{branch}`"), Style::new().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::raw("create a ready PR titled "),
                    Span::styled(format!("“{title}”"), Style::new().fg(Color::Blue)),
                ]),
                Line::default(),
                Line::from(Span::styled(
                    "⏎/y confirm · esc/n cancel",
                    Style::new().dim(),
                )),
            ];
            let area = popup_area(frame, 72, lines.len() as u16 + 2);
            let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Open a PR for #{task_id}?")),
            );
            frame.render_widget(para, area);
        }
        Mode::ConfirmCleanup { task_id, plan } => {
            let lines = vec![
                Line::from(Span::styled(
                    plan.describe(),
                    Style::new().fg(Color::Yellow),
                )),
                Line::default(),
                Line::from(Span::styled(
                    "⏎/y confirm · esc/n leave it",
                    Style::new().dim(),
                )),
            ];
            let area = popup_area(frame, 72, 5);
            let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Clean up the worktree for #{task_id}?")),
            );
            frame.render_widget(para, area);
        }
        Mode::Detail { task_id, scroll } => {
            let Some(row) = app.all.iter().find(|r| r.task.id == *task_id) else {
                return;
            };
            let frame_area = frame.area();
            let width = frame_area.width.saturating_sub(8).clamp(30, 90);
            let height = frame_area.height.saturating_sub(4).clamp(8, 40);
            let area = popup_area(frame, width, height);
            let t = &row.task;
            let mut lines = vec![
                Line::from(Span::styled(t.title.clone(), Style::new().bold())),
                Line::from(Span::styled(
                    format!(
                        "{} · {} · {} · w{}",
                        row.project, t.priority, t.state, row.weight
                    ),
                    Style::new().dim(),
                )),
            ];
            if let Some(q) = &t.question {
                lines.push(Line::from(Span::styled(
                    format!("question: {q}"),
                    Style::new().fg(Color::Cyan),
                )));
            }
            if let Some(pr) = &t.pr_url {
                lines.push(Line::from(pr_span(pr)));
            }
            if let Some(branch) = &t.branch {
                lines.push(Line::from(branch_span(branch)));
            }
            if app.show_score
                && let Some(b) = app.score_breakdown(*task_id)
            {
                lines.extend(score_lines(&b));
            }
            lines.push(Line::default());
            lines.extend(t.body.lines().map(|l| Line::from(l.to_string())));
            if app.show_history {
                lines.push(Line::default());
                lines.extend(history_lines(&app.task_events(*task_id)));
            }
            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "#{task_id} — ⏎ state · 0-3 priority · x score · h history · j/k scroll · esc close"
                )));
            frame.render_widget(para, area);
        }
        Mode::AgentPicker {
            task_id,
            agents,
            resolved,
            sel,
        } => {
            let items: Vec<ListItem> = agents
                .iter()
                .map(|a| {
                    if resolved.as_deref() == Some(a.as_str()) {
                        ListItem::new(format!("{a}  (resolved)"))
                    } else {
                        ListItem::new(a.clone())
                    }
                })
                .collect();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Dispatch #{task_id} — pick agent, ⏎ dispatch, esc cancel"
                )))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
        }
    }
}

/// The inline score decomposition (DESIGN.md §7) that `x` folds into a detail
/// view: one dim line breaking the total down, plus the "not scheduled" note
/// where the task's state keeps it out of the queue. Shared by the cockpit
/// detail pane and the tasks-screen Detail popup so both read identically.
fn score_lines(b: &ScoreBreakdown) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "weight {} · {} (value {}) · {} (+{}) · base w×(p+s) {:.1} · age {:.1}d (+{:.2})",
            b.weight,
            b.priority,
            b.priority_value,
            b.state,
            b.state_bonus,
            b.base,
            b.age_days,
            b.age_bonus
        ),
        Style::new().dim(),
    ))];
    if !matches!(
        b.state,
        TaskState::Ready | TaskState::NeedsInput | TaskState::Review | TaskState::Proposed
    ) {
        lines.push(Line::from(Span::styled(
            format!("({} tasks are not scheduled)", b.state),
            Style::new().dim(),
        )));
    }
    lines
}

/// The event-history section that `h` folds into a detail view: a bold
/// "History" header over one line per event — timestamp dim, kind bold, detail
/// plain, oldest first — matching the line format the popup used before.
fn history_lines(events: &[Event]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("History", Style::new().bold()))];
    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            "no events yet",
            Style::new().dim(),
        )));
    } else {
        lines.extend(events.iter().map(|e| {
            Line::from(vec![
                Span::styled(format!("{:<19} ", e.at), Style::new().dim()),
                Span::styled(format!("{:<10} ", e.kind), Style::new().bold()),
                Span::raw(e.detail.clone().unwrap_or_default()),
            ])
        }));
    }
    lines
}

fn draw_cockpit(frame: &mut Frame, app: &App) {
    let queue_height = (app.queue.len() as u16 + 2).clamp(3, 12);
    // Collapsed to nothing when no session is live, so the queue and detail
    // pane keep the space in the common case (DESIGN.md §9).
    let running_height = if app.running.is_empty() {
        0
    } else {
        (app.running.len() as u16 + 2).clamp(3, 6)
    };
    let [header, queue, detail, running, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(queue_height),
        Constraint::Min(5),
        Constraint::Length(running_height),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_queue(frame, app, queue);
    draw_detail(frame, app, detail);
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

/// The redispatch flag (DESIGN.md §8): a `ready` task whose most recent
/// session ended `failed` or `capped`, read fresh from session history
/// rather than a stored column.
fn redispatch_span() -> Span<'static> {
    Span::styled(
        "  [redispatch]",
        Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
    )
}

/// A tracked GitHub PR (DESIGN.md §11c) rendered for the detail pane, with the
/// jump-to-PR key spelled out so the reviewer knows how to reach it.
fn pr_span(url: &str) -> Span<'static> {
    Span::styled(
        format!("PR: {url}  (g to open)"),
        Style::new().fg(Color::Blue),
    )
}

/// The task's git branch (task #81) rendered for the detail pane — the intended
/// name dispatch injects, or the name the agent reported it worked on.
fn branch_span(branch: &str) -> Span<'static> {
    Span::styled(format!("branch: {branch}"), Style::new().fg(Color::Green))
}

/// The review sub-state (DESIGN.md §8), read from fields rather than a new
/// task state: a review task with no tracked PR shows "next: pr" (the PR key
/// opens one from its summary), one with a PR shows "PR open". `None` for any
/// task that is not in `review`.
fn review_substate_span(task: &voro_core::Task) -> Option<Span<'static>> {
    if task.state != voro_core::TaskState::Review {
        return None;
    }
    Some(if task.pr_url.is_some() {
        Span::styled("  PR open", Style::new().fg(Color::Blue))
    } else {
        Span::styled(
            "  next: pr",
            Style::new().fg(Color::Blue).add_modifier(Modifier::DIM),
        )
    })
}

/// The verb a queue row's Enter performs, from its state.
fn action_verb(state: voro_core::TaskState) -> &'static str {
    match state {
        voro_core::TaskState::NeedsInput => "answer",
        voro_core::TaskState::Review => "review",
        voro_core::TaskState::Proposed => "triage",
        voro_core::TaskState::Ready => "start",
        _ => "",
    }
}

fn draw_queue(frame: &mut Frame, app: &App, area: Rect) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        let item = match row {
            CockpitRow::Queue(idx) => {
                let c = &app.queue[*idx];
                let untriaged = c.task.state == voro_core::TaskState::Proposed;
                let style = if untriaged {
                    Style::new().dim()
                } else {
                    Style::new()
                };
                let score = if untriaged {
                    Span::styled(format!("{:5.1} ", c.score.total), style)
                } else {
                    score_span(c.score.total)
                };
                let mut spans = vec![
                    score,
                    Span::styled(
                        format!(
                            "{} {:6} {} {}: {}",
                            task_ref(c.task.id),
                            action_verb(c.task.state),
                            c.task.priority,
                            c.project_name,
                            c.task.title
                        ),
                        style,
                    ),
                ];
                if let Some(q) = &c.task.question {
                    spans.push(Span::styled(
                        format!("  — {q}"),
                        Style::new().fg(Color::Cyan),
                    ));
                }
                if app.redispatch.contains(&c.task.id) {
                    spans.push(redispatch_span());
                }
                if let Some(substate) = review_substate_span(&c.task) {
                    spans.push(substate);
                }
                ListItem::new(Line::from(spans))
            }
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
        .block(Block::default().borders(Borders::ALL).title("Next"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, area, &mut state);
    if empty {
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        frame.render_widget(Paragraph::new("nothing to do — press n").dim(), inner);
    }
}

/// The body of whichever row is selected — the pane follows the selection
/// instead of holding its own concept of "the" task.
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Detail");

    let selected = app.cockpit_rows.get(app.cockpit_sel);
    let (task, project, score) = match selected {
        Some(CockpitRow::Queue(i)) => {
            let c = &app.queue[*i];
            (&c.task, c.project_name.as_str(), Some(c.score.total))
        }
        Some(CockpitRow::Running(i)) => {
            let r = &app.running[*i];
            match app.all.iter().find(|row| row.task.id == r.task_id) {
                Some(row) => (&row.task, row.project.as_str(), None),
                None => {
                    frame.render_widget(Paragraph::new("").block(block), area);
                    return;
                }
            }
        }
        None => {
            frame.render_widget(Paragraph::new("").block(block), area);
            return;
        }
    };

    let mut meta = vec![Span::raw(format!(
        "#{} · {} · {} · {}",
        task.id, project, task.priority, task.state
    ))];
    if let Some(total) = score {
        meta.push(Span::raw(" · "));
        meta.push(score_span(total));
    }
    let mut lines = vec![
        Line::from(Span::styled(task.title.clone(), Style::new().bold())),
        Line::from(meta),
    ];
    if let Some(q) = &task.question {
        lines.push(Line::from(Span::styled(
            format!("question: {q}"),
            Style::new().fg(Color::Cyan),
        )));
    }
    if let Some(pr) = &task.pr_url {
        lines.push(Line::from(pr_span(pr)));
    } else if task.state == TaskState::Review {
        lines.push(Line::from(Span::styled(
            "next: pr  (g opens one from the summary)",
            Style::new().fg(Color::Blue),
        )));
    }
    if let Some(branch) = &task.branch {
        lines.push(Line::from(branch_span(branch)));
    }
    if app.redispatch.contains(&task.id) {
        lines.push(Line::from(redispatch_span()));
    }
    if app.show_score
        && let Some(b) = app.score_breakdown(task.id)
    {
        lines.extend(score_lines(&b));
    }
    lines.push(Line::default());
    lines.extend(task.body.lines().map(|l| Line::from(l.to_string())));
    if app.show_history {
        lines.push(Line::default());
        lines.extend(history_lines(&app.task_events(task.id)));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para.block(block), area);
}

/// Live sessions (DESIGN.md §9): agent, task state, and elapsed time since
/// dispatch. Collapsed to a zero-height area by `draw_cockpit` when nothing
/// is running, so there is nothing to draw here in the common case.
fn draw_running(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        if let CockpitRow::Running(idx) = row {
            let r = &app.running[*idx];
            if i == app.cockpit_sel {
                selected = Some(items.len());
            }
            let agent = match &r.agent {
                Some(agent) => Span::styled(format!("{agent:8} "), Style::new().fg(Color::Magenta)),
                None => Span::styled(format!("{:8} ", "—"), Style::new().dim()),
            };
            let mut spans = vec![
                Span::raw(format!("{} ", task_ref(r.task_id))),
                agent,
                Span::raw(format!("{:11} ", r.task_state.to_string())),
                Span::styled(
                    format!("{:>6}  ", format_elapsed(r.elapsed_secs)),
                    Style::new().dim(),
                ),
                Span::raw(r.task_title.clone()),
            ];
            if r.session_id.is_none() {
                spans.push(Span::styled(
                    "  ⚠ no live session",
                    Style::new().fg(Color::Yellow),
                ));
            }
            items.push(ListItem::new(Line::from(spans)));
        }
    }
    let mut state = ListState::default().with_selected(selected);
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Running"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, area, &mut state);
}

/// Seconds since a session's `started_at` as a compact clock — `12s`,
/// `3m07s`, `1h05m` — so the running strip's column stays a stable width.
fn format_elapsed(secs: i64) -> String {
    let secs = secs.max(0);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
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
            let mut spans = vec![Span::styled(
                format!(
                    "{} {:11} {} w{} {:14} {}",
                    task_ref(r.task.id),
                    r.task.state,
                    r.task.priority,
                    r.weight,
                    r.project,
                    r.task.title
                ),
                style,
            )];
            if app.redispatch.contains(&r.task.id) {
                spans.push(redispatch_span());
            }
            if let Some(substate) = review_substate_span(&r.task) {
                spans.push(substate);
            }
            spans.extend(blocker_spans(r));
            ListItem::new(Line::from(spans))
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

/// The `blocked by #4, #7` suffix for a parked browser row: what it is waiting
/// on, with already-closed blockers dimmed so the open ones read as the reason
/// it is still parked. Empty for any other state or a parked task with no
/// blockers (which is deliberately deferred, not blocked).
fn blocker_spans(row: &TaskRow) -> Vec<Span<'static>> {
    if row.task.state != voro_core::TaskState::Parked || row.blockers.is_empty() {
        return Vec::new();
    }
    let mut spans = vec![Span::styled("  blocked by ", Style::new().dim())];
    for (i, blocker) in row.blockers.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(", ", Style::new().dim()));
        }
        let style = if blocker.is_open() {
            Style::new()
        } else {
            Style::new().dim()
        };
        spans.push(Span::styled(task_ref(blocker.id).trim().to_string(), style));
    }
    spans
}

/// The projects screen (DESIGN.md §9): one row per project — weight, name,
/// path, open task count. Direct weight editing lives here (`0`–`5`), so the
/// morning ritual is one keystroke per project. The open count is the
/// project's non-terminal tasks, drawn from the already-loaded task list.
fn draw_projects(frame: &mut Frame, app: &App) {
    let [list_area, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .map(|p| {
            let open = app
                .all
                .iter()
                .filter(|r| r.task.project_id == p.id && !r.task.state.is_terminal())
                .count();
            let style = if p.weight == 0 {
                Style::new().dim()
            } else {
                Style::new()
            };
            ListItem::new(Line::from(Span::styled(
                format!("{:>2}  {:14} {:28} {} open", p.weight, p.name, p.path, open),
                style,
            )))
        })
        .collect();
    let empty = items.is_empty();
    let mut state =
        ListState::default().with_selected(if empty { None } else { Some(app.projects_sel) });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Projects"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, list_area, &mut state);
    if empty {
        let inner = list_area.inner(ratatui::layout::Margin::new(1, 1));
        frame.render_widget(
            Paragraph::new("no projects yet — press a to add one").dim(),
            inner,
        );
    }
    draw_status(frame, app, status);
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    // A red status message overrides the key line, as before.
    if let Some(msg) = &app.status {
        let line = Line::from(Span::styled(msg.clone(), Style::new().fg(Color::Red)));
        frame.render_widget(line, area);
        return;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, label)) in key_hints(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().dim()));
        }
        spans.push(Span::styled(key, Style::new().bold()));
        spans.push(Span::styled(format!(" {label}"), Style::new().dim()));
    }
    frame.render_widget(Line::from(spans), area);
}

/// The contextual per-screen key line (ui-redesign §2): the actions that apply
/// on the current screen and selection, as key/label pairs the caller renders
/// key-bold, label-dim. It lists actions, not navigation — `j`/`k` and the
/// arrows are omitted, and `r` refresh stays bound but unadvertised — while `q`
/// and `tab` are always present. On the cockpit the dispatch keys and the
/// score/history toggles only apply to a selected task, so they drop out when
/// nothing is selected.
fn key_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    // `enter_hint` yields "⏎ <verb>"; split the glyph from the verb so the
    // glyph renders as the bold key and the verb as the dim label.
    let enter = app
        .enter_hint()
        .and_then(|h| h.split_once(' '))
        .map(|(_, verb)| ("⏎", verb));
    match app.screen {
        Screen::Cockpit => {
            let mut pairs: Vec<(&'static str, &'static str)> = Vec::new();
            pairs.extend(enter);
            if app.selected_task_id().is_some() {
                pairs.push(("d", "dispatch"));
                pairs.push(("D", "agent"));
            }
            pairs.push(("s", "state"));
            if app.selected_task_id().is_some() {
                pairs.push(("x", "score"));
                pairs.push(("h", "history"));
            }
            pairs.push(("n", "new"));
            pairs.push(("e", "edit"));
            pairs.push(("tab", "tasks"));
            pairs.push(("q", "quit"));
            pairs
        }
        Screen::Tasks => {
            let mut pairs: Vec<(&'static str, &'static str)> = Vec::new();
            pairs.extend(enter);
            pairs.push(("s", "state"));
            pairs.push(("n", "new"));
            pairs.push(("e", "edit"));
            pairs.push(("tab", "projects"));
            pairs.push(("q", "quit"));
            pairs
        }
        Screen::Projects => vec![
            ("0-5", "weight"),
            ("r", "rename"),
            ("a", "add"),
            ("d", "delete"),
            ("tab", "cockpit"),
            ("q", "quit"),
        ],
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use voro_core::{Blocker, Priority, Task, TaskState};

    fn row(state: TaskState, blockers: Vec<Blocker>) -> TaskRow {
        TaskRow {
            task: Task {
                id: 9,
                project_id: 1,
                title: "waiting".into(),
                body: String::new(),
                priority: Priority::P2,
                state,
                agent: None,
                question: None,
                pr_url: None,
                branch: None,
                state_since: String::new(),
                created_at: String::new(),
                closed_at: None,
            },
            project: "voro".into(),
            weight: 3,
            blockers,
        }
    }

    fn blocker(id: i64, state: TaskState) -> Blocker {
        Blocker { id, state }
    }

    /// The rendered text of the suffix, ignoring styling.
    fn suffix(row: &TaskRow) -> String {
        blocker_spans(row)
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
    }

    #[test]
    fn parked_row_lists_blockers_with_open_ones_undimmed() {
        let r = row(
            TaskState::Parked,
            vec![blocker(4, TaskState::Done), blocker(7, TaskState::Running)],
        );
        assert_eq!(suffix(&r), "  blocked by #4, #7");

        let spans = blocker_spans(&r);
        let closed = spans.iter().find(|s| s.content == "#4").unwrap();
        let open = spans.iter().find(|s| s.content == "#7").unwrap();
        assert!(closed.style.add_modifier.contains(Modifier::DIM));
        assert!(!open.style.add_modifier.contains(Modifier::DIM));
    }

    /// End-to-end: a real store with a parked task blocked by one open and one
    /// closed task, rendered through the actual browser draw path, must show the
    /// suffix naming both blockers.
    #[test]
    fn browser_render_shows_blockers_for_a_parked_task() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
        };
        let open = store.create_task(new("open blocker")).unwrap();
        let closed = store.create_task(new("closed blocker")).unwrap();
        store.apply(closed.id, Action::Start).unwrap();
        store.apply(closed.id, Action::Complete(None)).unwrap();
        store.apply(closed.id, Action::Accept).unwrap();
        let waiting = store.create_task(new("waiting")).unwrap();
        store
            .set_blocks_deps(waiting.id, &[open.id, closed.id])
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.toggle_screen();

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw_tasks(f, &app)).unwrap();

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains(&format!("blocked by #{}, #{}", open.id, closed.id)),
            "browser did not annotate the parked row with its blockers: {rendered}"
        );
    }

    /// End-to-end: the projects screen renders one row per project showing its
    /// weight, name, path, and the count of its non-terminal tasks.
    #[test]
    fn projects_screen_renders_weight_name_path_and_open_count() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        // one open task and one terminal task — only the open one is counted
        let new = |title: &str, state: TaskState| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
        };
        store.create_task(new("open", TaskState::Ready)).unwrap();
        let closed = store.create_task(new("closed", TaskState::Ready)).unwrap();
        store
            .apply(closed.id, voro_core::Action::Start)
            .and_then(|_| store.apply(closed.id, voro_core::Action::Complete(None)))
            .and_then(|_| store.apply(closed.id, voro_core::Action::Accept))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        key_to_projects(&mut app);
        assert_eq!(app.screen, Screen::Projects);

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("3") && rendered.contains("voro") && rendered.contains("/tmp/voro"),
            "projects row missing weight/name/path: {rendered}"
        );
        assert!(
            rendered.contains("1 open"),
            "projects row should count only the open task: {rendered}"
        );
    }

    /// Drive the app onto the projects screen with the real key handler.
    fn key_to_projects(app: &mut crate::app::App) {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        app.on_key(KeyEvent::from(KeyCode::Char('3')));
    }

    /// End-to-end: with the score and history toggles on, the cockpit detail
    /// pane renders the inline decomposition line and the history section for
    /// the selected task, rather than opening a popup.
    #[test]
    fn cockpit_detail_folds_in_score_and_history_when_toggled() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "a task".into(),
                body: "body".into(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Char('h')));

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("base w×(p+s)"),
            "score decomposition should fold into the detail pane: {rendered}"
        );
        assert!(
            rendered.contains("History") && rendered.contains("created"),
            "history should fold into the detail pane: {rendered}"
        );
    }

    /// End-to-end: on the tasks screen the same sections fold into the Detail
    /// popup — `x`/`h` inside the popup drive the same shared flags — so score
    /// and history render inline on this screen too, never as separate popups.
    #[test]
    fn tasks_detail_popup_folds_in_score_and_history_when_toggled() {
        use crate::app::{App, Mode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "a task".into(),
                body: "body".into(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('2'))); // tasks screen
        app.on_key(KeyEvent::from(KeyCode::Enter)); // open the Detail popup
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Char('h')));
        assert!(matches!(app.mode, Mode::Detail { .. }));

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("base w×(p+s)"),
            "score decomposition should fold into the Detail popup: {rendered}"
        );
        assert!(
            rendered.contains("History") && rendered.contains("created"),
            "history should fold into the Detail popup: {rendered}"
        );
    }

    /// The cockpit key line only advertises the score/history toggles and the
    /// dispatch keys while a task is selected — with an empty queue there is
    /// nothing for them to act on, so they drop out.
    #[test]
    fn cockpit_key_line_drops_score_and_history_without_a_selection() {
        use crate::app::App;
        use voro_core::{NewTask, Store};

        let ctx = || {
            crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new("/nonexistent/voro.db"))
        };

        let empty = App::new(Store::open_in_memory().unwrap(), ctx()).unwrap();
        assert_eq!(empty.screen, Screen::Cockpit);
        assert!(empty.selected_task_id().is_none());
        let labels: Vec<&str> = key_hints(&empty).iter().map(|(_, l)| *l).collect();
        for dropped in ["score", "history", "dispatch", "agent"] {
            assert!(
                !labels.contains(&dropped),
                "empty cockpit should not advertise {dropped}: {labels:?}"
            );
        }

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "a task".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();
        let selected = App::new(store, ctx()).unwrap();
        assert!(selected.selected_task_id().is_some());
        let labels: Vec<&str> = key_hints(&selected).iter().map(|(_, l)| *l).collect();
        for shown in ["score", "history", "dispatch", "agent"] {
            assert!(
                labels.contains(&shown),
                "cockpit with a selection should advertise {shown}: {labels:?}"
            );
        }
    }

    #[test]
    fn non_parked_and_blockerless_rows_get_no_suffix() {
        assert!(
            blocker_spans(&row(TaskState::Ready, vec![blocker(4, TaskState::Done)])).is_empty()
        );
        assert!(blocker_spans(&row(TaskState::Parked, vec![])).is_empty());
    }
}
