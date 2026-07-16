use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use voro_core::{
    DepKind, DepRef, Event, ScoreBreakdown, Session, SessionOutcome, StateCounts, TaskState,
};

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
        Mode::PickProject { sel, flow } => {
            let items: Vec<ListItem> = app
                .projects
                .iter()
                .map(|p| ListItem::new(p.name.clone()))
                .collect();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let title = match flow {
                crate::app::CreateFlow::Editor => "Project for the new task",
                crate::app::CreateFlow::Plan => "Project to plan a task in",
            };
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
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
        Mode::ConfirmRebase {
            task_id,
            branch,
            base,
        } => {
            let lines = vec![
                Line::from(vec![
                    Span::raw("pull "),
                    Span::styled(format!("`{base}`"), Style::new().fg(Color::Green)),
                    Span::raw(" into the project checkout"),
                ]),
                Line::from(vec![
                    Span::raw("continue the task's session to rebase "),
                    Span::styled(format!("`{branch}`"), Style::new().fg(Color::Blue)),
                    Span::raw(" and resolve its conflicts"),
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
                    .title(format!("Resolve conflicts on #{task_id}?")),
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
            if t.human {
                lines.push(human_line());
            }
            if let Some(session) = app.last_sessions.get(task_id) {
                lines.extend(session_lines(session, t.state));
            }
            lines.extend(dep_lines(
                app.deps.get(task_id).map_or(&[][..], |v| v),
                app.dependents.get(task_id).map_or(&[][..], |v| v),
            ));
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
        Mode::ReviewActionPicker {
            options,
            current,
            sel,
            ..
        } => {
            let items: Vec<ListItem> = options
                .iter()
                .map(|o| {
                    if o == current {
                        ListItem::new(format!("{o}  (current)"))
                    } else {
                        ListItem::new(o.to_string())
                    }
                })
                .collect();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 52, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Review action — ⏎ set, esc cancel"),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
        }
    }
}

/// The inline score decomposition (DESIGN.md §7) that `x` folds into a detail
/// view: one dim line breaking the total down, plus a "not scheduled" note
/// where the task's state keeps it out of the queue. Shared by the cockpit pane
/// and the tasks-screen Detail popup.
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
        TaskState::Ready
            | TaskState::NeedsInput
            | TaskState::Review
            | TaskState::Stalled
            | TaskState::Proposed
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
/// plain, oldest first.
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
    // Projects stay on the left where they are edited every morning; the per-
    // state counts sit right-aligned so they never push the weights around.
    let counts = counts_line(&app.counts);
    let counts_width = counts.width() as u16;
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(counts_width)]).areas(area);
    frame.render_widget(Line::from(spans), left);
    frame.render_widget(counts, right);
}

/// The persistent header indicator (DESIGN.md §12): a compact per-state tally
/// so the backlogs stay felt independently of the queue's uniform cap (§7).
/// Each state shows only when non-zero; the untriaged `triage` count is
/// highlighted, the rest dim.
fn counts_line(counts: &StateCounts) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut push = |label: &str, n: i64, style: Style| {
        if n == 0 {
            return;
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(format!("{label} {n}"), style));
    };
    let dim = Style::new().dim();
    push(
        "triage",
        counts.proposed,
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    );
    push("input", counts.needs_input, dim);
    push("review", counts.review, dim);
    push("waiting", counts.waiting, dim);
    push("stalled", counts.stalled, dim);
    push("ready", counts.ready, dim);
    push("done", counts.done, dim);
    Line::from(spans)
}

fn score_span(total: f64) -> Span<'static> {
    Span::styled(format!("{total:5.1} "), Style::new().fg(Color::Yellow))
}

/// The incomplete-report flag (DESIGN.md §8): a `review` task carrying only one
/// of a branch and a summary. Yellow to match the running strip's "no live
/// session" warning, since both are anomalies needing the operator.
fn incomplete_report_span() -> Span<'static> {
    Span::styled(
        "  [incomplete report]",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// The human-only flag (task #100) rendered as a row marker. A property of the
/// task rather than an anomaly, so it stays dim where the warning flags shout.
fn human_span() -> Span<'static> {
    Span::styled("  [human]", Style::new().dim())
}

/// The same flag spelled out for a detail view, beside the branch/PR lines.
fn human_line() -> Line<'static> {
    Line::from(Span::styled(
        "human-only — never dispatched",
        Style::new().dim(),
    ))
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

/// A review row's next action rendered as a browser suffix (DESIGN.md §3). The
/// browser shows state in its own column, so only `review` — whose verb reads
/// the tracked PR, not the state alone — earns the suffix.
fn review_next_span(task: &voro_core::Task) -> Option<Span<'static>> {
    if task.state != voro_core::TaskState::Review {
        return None;
    }
    let verb = task.next_action()?;
    Some(Span::styled(
        format!("  next: {verb}"),
        Style::new().fg(Color::Blue),
    ))
}

/// A task's newest session, rendered for the attention states (tasks #73/#110).
/// A finished session is a post-mortem: its outcome (`capped` yellow — it clears
/// when the quota resets — `failed` red and wanting its log read), agent, and
/// end time. An open one shows agent and start time. Both end on the log path
/// the `l` key pages. States where the session is history rather than context
/// (`done`, `rejected`, a redispatch-ready task) render nothing.
fn session_lines(session: &Session, state: TaskState) -> Vec<Line<'static>> {
    if !matches!(
        state,
        TaskState::Stalled
            | TaskState::Running
            | TaskState::Review
            | TaskState::Waiting
            | TaskState::NeedsInput
    ) {
        return Vec::new();
    }
    let mut lines = vec![match &session.ended_at {
        Some(ended) => {
            let outcome_color = match session.outcome {
                Some(SessionOutcome::Capped) => Color::Yellow,
                _ => Color::Red,
            };
            let outcome = session
                .outcome
                .map(|o| o.to_string())
                .unwrap_or_else(|| "unknown".into());
            Line::from(vec![
                Span::raw("last session: "),
                Span::styled(
                    outcome,
                    Style::new().fg(outcome_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · {} · ended {ended}", session.agent),
                    Style::new().dim(),
                ),
            ])
        }
        None => Line::from(vec![
            Span::raw("session: "),
            Span::styled(
                format!("{} · started {}", session.agent, session.started_at),
                Style::new().dim(),
            ),
        ]),
    }];
    lines.push(match &session.log_path {
        Some(path) => Line::from(vec![
            Span::styled(format!("log: {path}"), Style::new().dim()),
            Span::styled("  (l opens in $PAGER)", Style::new().fg(Color::Blue)),
        ]),
        None => Line::from(Span::styled(
            "no session log was recorded",
            Style::new().dim(),
        )),
    });
    lines
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
                            "{} {:10} {} {}: {}",
                            task_ref(c.task.id),
                            c.task.next_action().map_or("", |a| a.as_str()),
                            c.task.priority,
                            c.project_name,
                            c.task.title
                        ),
                        style,
                    ),
                ];
                if c.task.human {
                    spans.push(human_span());
                }
                if let Some(q) = &c.task.question {
                    spans.push(Span::styled(
                        format!("  — {q}"),
                        Style::new().fg(Color::Cyan),
                    ));
                }
                if app.incomplete_report.contains(&c.task.id) {
                    // The verb column says "pr", but a PR cannot be opened
                    // from a half-finished report, so name the gap too.
                    spans.push(incomplete_report_span());
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
    } else if app.incomplete_report.contains(&task.id) {
        // A review task missing a branch or summary: `pr` would fail, so say
        // what is needed rather than the optimistic "next: pr".
        lines.push(Line::from(incomplete_report_span()));
    } else if let Some(verb) = task.next_action() {
        let hint = match verb {
            voro_core::NextAction::Pr => "  (g opens one from the summary)",
            _ => "",
        };
        lines.push(Line::from(Span::styled(
            format!("next: {verb}{hint}"),
            Style::new().fg(Color::Blue),
        )));
    }
    if let Some(branch) = &task.branch {
        lines.push(Line::from(branch_span(branch)));
    }
    if task.human {
        lines.push(human_line());
    }
    if let Some(session) = app.last_sessions.get(&task.id) {
        lines.extend(session_lines(session, task.state));
    }
    lines.extend(dep_lines(
        app.deps.get(&task.id).map_or(&[][..], |v| v),
        app.dependents.get(&task.id).map_or(&[][..], |v| v),
    ));
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

    // Measure the wrapped body height against the inner area to clamp the
    // scroll and decide whether to advertise it. `line_count` wants the text
    // width, so pass the inner width with the block off this measuring paragraph.
    let inner = block.inner(area);
    let total = para.line_count(inner.width) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    app.detail_max_scroll.set(max_scroll);
    let scroll = app.detail_scroll.min(max_scroll);

    let block = if max_scroll > 0 {
        block.title_bottom(
            Line::from(format!(" {scroll}/{max_scroll} ↕ J/K PgDn/PgUp ")).right_aligned(),
        )
    } else {
        block
    };
    frame.render_widget(para.scroll((scroll, 0)).block(block), area);
}

/// Live sessions (DESIGN.md §9): agent, task state, and elapsed time since
/// dispatch. `draw_cockpit` collapses this to a zero-height area when nothing
/// is running.
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
            if r.task.human {
                spans.push(human_span());
            }
            if app.incomplete_report.contains(&r.task.id) {
                spans.push(incomplete_report_span());
            } else if let Some(span) = review_next_span(&r.task) {
                spans.push(span);
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

/// The dependency section of a detail view (task #103), both directions, one
/// line per edge: `blocked by #N title` for the task's own blockers, `blocks #N
/// title` for the reverse edges, and other forward kinds by name. Closed tasks
/// are dimmed, as in `blocker_spans`.
fn dep_lines(deps: &[DepRef], dependents: &[DepRef]) -> Vec<Line<'static>> {
    let blocked_by = deps.iter().filter(|d| d.kind == DepKind::Blocks);
    let blocks = dependents.iter().filter(|d| d.kind == DepKind::Blocks);
    let other = deps.iter().filter(|d| d.kind != DepKind::Blocks);
    blocked_by
        .map(|d| dep_line("blocked by", d))
        .chain(blocks.map(|d| dep_line("blocks", d)))
        .chain(other.map(|d| dep_line(d.kind.as_str(), d)))
        .collect()
}

fn dep_line(label: &str, d: &DepRef) -> Line<'static> {
    let target = if d.is_open() {
        Style::new()
    } else {
        Style::new().dim()
    };
    Line::from(vec![
        Span::styled(format!("{label} "), Style::new().dim()),
        Span::styled(format!("{} {}", task_ref(d.id).trim(), d.title), target),
    ])
}

/// The `blocked by #4, #7` suffix for a parked browser row, with already-closed
/// blockers dimmed so the open ones read as the reason it is still parked. Empty
/// for any other state, or a parked task with no blockers (deferred, not blocked).
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
/// path, open task count, and the review action when one is pinned (§8). The
/// open count is the project's non-terminal tasks, from the loaded task list.
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
            let action = match &p.review_action {
                voro_core::ReviewAction::Auto => String::new(),
                other => format!("  [{other}]"),
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{:>2}  {:14} {:28} {} open{action}",
                    p.weight, p.name, p.path, open
                ),
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
/// key-bold, label-dim. It lists actions, not navigation (`j`/`k` and `r`
/// refresh are omitted); `q` and `tab` are always present. Selection-only
/// actions drop out on the cockpit when nothing is selected.
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
            if app.selected_session_log().is_some() {
                pairs.push(("l", "log"));
            }
            if app.selected_can_hand_off() {
                pairs.push(("w", "wait"));
            }
            if app.selected_can_rebase() {
                pairs.push(("R", "rebase"));
            }
            pairs.push(("n", "new"));
            pairs.push(("N", "plan"));
            pairs.push(("e", "edit"));
            pairs.push(("tab", "tasks"));
            pairs.push(("q", "quit"));
            pairs
        }
        Screen::Tasks => {
            let mut pairs: Vec<(&'static str, &'static str)> = Vec::new();
            pairs.extend(enter);
            if app.selected_session_log().is_some() {
                pairs.push(("l", "log"));
            }
            if app.selected_can_hand_off() {
                pairs.push(("w", "wait"));
            }
            if app.selected_can_rebase() {
                pairs.push(("R", "rebase"));
            }
            pairs.push(("s", "state"));
            pairs.push(("n", "new"));
            pairs.push(("N", "plan"));
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
            ("v", "review action"),
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
    use voro_core::{Priority, Task, TaskState};

    fn row(state: TaskState, blockers: Vec<DepRef>) -> TaskRow {
        TaskRow {
            task: Task {
                id: 9,
                project_id: 1,
                title: "waiting".into(),
                body: String::new(),
                priority: Priority::P2,
                state,
                agent: None,
                human: false,
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

    fn blocker(id: i64, state: TaskState) -> DepRef {
        DepRef {
            id,
            title: String::new(),
            state,
            kind: DepKind::Blocks,
        }
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
            human: false,
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

    /// End-to-end: a human-only task carries the `[human]` marker on its queue
    /// and browser rows and the spelled-out line in the cockpit detail pane,
    /// drawn dim rather than warning-coloured — a property, not an anomaly.
    #[test]
    fn human_only_flag_renders_in_queue_browser_and_detail() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "hands-on".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: true,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal.draw(|f| draw(f, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let cockpit = render(&app, &mut terminal);
        assert!(
            cockpit.contains("[human]"),
            "queue row should carry the marker: {cockpit}"
        );
        assert!(
            cockpit.contains("human-only — never dispatched"),
            "detail pane should spell the flag out: {cockpit}"
        );

        app.toggle_screen();
        let browser = render(&app, &mut terminal);
        assert!(
            browser.contains("[human]"),
            "browser row should carry the marker: {browser}"
        );

        let marker = human_span();
        assert!(marker.style.add_modifier.contains(Modifier::DIM));
        assert!(!marker.style.add_modifier.contains(Modifier::BOLD));
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
            human: false,
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
                human: false,
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

    /// The dependency section lists the task's own blockers first, then the
    /// reverse edges it holds back, then other kinds by name — closed tasks
    /// dimmed, open ones plain — and reverse edges of non-blocks kinds (which
    /// would read in the wrong direction) not at all.
    #[test]
    fn dep_lines_render_both_directions_with_closed_targets_dimmed() {
        use voro_core::{DepKind, DepRef};

        let dep = |id: i64, kind, state| DepRef {
            id,
            title: format!("t{id}"),
            state,
            kind,
        };
        let deps = vec![
            dep(4, DepKind::Blocks, TaskState::Done),
            dep(6, DepKind::DiscoveredFrom, TaskState::Ready),
        ];
        let dependents = vec![
            dep(9, DepKind::Blocks, TaskState::Ready),
            dep(11, DepKind::Related, TaskState::Ready),
        ];

        let lines = dep_lines(&deps, &dependents);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            vec!["blocked by #4 t4", "blocks #9 t9", "discovered-from #6 t6"]
        );

        let closed = &lines[0].spans[1];
        assert!(closed.style.add_modifier.contains(Modifier::DIM));
        let open = &lines[1].spans[1];
        assert!(!open.style.add_modifier.contains(Modifier::DIM));

        assert!(dep_lines(&[], &[]).is_empty());
    }

    /// End-to-end: a task with dependencies in both directions renders them in
    /// the cockpit detail pane and in the tasks-screen Detail popup — blockers,
    /// the task it blocks, and its discovered-from source, each with its title.
    #[test]
    fn detail_views_show_dependencies_in_both_directions() {
        use crate::app::{App, Mode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{Action, DepKind, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str, priority| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Ready,
            agent: None,
            human: false,
        };
        let closed = store
            .create_task(new("closed blocker", Priority::P2))
            .unwrap();
        store.apply(closed.id, Action::Start).unwrap();
        store.apply(closed.id, Action::Complete(None)).unwrap();
        store.apply(closed.id, Action::Accept).unwrap();
        let source = store.create_task(new("source", Priority::P2)).unwrap();
        // P1 puts the target at the top of the queue, so the cockpit detail
        // pane shows it without moving the selection.
        let target = store.create_task(new("target", Priority::P1)).unwrap();
        let waiting = store.create_task(new("waiting", Priority::P2)).unwrap();
        store
            .add_dep(target.id, closed.id, DepKind::Blocks)
            .unwrap();
        store
            .add_dep(target.id, source.id, DepKind::DiscoveredFrom)
            .unwrap();
        store
            .add_dep(waiting.id, target.id, DepKind::Blocks)
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal.draw(|f| draw(f, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let blocked_by = format!("blocked by #{} closed blocker", closed.id);
        let blocks = format!("blocks #{} waiting", waiting.id);
        let discovered = format!("discovered-from #{} source", source.id);

        let cockpit = render(&app, &mut terminal);
        for needle in [&blocked_by, &blocks, &discovered] {
            assert!(
                cockpit.contains(needle.as_str()),
                "cockpit detail pane should show '{needle}': {cockpit}"
            );
        }

        // The same lines in the tasks-screen Detail popup, on the target row.
        app.on_key(KeyEvent::from(KeyCode::Char('2')));
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(app.mode, Mode::Detail { task_id, .. } if task_id == target.id),
            "expected the Detail popup on the target task"
        );
        let popup = render(&app, &mut terminal);
        for needle in [&blocked_by, &blocks, &discovered] {
            assert!(
                popup.contains(needle.as_str()),
                "Detail popup should show '{needle}': {popup}"
            );
        }
    }

    /// End-to-end: every queue row carries its next-action verb (DESIGN.md §3),
    /// one task per arm of the derivation, rendered through the real cockpit
    /// draw path. `do` versus `dispatch` is also the human-task marker.
    #[test]
    fn cockpit_queue_shows_the_next_action_verb_on_each_row() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        let new = |title: &str, state, human| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human,
        };

        let triage = store
            .create_task(new("untriaged", TaskState::Proposed, false))
            .unwrap();
        let answer = store
            .create_task(new("asking", TaskState::Ready, false))
            .unwrap();
        store.apply(answer.id, Action::Start).unwrap();
        store
            .apply(answer.id, Action::Ask("A or B?".into()))
            .unwrap();
        let pr = store
            .create_task(new("done, no PR", TaskState::Ready, false))
            .unwrap();
        store.apply(pr.id, Action::Start).unwrap();
        store.apply(pr.id, Action::Complete(None)).unwrap();
        let review_pr = store
            .create_task(new("done, PR open", TaskState::Ready, false))
            .unwrap();
        store.apply(review_pr.id, Action::Start).unwrap();
        store.apply(review_pr.id, Action::Complete(None)).unwrap();
        store
            .set_pr(review_pr.id, Some("https://github.com/o/r/pull/1"))
            .unwrap();
        let redispatch = store
            .create_task(new("died", TaskState::Ready, false))
            .unwrap();
        let (_, session) = store
            .record_dispatch(redispatch.id, "claude", Some(1), None)
            .unwrap();
        store.reconcile_session(session.id, false, false).unwrap();
        let do_ = store
            .create_task(new("by hand", TaskState::Ready, true))
            .unwrap();
        let dispatch = store
            .create_task(new("startable", TaskState::Ready, false))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();

        for (task, verb) in [
            (&triage, "triage"),
            (&answer, "answer"),
            (&pr, "pr"),
            (&review_pr, "review PR"),
            (&redispatch, "redispatch"),
            (&do_, "do"),
            (&dispatch, "dispatch"),
        ] {
            let cell = format!("{} {:10}", task_ref(task.id), verb);
            assert!(
                rendered.contains(&cell),
                "queue row for #{} should carry '{verb}': {rendered}",
                task.id
            );
        }
    }

    /// End-to-end: a body taller than the focus card overflows, so the pane
    /// advertises the scroll, `J` moves the view down and clamps at the bottom,
    /// and `K` returns it to the top. Renders into a short terminal to force
    /// the overflow, since the clamp depends on the measured geometry.
    #[test]
    fn cockpit_focus_card_scrolls_a_long_body() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let body = (0..40)
            .map(|i| format!("row{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "a long task".into(),
                body,
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal.draw(|f| draw(f, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let first = render(&app, &mut terminal);
        let max = app.detail_max_scroll.get();
        assert!(max > 0, "the body should overflow the focus card");
        assert!(
            first.contains("J/K"),
            "an overflowing pane advertises the scroll"
        );
        assert!(
            first.contains("row00"),
            "the top of the body is visible at rest"
        );

        // `J` scrolls down; the top line falls off, the indicator advances.
        for _ in 0..max as usize + 5 {
            app.on_key(KeyEvent::from(KeyCode::Char('J')));
        }
        assert_eq!(app.detail_scroll, max, "J clamps at the bottom");
        let bottom = render(&app, &mut terminal);
        assert!(!bottom.contains("row00"), "the top scrolled out of view");

        // `K` returns to the top and stops there.
        for _ in 0..max as usize + 5 {
            app.on_key(KeyEvent::from(KeyCode::Char('K')));
        }
        assert_eq!(app.detail_scroll, 0, "K clamps at the top");

        // Moving the selection resets the view to the top of the new body.
        for _ in 0..3 {
            app.on_key(KeyEvent::from(KeyCode::Char('J')));
        }
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.detail_scroll, 0, "a new selection starts at the top");
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
                human: false,
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

    /// The cockpit detail pane answers "what happened" for a stalled task
    /// (task #73): the dead session's outcome, agent, end time, and log path
    /// render under the metadata, and the key line advertises `l`. A capped
    /// session reads `capped`; a clean ready task carries none of it.
    #[test]
    fn detail_pane_shows_a_stalled_tasks_session_post_mortem() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let ctx = || {
            crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new("/nonexistent/voro.db"))
        };
        let app_with_session = |capped: bool| {
            let mut store = Store::open_in_memory().unwrap();
            let p = store.create_project("voro", "/tmp/voro").unwrap();
            store.set_weight(p.id, 3).unwrap();
            let task = store
                .create_task(NewTask {
                    project_id: p.id,
                    title: "went quiet".into(),
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                })
                .unwrap();
            let (_, session) = store
                .record_dispatch(task.id, "claude", Some(1), Some("/tmp/voro/s.log"))
                .unwrap();
            store.reconcile_session(session.id, false, capped).unwrap();
            App::new(store, ctx()).unwrap()
        };
        let render = |app: &App| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal.draw(|f| draw(f, app)).unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let failed = app_with_session(false);
        let rendered = render(&failed);
        assert!(rendered.contains("last session: failed"), "{rendered}");
        assert!(rendered.contains("claude"), "{rendered}");
        assert!(rendered.contains("ended 2"), "{rendered}");
        assert!(rendered.contains("log: /tmp/voro/s.log"), "{rendered}");
        let labels: Vec<&str> = key_hints(&failed).iter().map(|(_, l)| *l).collect();
        assert!(labels.contains(&"log"), "{labels:?}");

        let capped = app_with_session(true);
        assert!(render(&capped).contains("last session: capped"));

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "fresh".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        let clean = App::new(store, ctx()).unwrap();
        let rendered = render(&clean);
        assert!(!rendered.contains("last session"), "{rendered}");
        let labels: Vec<&str> = key_hints(&clean).iter().map(|(_, l)| *l).collect();
        assert!(!labels.contains(&"log"), "{labels:?}");
    }

    /// A task whose session is still open — here needs-input, whose session
    /// survives the transition (DESIGN.md §8) — shows the session's agent,
    /// start time, and log path instead of a post-mortem (task #110), and the
    /// key line advertises `l` there too.
    #[test]
    fn detail_pane_shows_an_open_session_on_a_needs_input_task() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                title: "mid-flight".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        store
            .record_dispatch(task.id, "claude", Some(1), Some("/tmp/voro/open.log"))
            .unwrap();
        store.apply(task.id, Action::Ask("A or B?".into())).unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(rendered.contains("session: claude"), "{rendered}");
        assert!(rendered.contains("started 2"), "{rendered}");
        assert!(rendered.contains("log: /tmp/voro/open.log"), "{rendered}");
        assert!(!rendered.contains("last session:"), "{rendered}");
        let labels: Vec<&str> = key_hints(&app).iter().map(|(_, l)| *l).collect();
        assert!(labels.contains(&"log"), "{labels:?}");
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
                human: false,
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

    #[test]
    fn header_counts_show_nonzero_states_and_omit_the_rest() {
        let counts = voro_core::StateCounts {
            proposed: 3,
            ready: 5,
            running: 2,
            needs_input: 1,
            review: 0,
            waiting: 0,
            stalled: 0,
            done: 0,
        };
        let line = counts_line(&counts);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("triage 3"), "{text}");
        assert!(text.contains("ready 5"), "{text}");
        assert!(text.contains("input 1"), "{text}");
        // Zero-count states never render, and `running` is not a header stat.
        assert!(!text.contains("review"), "{text}");
        assert!(!text.contains("waiting"), "{text}");
        assert!(!text.contains("stalled"), "{text}");
        assert!(!text.contains("done"), "{text}");
        assert!(!text.contains("running"), "{text}");

        // With no work anywhere the indicator collapses to nothing.
        assert_eq!(counts_line(&voro_core::StateCounts::default()).width(), 0);
    }

    #[test]
    fn header_renders_the_untriaged_count_alongside_projects() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        let new = |title: &str, state: TaskState| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
        };
        store.create_task(new("idea", TaskState::Proposed)).unwrap();
        store.create_task(new("go", TaskState::Ready)).unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|f| draw(f, &app)).unwrap();
        let header = terminal.backend().buffer().content()[..80]
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(header.contains("voro"), "header missing brand: {header}");
        assert!(
            header.contains("triage 1"),
            "header missing untriaged count: {header}"
        );
        assert!(
            header.contains("ready 1"),
            "header missing ready count: {header}"
        );
    }
}
