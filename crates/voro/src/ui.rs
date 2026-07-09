use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

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
            let area = popup_area(frame, 48, (items.len() as u16 + 3).max(4));
            let content = popup_block(
                frame,
                area,
                "Weights".to_string(),
                "0-5 weight · r rename · d delete · esc close",
            );
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items).highlight_style(SELECTED);
            frame.render_stateful_widget(list, content, &mut state);
        }
        Mode::AddProject {
            name,
            path,
            on_path,
            editing,
        } => {
            let area = popup_area(frame, 56, 5);
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
                Some(id) => format!("Edit project #{id}"),
                None => "New project".to_string(),
            };
            let content = popup_block(frame, area, title, "tab switch field · ⏎ save · esc cancel");
            let para = Paragraph::new(vec![
                field("name", name, !*on_path),
                field("path", path, *on_path),
            ]);
            frame.render_widget(para, content);
        }
        Mode::PickProject { sel } => {
            let items: Vec<ListItem> = app
                .projects
                .iter()
                .map(|p| ListItem::new(p.name.clone()))
                .collect();
            let area = popup_area(frame, 44, (items.len() as u16 + 3).max(4));
            let content = popup_block(
                frame,
                area,
                "Pick project".to_string(),
                "j/k move · ⏎ select · esc cancel",
            );
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items).highlight_style(SELECTED);
            frame.render_stateful_widget(list, content, &mut state);
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
            let area = popup_area(frame, 48, (items.len() as u16 + 3).max(4));
            let content = popup_block(
                frame,
                area,
                format!("Transition #{task_id}"),
                "j/k move · ⏎ select · esc close",
            );
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items).highlight_style(SELECTED);
            frame.render_stateful_widget(list, content, &mut state);
        }
        Mode::Prompt { kind, buffer, .. } => {
            let area = popup_area(frame, 64, 4);
            let content = popup_block(
                frame,
                area,
                kind.title().to_string(),
                "⏎ submit · esc cancel",
            );
            let para = Paragraph::new(Line::from(vec![Span::raw(format!("{buffer}▏"))]));
            frame.render_widget(para, content);
        }
        Mode::Detail { task_id, scroll } => {
            let Some(row) = app.all.iter().find(|r| r.task.id == *task_id) else {
                return;
            };
            let frame_area = frame.area();
            let width = frame_area.width.saturating_sub(8).clamp(30, 90);
            let height = frame_area.height.saturating_sub(4).clamp(8, 40);
            let area = popup_area(frame, width, height);
            let content = popup_block(
                frame,
                area,
                format!("#{task_id}"),
                "⏎ state · j/k scroll · esc close",
            );
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
            lines.push(Line::default());
            lines.extend(t.body.lines().map(|l| Line::from(l.to_string())));
            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0));
            frame.render_widget(para, content);
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
            let area = popup_area(frame, 44, (items.len() as u16 + 3).max(4));
            let content = popup_block(
                frame,
                area,
                format!("Dispatch #{task_id}"),
                "j/k move · ⏎ dispatch · esc cancel",
            );
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items).highlight_style(SELECTED);
            frame.render_stateful_widget(list, content, &mut state);
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
                Line::from(format!(
                    "state           {:>6}  (bonus +{})",
                    b.state.to_string(),
                    b.state_bonus
                )),
                Line::from(format!("base w×(p+s)    {:>6.1}", b.base)),
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
                    | voro_core::TaskState::Proposed
            ) {
                lines.push(Line::from(Span::styled(
                    format!("({state} tasks are not scheduled)"),
                    Style::new().dim(),
                )));
            }
            let area = popup_area(frame, 44, lines.len() as u16 + 3);
            let content = popup_block(frame, area, format!("Score of #{task_id}"), "esc close");
            frame.render_widget(Paragraph::new(lines), content);
        }
        Mode::History {
            task_id,
            events,
            scroll,
        } => {
            let frame_area = frame.area();
            let width = frame_area.width.saturating_sub(8).clamp(30, 100);
            let height = frame_area.height.saturating_sub(4).clamp(8, 40);
            let area = popup_area(frame, width, height);
            let content = popup_block(
                frame,
                area,
                format!("History of #{task_id}"),
                "j/k scroll · esc close",
            );
            let lines: Vec<Line> = if events.is_empty() {
                vec![Line::from(Span::styled(
                    "no events yet",
                    Style::new().dim(),
                ))]
            } else {
                events
                    .iter()
                    .map(|e| {
                        Line::from(vec![
                            Span::styled(format!("{:<19} ", e.at), Style::new().dim()),
                            Span::styled(format!("{:<10} ", e.kind), Style::new().bold()),
                            Span::raw(e.detail.clone().unwrap_or_default()),
                        ])
                    })
                    .collect()
            };
            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0));
            frame.render_widget(para, content);
        }
    }
}

fn draw_cockpit(frame: &mut Frame, app: &App) {
    // Regions are separated by whitespace and headed by a section label rather
    // than a titled border, so each height is the row count itself (min 1 to
    // keep the empty-queue hint, no border rows to add).
    let queue_height = (app.queue.len() as u16).clamp(1, 10);

    if app.running.is_empty() {
        // The running strip and its label collapse away when no session is
        // live, so the queue and detail pane keep the space (DESIGN.md §9).
        let [header, next_label, queue, _gap, detail, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(queue_height),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        draw_header(frame, app, header);
        frame.render_widget(section_label("Next", app.queue.len()), next_label);
        draw_queue(frame, app, queue);
        draw_detail(frame, app, detail);
        draw_status(frame, app, status);
    } else {
        let running_height = (app.running.len() as u16).clamp(1, 4);
        let [
            header,
            next_label,
            queue,
            _gap1,
            detail,
            _gap2,
            running_label,
            running,
            status,
        ] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(queue_height),
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(running_height),
            Constraint::Length(1),
        ])
        .areas(frame.area());

        draw_header(frame, app, header);
        frame.render_widget(section_label("Next", app.queue.len()), next_label);
        draw_queue(frame, app, queue);
        draw_detail(frame, app, detail);
        frame.render_widget(section_label("Running", app.running.len()), running_label);
        draw_running(frame, app, running);
        draw_status(frame, app, status);
    }
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
    let list = List::new(items).highlight_style(SELECTED);
    frame.render_stateful_widget(list, area, &mut state);
    if empty {
        frame.render_widget(Paragraph::new("nothing to do — press n").dim(), area);
    }
}

/// The body of whichever row is selected — the pane follows the selection
/// instead of holding its own concept of "the" task.
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
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
                None => return,
            }
        }
        None => return,
    };

    // The meta line heads the pane in place of a section label, so it leads
    // and the title follows it.
    let mut meta = vec![Span::raw(format!(
        "#{} · {} · {} · {}",
        task.id, project, task.priority, task.state
    ))];
    if let Some(total) = score {
        meta.push(Span::raw(" · "));
        meta.push(score_span(total));
    }
    let mut lines = vec![
        Line::from(meta),
        Line::from(Span::styled(task.title.clone(), Style::new().bold())),
    ];
    if let Some(q) = &task.question {
        lines.push(Line::from(Span::styled(
            format!("question: {q}"),
            Style::new().fg(Color::Cyan),
        )));
    }
    if app.redispatch.contains(&task.id) {
        lines.push(Line::from(redispatch_span()));
    }
    lines.push(Line::default());
    lines.extend(task.body.lines().map(|l| Line::from(l.to_string())));
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    frame.render_widget(para, area);
}

/// Live sessions (DESIGN.md §9): agent, task state, and elapsed time since
/// dispatch. `draw_cockpit` omits this region and its label entirely when
/// nothing is running, so it is not drawn at all in the common case.
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
            items.push(ListItem::new(Line::from(vec![
                Span::raw(format!("{} ", task_ref(r.task_id))),
                Span::styled(format!("{:8} ", r.agent), Style::new().fg(Color::Magenta)),
                Span::raw(format!("{:11} ", r.task_state.to_string())),
                Span::styled(
                    format!("{:>6}  ", format_elapsed(r.elapsed_secs)),
                    Style::new().dim(),
                ),
                Span::raw(r.task_title.clone()),
            ])));
        }
    }
    let mut state = ListState::default().with_selected(selected);
    let list = List::new(items).highlight_style(SELECTED);
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
    let [label, list_area, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    frame.render_widget(section_label("All tasks", app.all.len()), label);

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
            spans.extend(blocker_spans(r));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(if app.all.is_empty() {
        None
    } else {
        Some(app.tasks_sel)
    });
    let list = List::new(items).highlight_style(SELECTED);
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

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let line = match &app.status {
        Some(msg) => Line::from(Span::styled(msg.clone(), Style::new().fg(Color::Red))),
        None => {
            let mut hints = String::from("q quit · tab screen · j/k move");
            if let Some(enter) = app.enter_hint() {
                hints.push_str(" · ");
                hints.push_str(enter);
            }
            hints.push_str(
                " · n new · e edit · s state · d dispatch · D agent · o open · x score · h history · w weights · P project",
            );
            Line::from(Span::styled(hints, Style::new().dim()))
        }
    };
    frame.render_widget(line, area);
}

/// A section heading in the shared style — a bold title and a dim count,
/// e.g. `Next · 4` — used in place of a titled border to separate the
/// cockpit's regions and head the tasks list.
fn section_label(title: &str, count: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(title.to_string(), Style::new().bold()),
        Span::styled(format!(" · {count}"), Style::new().dim()),
    ])
}

/// Draw a popup's bordered block with a subject-only title and a dim key-help
/// footer on its last inner row, returning the content area above the footer.
fn popup_block(frame: &mut Frame, area: Rect, title: String, footer: &str) -> Rect {
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [content, footer_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer.to_string(),
            Style::new().dim(),
        ))),
        footer_area,
    );
    content
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
        store.apply(closed.id, Action::Complete).unwrap();
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

    #[test]
    fn non_parked_and_blockerless_rows_get_no_suffix() {
        assert!(
            blocker_spans(&row(TaskState::Ready, vec![blocker(4, TaskState::Done)])).is_empty()
        );
        assert!(blocker_spans(&row(TaskState::Parked, vec![])).is_empty());
    }
}
