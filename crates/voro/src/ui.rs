use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use voro_core::{
    ActionRow, CapReading, CompletionReport, DepKind, DepRef, DigestRow, EffectiveScore, Event,
    QueueRow, ScoreBreakdown, Session, SessionOutcome, StateCounts, TaskState,
};

use crate::app::{
    App, CockpitRow, Mode, Screen, TaskRow, ViewerFormState, ViewerOption, viewer_label,
};

const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// What a click at some point of the last-drawn frame means (DESIGN.md §9).
/// Each variant carries the index the click selects, counted in the same space
/// the key handlers count in — so routing a click is setting the field `j`/`k`
/// would set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A cockpit row, queue strip and running strip alike: an index into
    /// `App::cockpit_rows`, the one space `cockpit_sel` counts in.
    CockpitRow(usize),
    TaskRow(usize),
    ProjectRow(usize),
    ViewerRow(usize),
    /// An option of whichever modal picker is open.
    PickerOption(usize),
}

/// The click targets of one drawn frame. Built by [`draw`] and read by
/// `App::on_mouse`, which is what keeps the key handlers free of geometry: the
/// layout is resolved where it is already known, at draw time. Anything no rect
/// covers — the detail pane, the header, the status line, empty space — is a
/// dead zone where a click does nothing.
#[derive(Debug, Default)]
pub struct HitMap(Vec<(Rect, Hit)>);

impl HitMap {
    /// The rows of a bordered list, mapped through `hit`. `offset` is the scroll
    /// ratatui computed while rendering, so a scrolled list still maps a visible
    /// line to the item on it; lines past `count` items — the trailing read-only
    /// note some lists carry — get no target. Rows are one line tall throughout.
    fn push_list(&mut self, area: Rect, offset: usize, count: usize, hit: impl Fn(usize) -> Hit) {
        let inner = area.inner(Margin::new(1, 1));
        for line in 0..inner.height {
            let item = offset + line as usize;
            if item >= count {
                break;
            }
            self.0.push((
                Rect::new(inner.x, inner.y + line, inner.width, 1),
                hit(item),
            ));
        }
    }

    /// Drop every target, for when a modal takes the pointer.
    fn clear(&mut self) {
        self.0.clear();
    }

    pub fn at(&self, col: u16, row: u16) -> Option<Hit> {
        self.0
            .iter()
            .find(|(rect, _)| rect.contains((col, row).into()))
            .map(|(_, hit)| *hit)
    }
}

/// The canonical rendering of a task identifier, right-aligned for list columns.
fn task_ref(id: i64) -> String {
    format!("{:>4}", format!("#{id}"))
}

/// A compact one-line rendering of a possibly multi-line question, for the row
/// summaries where only a single line fits: the first line, with a trailing `…`
/// when there is more (the full text reads in the cockpit detail pane).
fn question_summary(question: &str) -> String {
    let mut lines = question.lines();
    let first = lines.next().unwrap_or("");
    if lines.next().is_some() {
        format!("{first}…")
    } else {
        first.to_string()
    }
}

pub fn draw(frame: &mut Frame, app: &App) -> HitMap {
    let mut hits = HitMap::default();
    match app.screen {
        Screen::Cockpit => draw_cockpit(frame, app, &mut hits),
        Screen::Tasks => draw_tasks(frame, app, &mut hits),
        Screen::Projects => draw_projects(frame, app, &mut hits),
        Screen::Config => draw_config(frame, app, &mut hits),
    }
    // A modal owns the pointer: the screen behind it keeps drawing but stops
    // being clickable, so only the popup's own options are targets.
    if !matches!(app.mode, Mode::Normal) {
        hits.clear();
    }
    draw_mode(frame, app, &mut hits);
    hits
}

fn draw_mode(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    match &app.mode {
        Mode::Normal => {}
        Mode::KeyMap { page } => draw_key_map(frame, app, *page),
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
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let title = match flow {
                crate::app::CreateFlow::Quick => "Project to propose a task in",
                crate::app::CreateFlow::Editor => "Project for the new task",
                crate::app::CreateFlow::Plan => "Project to plan a task in",
            };
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
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
            let count = items.len();
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
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
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
            let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} — ⏎ to submit, esc to cancel", kind.title())),
            );
            // Typed text never contains a newline (⏎ submits), so the box has
            // to be sized from the *wrapped* line count at the popup's inner
            // width — counting `\n`s would leave a one-row box with the tail of
            // a long note wrapped out of sight. `line_count` counts both, and
            // includes the block's two border rows.
            let width = 72.min(frame.area().width);
            let rendered_rows = para.line_count(width.saturating_sub(2)) as u16;
            let area = popup_area(frame, width, rendered_rows.clamp(3, 20));
            // Past the clamp the box stops growing, so scroll to the end of the
            // text: the cursor lives there, and the tail is what is being typed.
            let overflow = rendered_rows
                .saturating_sub(2)
                .saturating_sub(area.height.saturating_sub(2));
            frame.render_widget(para.scroll((overflow, 0)), area);
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
        Mode::QuickCreate { project_id, buffer } => {
            let area = popup_area(frame, 72, 3);
            let line = Line::from(vec![Span::raw(buffer.as_str()), Span::raw("▏")]);
            let project = app
                .projects
                .iter()
                .find(|p| p.id == *project_id)
                .map(|p| p.name.as_str())
                .unwrap_or("the project");
            let para = Paragraph::new(line).wrap(Wrap { trim: false }).block(
                Block::default().borders(Borders::ALL).title(format!(
                    "New task in {project} — ⏎ to propose, esc to cancel"
                )),
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
                Line::from(Span::raw("and open it in the browser")),
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
                    format!("question: {}", question_summary(q)),
                    Style::new().fg(Color::Cyan),
                )));
            }
            if let Some(pr) = &t.pr_url {
                lines.push(Line::from(pr_span(pr)));
            }
            if let Some(branch) = &t.branch {
                lines.push(Line::from(branch_span(branch)));
            }
            if let Some((name, path)) = app.task_repo(t) {
                lines.push(Line::from(repo_span(&name, &path)));
            }
            if t.human {
                lines.push(human_line());
            }
            if t.deep {
                lines.push(deep_line());
            }
            if let Some(session) = app.last_sessions.get(task_id) {
                lines.extend(session_lines(session, t.state));
            }
            lines.extend(doc_lines(app, *task_id));
            lines.extend(dep_lines(
                app.deps.get(task_id).map_or(&[][..], |v| v),
                app.dependents.get(task_id).map_or(&[][..], |v| v),
            ));
            if app.show_score
                && let Some(b) = app.score_breakdown(*task_id)
            {
                lines.extend(score_lines(&b, app.effective_score(t, b.total)));
            }
            lines.push(Line::default());
            lines.extend(crate::markdown::body_lines(&t.body));
            if app.show_history {
                lines.push(Line::default());
                lines.extend(history_lines(&app.task_events(*task_id)));
            }
            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "#{task_id} — ⏎ state · 0-3 priority · ! deep · c docs · x score · h history · j/k scroll · esc close"
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
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Dispatch #{task_id} — pick agent, ⏎ dispatch, esc cancel"
                )))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::DocPicker {
            task_id, docs, sel, ..
        } => {
            let items: Vec<ListItem> = docs
                .iter()
                .map(|doc| ListItem::new(doc_picker_row(app, *task_id, doc)))
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            // Resolved locations are absolute paths, so this picker takes what
            // the terminal will give rather than the fixed width the short-row
            // pickers use.
            let width = frame.area().width.saturating_sub(4).max(44);
            let area = popup_area(frame, width, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Documents for #{task_id} — ⏎ link/unlink, esc close"
                )))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::ViewerPicker {
            options,
            current,
            sel,
            ..
        } => {
            let items: Vec<ListItem> = options
                .iter()
                .map(|o| match o {
                    ViewerOption::Viewer(v) if v == current => {
                        ListItem::new(format!("{}  (current)", viewer_label(v.as_deref())))
                    }
                    ViewerOption::Viewer(v) => {
                        ListItem::new(viewer_label(v.as_deref()).to_string())
                    }
                    ViewerOption::NewViewer => ListItem::new(Line::from(Span::styled(
                        "new viewer…",
                        Style::new().fg(Color::Blue),
                    ))),
                })
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 52, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Viewer — ⏎ set, esc cancel"),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::ViewerForm(ViewerFormState {
            name,
            cmd,
            on_cmd,
            editing,
            cmd_tracks_name,
            ..
        }) => {
            let field = |label: &str, value: &str, active: bool| {
                let style = if active {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Line::from(vec![
                    Span::raw(format!("{label:>7}: ")),
                    Span::styled(format!("{value}▏"), style),
                ])
            };
            let area = popup_area(frame, 72, 4);
            let title = if *editing {
                format!("Edit viewer '{name}' — ⏎ to save, esc to cancel")
            } else {
                "New viewer — tab to switch, ⏎ to advance/save, esc to cancel".to_string()
            };
            // The name field is inert on an edit, so dim it to say so.
            let name_line = if *editing {
                Line::from(vec![
                    Span::raw("   name: "),
                    Span::styled(name.clone(), Style::new().dim()),
                ])
            } else {
                field("name", name, !*on_cmd)
            };
            // A command the form wrote is dim, focused or not, so that what was
            // typed and what was filled in never look alike.
            let mut cmd_style = Style::new();
            if *on_cmd {
                cmd_style = cmd_style.add_modifier(Modifier::REVERSED);
            }
            if *cmd_tracks_name {
                cmd_style = cmd_style.dim();
            }
            let cmd_line = Line::from(vec![
                Span::raw("command: "),
                Span::styled(format!("{cmd}▏"), cmd_style),
            ]);
            let para = Paragraph::new(vec![name_line, cmd_line])
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
        }
        Mode::DefaultPicker {
            kind,
            names,
            current,
            sel,
        } => {
            let items: Vec<ListItem> = names
                .iter()
                .map(|n| {
                    if Some(n) == current.as_ref() {
                        ListItem::new(format!("{n}  (current)"))
                    } else {
                        ListItem::new(n.clone())
                    }
                })
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let what = match kind {
                crate::app::DefaultKind::Agent => "default agent",
                crate::app::DefaultKind::Viewer => "default viewer",
            };
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Pick {what} — ⏎ set, esc cancel")),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
    }
}

/// The inline score decomposition (DESIGN.md §7) that `x` folds into a detail
/// view: one dim line breaking the total down, plus a "not scheduled" note
/// where the task's state keeps it out of the queue. Shared by the cockpit pane
/// and the tasks-screen Detail popup.
fn score_lines(b: &ScoreBreakdown, effective: Option<EffectiveScore>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "weight {} · {} (value {}) · {} (+{}) · blocks ×{} (+{}) · base w×(p+s+u) {:.1} · age {:.1}d (+{:.2})",
            b.weight,
            b.priority,
            b.priority_value,
            b.state,
            b.state_bonus,
            b.open_dependents,
            b.unblock_bonus,
            b.base,
            b.age_days,
            b.age_bonus
        ),
        Style::new().dim(),
    ))];
    // What the queue ranks by: the total priced by what the row asks of the
    // operator (DESIGN.md §7).
    if let Some(e) = effective {
        lines.push(Line::from(Span::styled(
            format!(
                "{:.2} ÷ {} ({}) = {:.2} effective",
                b.total,
                e.cost,
                e.action.as_str(),
                e.effective
            ),
            Style::new().dim(),
        )));
    }
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
                Span::raw(crate::cli::event_detail(e)),
            ])
        }));
    }
    lines
}

fn draw_cockpit(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let queue_rows = app
        .cockpit_rows
        .iter()
        .filter(|row| !matches!(row, CockpitRow::Running(_)))
        .count();
    let queue_height = (queue_rows as u16 + 2).clamp(3, 12);
    // Collapsed to nothing when no session is live, so the queue and detail
    // pane keep the space in the common case (DESIGN.md §9).
    let running_height = if app.running.is_empty() {
        0
    } else {
        (app.running.len() as u16 + 2).clamp(3, 10)
    };
    let [header, queue, detail, running, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(queue_height),
        Constraint::Min(5),
        Constraint::Length(running_height),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    draw_header(frame, app, header);
    draw_queue(frame, app, queue, hits);
    draw_detail(frame, app, detail);
    draw_running(frame, app, running, hits);
    draw_status(frame, app, status);
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled("voro", Style::new().bold()), Span::raw("  ")];
    // Archived projects have left the cockpit (DESIGN.md §5); the projects
    // screen is where they remain visible.
    for p in app.projects.iter().filter(|p| !p.archived) {
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
    // A proposal being refined has left the triage count, so it is named here
    // rather than silently missing from the backlog the header exists to keep
    // felt (DESIGN.md §6/§12).
    push("refining", counts.refining, dim);
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

/// The incomplete-report flag (DESIGN.md §8): a `review` task carrying a branch
/// and no summary. Yellow to match the running strip's "no live
/// session" warning, since both are anomalies needing the operator.
fn incomplete_report_span() -> Span<'static> {
    Span::styled(
        "  [incomplete report]",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// The refine marker (DESIGN.md §6): a proposal whose last refine round
/// reworked its body against the operator's note, so this row is an improved
/// version awaiting a fresh verdict. A property of the row rather than an
/// anomaly — cyan like the question text, not the warning yellow.
fn refined_span() -> Span<'static> {
    Span::styled("  ↻ refined", Style::new().fg(Color::Cyan))
}

/// Its counterpart (DESIGN.md §6): a proposal whose last refine round died
/// without rewriting anything. Red rather than cyan, because the body the
/// operator is about to read is the *old* one and the rewrite they asked for
/// never happened — an absence they should never have to notice for themselves.
fn refine_failed_span() -> Span<'static> {
    Span::styled(
        "  ⚠ refine failed",
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    )
}

/// A refine in flight on the running strip (DESIGN.md §9), where it sits beside
/// dispatched work: same columns, but named for what it is, since the keys that
/// act on a dispatch do not act on this.
fn refining_span() -> Span<'static> {
    Span::styled(
        format!("{:11} ", "⟳ refining"),
        Style::new().fg(Color::Cyan),
    )
}

/// A hand-off on the running strip (DESIGN.md §9): work in flight that someone
/// else owns, sitting beside the work an agent owns. Blue rather than the
/// refine's cyan, because nothing here is being typed into — the elapsed time
/// beside it counts from the hand-off, not from any session.
///
/// Padded one narrower than the other state labels: the hourglass is a
/// double-width glyph, so nine characters here occupy the same eleven columns
/// the state column holds everywhere else.
fn waiting_span() -> Span<'static> {
    Span::styled(
        format!("{:10} ", "⏳ waiting"),
        Style::new().fg(Color::Blue),
    )
}

/// What a waiting strip row is holding up (DESIGN.md §7): its direct open
/// `blocks` dependents, counted by the rule the `unblock_bonus` uses. A waiting
/// task earns no score, so the count cannot reach the operator through the
/// queue — this badge is the only place the gating shows.
fn blocks_span(open_dependents: usize) -> Span<'static> {
    Span::styled(
        format!("  blocks {open_dependents}"),
        Style::new().fg(Color::Yellow),
    )
}

/// A tracked PR on a waiting strip row — presence only, never its state, which
/// Voro does not poll (DESIGN.md §8).
fn strip_pr_span() -> Span<'static> {
    Span::styled("  PR", Style::new().fg(Color::Magenta))
}

/// The usage-cap badge on a running strip row (DESIGN.md §8): this session is
/// alive but held at a cap, doing nothing until the window reopens.
///
/// Yellow rather than red, and no state change behind it, because nothing has
/// gone wrong — the session is intact and will pick up where it left off. It is
/// the *absence* of this badge on a stuck row that used to mislead: capped work
/// sat on the strip with a climbing elapsed time and no way to tell it from work
/// in progress.
///
/// Three shapes, in decreasing order of what Voro managed to learn. With a
/// parsed reset time still ahead, `⚠ capped ↻21:50` — the operator can decide
/// whether to wait. Past that time the window is open and the session is merely
/// waiting to be nudged, which is a different situation and a different thing
/// to do about it, so it says so. With no time parsed at all, the bare badge:
/// the cap is the part worth knowing, and suppressing it for want of a
/// timestamp would trade the whole signal for a detail.
fn capped_span(reading: &CapReading, now_minutes: Option<u16>) -> Span<'static> {
    let past = now_minutes.is_some_and(|now| reading.reset_passed(now));
    let text = match (reading.reset_label(), past) {
        (_, true) => "  ⚠ capped · reset passed".to_string(),
        (Some(at), false) => format!("  ⚠ capped ↻{at}"),
        (None, false) => "  ⚠ capped".to_string(),
    };
    Span::styled(
        text,
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// The stale-branch marker (DESIGN.md §8): a review task whose tracked PR
/// reports a merge conflict, probed on demand for the selected task. Purely
/// informational — it flags that the branch needs resolving before it can
/// merge — the same shout as `[incomplete report]`.
fn conflict_span() -> Span<'static> {
    Span::styled(
        "  [branch conflicts]",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// A queue row's state cell, coloured by what the state asks of the operator.
/// Only the states that are stuck on them take a colour: `needs-input` cyan,
/// the hue the agent already speaks in throughout the TUI (the question suffix,
/// `↻ refined`, `⟳ refining`); `review` green, matching the branch and repo
/// lines in the detail pane, since a git artifact is what there is to look at;
/// `stalled` red, matching the failed-session line. `ready` stays plain so an
/// uncoloured queue reads as nothing waiting on the operator, and `proposed`
/// stays dim with the rest of its row. No bold — that is reserved for the row
/// markers, which sit above the state cell in the hierarchy.
fn state_span(state: TaskState) -> Span<'static> {
    let style = match state {
        TaskState::NeedsInput => Style::new().fg(Color::Cyan),
        TaskState::Review => Style::new().fg(Color::Green),
        TaskState::Stalled => Style::new().fg(Color::Red),
        TaskState::Proposed => Style::new().dim(),
        _ => Style::new(),
    };
    Span::styled(format!("{:11}", state.as_str()), style)
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

/// The deep flag (task #241) as a one-column row marker sitting beside the
/// priority cell: `!` when the task dispatches on the agent's strongest model,
/// a blank of the same width otherwise, so the columns after it stay aligned
/// whether or not any row in the list is deep.
fn deep_marker(deep: bool) -> Span<'static> {
    if deep {
        Span::styled(
            "!",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(" ")
    }
}

/// The same flag spelled out for a detail view, beside the human line.
fn deep_line() -> Line<'static> {
    Line::from(Span::styled(
        "deep — dispatches on the agent's strongest model",
        Style::new().fg(Color::Magenta),
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

/// The checkout a task runs in (DESIGN.md §3), rendered only when the task
/// names a repo of its own — a task on the project default reads as it always
/// did, so the line appears exactly when it carries information.
fn repo_span(name: &str, path: &str) -> Span<'static> {
    Span::styled(
        format!("repo: {name} ({path})"),
        Style::new().fg(Color::Green),
    )
}

/// The plans a task derives from (DESIGN.md §3), one line each: the title when
/// the document carries one, then where it resolves to — the same location
/// dispatch names in the agent's prompt. A task citing no document renders
/// nothing.
fn doc_lines(app: &App, task_id: i64) -> Vec<Line<'static>> {
    app.docs
        .get(&task_id)
        .map_or(&[][..], |v| v)
        .iter()
        .map(|doc| {
            let location = app
                .doc_locations
                .get(&doc.id)
                .cloned()
                .unwrap_or_else(|| doc.location.clone());
            let text = match &doc.title {
                Some(title) => format!("doc: {title} — {location}"),
                None => format!("doc: {location}"),
            };
            Line::from(Span::styled(text, Style::new().fg(Color::Magenta)))
        })
        .collect()
}

/// One row of the document picker (DESIGN.md §8): a tick for the documents the
/// task already cites, then the same title-and-location pair the detail panes
/// show, so a link made here reads back identically. A document owned by
/// another project carries that project's name, since the list spans them all
/// and two plans can share a filename.
fn doc_picker_row(app: &App, task_id: i64, doc: &voro_core::Doc) -> Line<'static> {
    let linked = app.doc_linked(task_id, doc.id);
    let owner = app
        .all
        .iter()
        .find(|row| row.task.id == task_id)
        .filter(|row| row.task.project_id != doc.project_id)
        .and_then(|_| app.projects.iter().find(|p| p.id == doc.project_id))
        .map(|p| format!("[{}] ", p.name))
        .unwrap_or_default();
    let location = app
        .doc_locations
        .get(&doc.id)
        .cloned()
        .unwrap_or_else(|| doc.location.clone());
    let text = match &doc.title {
        Some(title) => format!("{owner}{title} — {location}"),
        None => format!("{owner}{location}"),
    };
    let (mark, style) = if linked {
        ("✓ ", Style::new().fg(Color::Magenta))
    } else {
        ("  ", Style::new().dim())
    };
    Line::from(vec![Span::styled(mark, style), Span::styled(text, style)])
}

/// A review row's next action rendered as a browser suffix (DESIGN.md §3). The
/// browser shows state in its own column, so only `review` — whose verb reads
/// the tracked PR, not the state alone — earns the suffix.
fn review_next_span(app: &App, task: &voro_core::Task) -> Option<Span<'static>> {
    if task.state != voro_core::TaskState::Review {
        return None;
    }
    let verb = app.advertised_action(task)?;
    Some(Span::styled(
        format!("  next: {verb}"),
        Style::new().fg(Color::Blue),
    ))
}

/// What the agent reported (DESIGN.md §8): the completion summary of the cycle
/// in hand, rendered above the body — the body is the instruction that has
/// already been carried out, the summary is the account of carrying it out, and
/// the account is what a verdict is given against. It is the only such account
/// a project with no PR and no configured viewer has, so the card cannot leave
/// it to `voro show`. On a rework it is headed by the feedback it answers,
/// which is the other half of making a re-review proportional to the fix — the
/// operator reads *what the agent says it changed* here and the diff since the
/// rejected revision beside it.
fn completion_lines(report: &CompletionReport, width: u16) -> Vec<Line<'static>> {
    let heading = match report.feedback {
        Some(_) => "response to the review feedback:",
        None => "completion summary:",
    };
    let mut lines = vec![Line::default()];
    lines.extend(agent_voice_block(heading, &report.summary, width));
    lines
}

/// The gutter that marks a block as the agent's own words rather than the
/// operator's instruction. Two columns wide, repeated on every visual line.
const GUTTER: &str = "│ ";

/// An agent-authored block — a completion summary, its rework variant, or a
/// question — rendered as markdown behind a quote-style gutter (task #430).
/// The content is styled exactly as a task body is, so cyan text means inline
/// code here as it does there; the voice is carried by the bar in the margin
/// instead of by a colour wash. Lines are wrapped to fit inside the gutter
/// before it is prefixed, because the card's `Paragraph` re-wraps a long line
/// without repeating anything and would leave the bar broken part-way down.
fn agent_voice_block(heading: &str, text: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        heading.to_string(),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(crate::markdown::body_lines(text));
    let inner = (width as usize).saturating_sub(GUTTER.chars().count());
    crate::markdown::wrap_lines(lines, inner)
        .into_iter()
        .map(|line| {
            // A blank line keeps a bare bar, so the block reads as one column
            // for its full height rather than as fragments.
            let bar = if line.width() == 0 {
                GUTTER.trim_end()
            } else {
                GUTTER
            };
            let mut spans = vec![Span::styled(bar, Style::new().fg(Color::Cyan))];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
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

/// One task's queue row: the effective score it was ranked by, its state, and
/// the markers the row carries. Shared by the top-level rows and the proposals
/// listed under an expanded digest, which differ only in indent and dimming.
fn action_row_line(app: &App, row: &ActionRow, indent: &str) -> Line<'static> {
    let c = &row.candidate;
    let untriaged = c.task.state == TaskState::Proposed;
    let style = if untriaged {
        Style::new().dim()
    } else {
        Style::new()
    };
    let score = if untriaged {
        Span::styled(format!("{:5.1} ", row.effective), style)
    } else {
        score_span(row.effective)
    };
    let mut spans = vec![
        score,
        Span::styled(format!("{indent}{} ", task_ref(c.task.id)), style),
        state_span(c.task.state),
        Span::styled(format!(" {}", c.task.priority), style),
        deep_marker(c.task.deep),
        Span::styled(format!(" {}: {}", c.project_name, c.task.title), style),
    ];
    if c.task.human {
        spans.push(human_span());
    }
    if let Some(q) = &c.task.question {
        spans.push(Span::styled(
            format!("  — {}", question_summary(q)),
            Style::new().fg(Color::Cyan),
        ));
    }
    if app.refined.contains(&c.task.id) {
        spans.push(refined_span());
    }
    if app.refine_failed.contains(&c.task.id) {
        spans.push(refine_failed_span());
    }
    if app.incomplete_report.contains(&c.task.id) {
        // A PR cannot be opened from a half-finished report, and
        // nothing else on the row says so, so name the gap.
        spans.push(incomplete_report_span());
    }
    Line::from(spans)
}

/// A project's collapsed proposals (DESIGN.md §7): one row scored as its best
/// child, so a triage backlog stays felt without swamping the queue.
fn digest_line(app: &App, digest: &DigestRow) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{:5.1} ", digest.effective), Style::new().dim()),
        Span::styled(
            format!(
                "▲ {} awaiting triage ({})",
                pluralise(digest.tasks.len(), "proposal"),
                digest.project_name
            ),
            Style::new().fg(Color::Yellow),
        ),
    ];
    // A collapsed digest hides its constituents' own markers, so the count of
    // reworked bodies rides the summary row (DESIGN.md §6) — otherwise a refine
    // that has landed is invisible until the digest is folded open.
    let refined = digest
        .tasks
        .iter()
        .filter(|row| app.refined.contains(&row.candidate.task.id))
        .count();
    if refined > 0 {
        spans.push(Span::styled(
            format!("  ↻ {refined} refined"),
            Style::new().fg(Color::Cyan),
        ));
    }
    let failed = digest
        .tasks
        .iter()
        .filter(|row| app.refine_failed.contains(&row.candidate.task.id))
        .count();
    if failed > 0 {
        spans.push(Span::styled(
            format!("  ⚠ {failed} refine failed"),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn pluralise(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn draw_queue(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    // The pane skips the running rows, so a rendered line stands for the
    // cockpit row this records, not for its own position.
    let mut rows: Vec<usize> = Vec::new();
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        let item = match row {
            CockpitRow::Queue(idx) => match app.queue.rows.get(*idx) {
                Some(QueueRow::Action(row)) => ListItem::new(action_row_line(app, row, "")),
                Some(QueueRow::Digest(digest)) => ListItem::new(digest_line(app, digest)),
                None => continue,
            },
            CockpitRow::Proposal(i, j) => match app.digest_child(*i, *j) {
                Some(row) => ListItem::new(action_row_line(app, row, "  ↳ ")),
                None => continue,
            },
            CockpitRow::Running(_) => continue,
        };
        if i == app.cockpit_sel {
            selected = Some(items.len());
        }
        rows.push(i);
        items.push(item);
    }
    let empty = items.is_empty();
    let mut state = ListState::default().with_selected(selected);
    let mut block = Block::default().borders(Borders::ALL).title("Next");
    // The gate suppressed every dispatch row, so the pane says why rather than
    // reading as "nothing startable" (DESIGN.md §7).
    if let Some(gate) = app.queue.at_capacity {
        block = block.title_top(
            Line::from(Span::styled(
                format!(
                    " ⏸ dispatch at capacity ({}/{} running) ",
                    gate.running, gate.max_running
                ),
                Style::new().fg(Color::Yellow),
            ))
            .right_aligned(),
        );
    }
    let list = List::new(items).block(block).highlight_style(SELECTED);
    frame.render_stateful_widget(list, area, &mut state);
    hits.push_list(area, state.offset(), rows.len(), |i| {
        Hit::CockpitRow(rows[i])
    });
    if empty {
        let inner = area.inner(ratatui::layout::Margin::new(1, 1));
        // The cockpit is gated behind having a project (DESIGN.md §9), so an
        // empty queue here is always the drained one and `n` is always the way
        // to fill it.
        frame.render_widget(Paragraph::new("nothing to do — press n").dim(), inner);
    }
}

/// The detail pane's view of a digest row: the proposals it stands for, so the
/// operator can read the backlog without folding it open first.
fn digest_detail_lines(digest: &DigestRow) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "{} awaiting triage in {}",
                pluralise(digest.tasks.len(), "proposal"),
                digest.project_name
            ),
            Style::new().bold(),
        )),
        Line::from(Span::styled(
            "⏎ folds the digest open, so each proposal can be triaged in place",
            Style::new().dim(),
        )),
        Line::default(),
    ];
    lines.extend(digest.tasks.iter().map(|row| {
        Line::from(vec![
            Span::styled(format!("{:5.1} ", row.effective), Style::new().dim()),
            Span::raw(format!(
                "{} {} {}",
                task_ref(row.candidate.task.id),
                row.candidate.task.priority,
                row.candidate.task.title
            )),
        ])
    }));
    lines
}

/// The body of whichever row is selected — the pane follows the selection
/// instead of holding its own concept of "the" task.
fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Detail");

    let selected = app.cockpit_rows.get(app.cockpit_sel);
    let (task, project, score) = match selected {
        Some(CockpitRow::Queue(i)) => match app.queue.rows.get(*i) {
            Some(QueueRow::Action(row)) => (
                &row.candidate.task,
                row.candidate.project_name.as_str(),
                Some(row.effective),
            ),
            Some(QueueRow::Digest(digest)) => {
                let para = Paragraph::new(digest_detail_lines(digest))
                    .wrap(Wrap { trim: false })
                    .block(block);
                frame.render_widget(para, area);
                app.detail_max_scroll.set(0);
                return;
            }
            None => {
                frame.render_widget(Paragraph::new("").block(block), area);
                return;
            }
        },
        Some(CockpitRow::Proposal(i, j)) => match app.digest_child(*i, *j) {
            Some(row) => (
                &row.candidate.task,
                row.candidate.project_name.as_str(),
                Some(row.effective),
            ),
            None => {
                frame.render_widget(Paragraph::new("").block(block), area);
                return;
            }
        },
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

    // The gutter blocks below wrap themselves, so they need the width the
    // paragraph would have wrapped them at; the scroll clamp reuses it.
    let inner = block.inner(area);

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
    // The detail pane is where the operator reads the full question before
    // answering it in-session (DESIGN.md §6), so it renders whole.
    let mut agent_voice = false;
    if let Some(q) = &task.question {
        lines.extend(agent_voice_block("question:", q, inner.width));
        agent_voice = true;
    }
    if app.refined.contains(&task.id) {
        lines.push(Line::from(refined_span()));
    }
    if app.refine_failed.contains(&task.id) {
        lines.push(Line::from(refine_failed_span()));
    }
    if let Some(pr) = &task.pr_url {
        lines.push(Line::from(pr_span(pr)));
        // A review branch that no longer merges with the base (DESIGN.md §8):
        // the on-demand probe for this selection, shown beside its PR link.
        if app
            .conflict_selected
            .is_some_and(|(cid, conflicts)| cid == task.id && conflicts)
        {
            lines.push(Line::from(conflict_span()));
        }
    } else {
        // A review task with a branch and no summary withholds `pr`, which
        // would fail, and says what is needed instead; a checkout that
        // advertises `open` keeps its recommendation and wears the marker
        // under it, since reading the diff needs no summary (DESIGN.md §8).
        if let Some(verb) = app.advertised_action(task) {
            let hint = match verb {
                voro_core::NextAction::Pr => "  (g opens one from the summary)",
                voro_core::NextAction::Open => "  (o shows the diff in a viewer)",
                _ => "",
            };
            lines.push(Line::from(Span::styled(
                format!("next: {verb}{hint}"),
                Style::new().fg(Color::Blue),
            )));
        }
        if app.incomplete_report.contains(&task.id) {
            lines.push(Line::from(incomplete_report_span()));
        }
    }
    if let Some(branch) = &task.branch {
        lines.push(Line::from(branch_span(branch)));
    }
    if let Some((name, path)) = app.task_repo(task) {
        lines.push(Line::from(repo_span(&name, &path)));
    }
    if task.human {
        lines.push(human_line());
    }
    if task.deep {
        lines.push(deep_line());
    }
    if let Some(session) = app.last_sessions.get(&task.id) {
        lines.extend(session_lines(session, task.state));
    }
    lines.extend(doc_lines(app, task.id));
    lines.extend(dep_lines(
        app.deps.get(&task.id).map_or(&[][..], |v| v),
        app.dependents.get(&task.id).map_or(&[][..], |v| v),
    ));
    if app.show_score
        && let Some(b) = app.score_breakdown(task.id)
    {
        lines.extend(score_lines(&b, app.effective_score(task, b.total)));
    }
    // Only where the report is the thing being acted on: a task under review,
    // or handed off for someone else to review (DESIGN.md §8). Once a verdict
    // has been given the summary is history, and the card is read for the body.
    if matches!(task.state, TaskState::Review | TaskState::Waiting)
        && let Some(report) = app.completion_report(task.id)
    {
        lines.extend(completion_lines(&report, inner.width));
        agent_voice = true;
    }
    lines.push(Line::default());
    // Name the body only when a block above it speaks in the agent's voice, so
    // the reader knows which voice they are in; a heading over the only content
    // on the card would be noise.
    if agent_voice && !task.body.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "task:",
            Style::new().add_modifier(Modifier::BOLD),
        )));
    }
    lines.extend(crate::markdown::body_lines(&task.body));
    if app.show_history {
        lines.push(Line::default());
        lines.extend(history_lines(&app.task_events(task.id)));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });

    // Measure the wrapped body height against the inner area to clamp the
    // scroll and decide whether to advertise it. `line_count` wants the text
    // width, so pass the inner width with the block off this measuring paragraph.
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

/// Work in flight that someone else owns (DESIGN.md §9): agent, task state, and
/// elapsed time — dispatched `running` tasks, the refine rounds rewriting a
/// proposal's body, and the `waiting` hand-offs whose owner is a person rather
/// than an agent. `draw_cockpit` collapses this to a zero-height area when
/// nothing is in flight.
fn draw_running(frame: &mut Frame, app: &App, area: Rect, hits: &mut HitMap) {
    if area.height == 0 {
        return;
    }
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    let mut rows: Vec<usize> = Vec::new();
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        if let CockpitRow::Running(idx) = row {
            let r = &app.running[*idx];
            if i == app.cockpit_sel {
                selected = Some(items.len());
            }
            rows.push(i);
            let agent = match &r.agent {
                Some(agent) => Span::styled(format!("{agent:8} "), Style::new().fg(Color::Magenta)),
                None => Span::styled(format!("{:8} ", "—"), Style::new().dim()),
            };
            let waiting = r.task_state == TaskState::Waiting;
            let state = match r.task_state {
                TaskState::Refining => refining_span(),
                TaskState::Waiting => waiting_span(),
                other => Span::raw(format!("{other:11} ")),
            };
            let mut spans = vec![
                Span::raw(format!("{} ", task_ref(r.task_id))),
                agent,
                state,
                Span::styled(
                    format!("{:>6}  ", format_elapsed(r.elapsed_secs)),
                    Style::new().dim(),
                ),
                Span::raw(r.task_title.clone()),
            ];
            if waiting {
                let open = app.dependents.get(&r.task_id).map_or(0, |d| {
                    d.iter()
                        .filter(|d| d.kind == DepKind::Blocks && d.is_open())
                        .count()
                });
                if open > 0 {
                    spans.push(blocks_span(open));
                }
                if r.pr_url.is_some() {
                    spans.push(strip_pr_span());
                }
            }
            if let Some(reading) = app.caps.get(&r.task_id) {
                spans.push(capped_span(reading, app.now_minutes));
            }
            // A hand-off has nothing left to be live: the work is with someone
            // else, so a closed session is the expected shape, not an orphan.
            if r.session_id.is_none() && !waiting {
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
    hits.push_list(area, state.offset(), rows.len(), |i| {
        Hit::CockpitRow(rows[i])
    });
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

fn draw_tasks(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let [list_area, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

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
            let mut spans = vec![
                Span::styled(
                    format!(
                        "{} {:11} {}",
                        task_ref(r.task.id),
                        r.task.state,
                        r.task.priority,
                    ),
                    style,
                ),
                deep_marker(r.task.deep),
                Span::styled(
                    format!(" w{} {:14} {}", r.weight, r.project, r.task.title),
                    style,
                ),
            ];
            if r.task.human {
                spans.push(human_span());
            }
            if app.refined.contains(&r.task.id) {
                spans.push(refined_span());
            }
            if app.refine_failed.contains(&r.task.id) {
                spans.push(refine_failed_span());
            }
            if let Some(span) = review_next_span(app, &r.task) {
                spans.push(span);
            }
            if app.incomplete_report.contains(&r.task.id) {
                spans.push(incomplete_report_span());
            }
            spans.extend(blocker_spans(r));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let empty = items.is_empty();
    let mut state =
        ListState::default().with_selected(if empty { None } else { Some(app.tasks_sel) });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("All tasks"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, list_area, &mut state);
    hits.push_list(list_area, state.offset(), app.all.len(), Hit::TaskRow);
    if empty {
        let inner = list_area.inner(ratatui::layout::Margin::new(1, 1));
        // Like the cockpit's, this box has only one case to explain: the
        // browser is gated behind having a project (DESIGN.md §9), so `n` can
        // always create one.
        frame.render_widget(
            Paragraph::new("no tasks yet — press n to add one").dim(),
            inner,
        );
    }
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
/// path, open task count, and the viewer when the project names one (§8). The
/// open count is the project's non-terminal tasks, from the loaded task list.
/// An archived project stays on this screen, dim and tagged, so it can be
/// found and unarchived (§5).
fn draw_projects(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let [list_area, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .map(|p| {
            let open = app
                .all
                .iter()
                .filter(|r| r.task.project_id == p.id && !r.task.state.is_terminal())
                .count();
            let style = if p.weight == 0 || p.archived {
                Style::new().dim()
            } else {
                Style::new()
            };
            let viewer = match &p.viewer {
                Some(name) => format!("  [viewer:{name}]"),
                None => String::new(),
            };
            let archived = if p.archived { "  [archived]" } else { "" };
            // The path column shows the default repo, so a single-repo project
            // reads as it always did; extra checkouts are tagged (DESIGN.md §3).
            let extra = match app.repo_count(p.id) {
                0 | 1 => String::new(),
                n => format!("  [+{} repo(s)]", n - 1),
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{:>2}  {:14} {:28} {} open{extra}{viewer}{archived}",
                    p.weight,
                    p.name,
                    app.project_path(p.id),
                    open
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
    hits.push_list(
        list_area,
        state.offset(),
        app.projects.len(),
        Hit::ProjectRow,
    );
    if empty {
        let inner = list_area.inner(ratatui::layout::Margin::new(1, 1));
        frame.render_widget(
            Paragraph::new("no projects yet — press a to add one").dim(),
            inner,
        );
    }
    draw_status(frame, app, status);
}

/// The Config screen (DESIGN.md §5): the effective `voro.toml` surface. Agents
/// (read-only) with provenance and the default marked, over the viewers — the
/// built-ins and the user's tables, each with its provenance, the user's
/// editable — with the legacy anonymous `[viewer]` shown read-only beneath
/// them. A file that failed to parse is surfaced here rather than rendering
/// empty.
fn draw_config(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let [main, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    if let Some(err) = &app.config_error {
        let para = Paragraph::new(vec![
            Line::from(Span::styled(
                "voro.toml could not be read:",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::raw(err.clone())),
            Line::from(Span::styled(
                format!("path: {}", app.config_path().display()),
                Style::new().dim(),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Config"));
        frame.render_widget(para, main);
        draw_status(frame, app, status);
        return;
    }

    // Agents: one line each (default starred, verbs listed), plus a warning line
    // where an override drops built-in verbs (DESIGN.md §8).
    let mut agent_lines: Vec<Line> = Vec::new();
    for a in &app.config_agents {
        let marker = if a.is_default { "* " } else { "  " };
        let verbs = if a.verbs.is_empty() {
            String::new()
        } else {
            format!("  [{}]", a.verbs.join(" "))
        };
        // What `{model}` resolves to, so the placeholder in the command line
        // below reads without opening voro.toml.
        let models = match &a.models {
            None => String::new(),
            Some((model, deep, plan)) => {
                format!("  <model {model} · deep {deep} · plan {plan}>")
            }
        };
        agent_lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<10}", a.name), Style::new().bold()),
            Span::styled(format!(" {:<14}", a.provenance), Style::new().dim()),
            Span::raw(verbs),
            Span::styled(models, Style::new().dim()),
        ]));
        // The dispatch command on a dim continuation line (clipped to the pane),
        // so the row shows what each agent actually runs.
        agent_lines.push(Line::from(Span::styled(
            format!("    {}", a.dispatch),
            Style::new().dim(),
        )));
        if !a.missing_verbs.is_empty() {
            agent_lines.push(Line::from(Span::styled(
                format!("    ! override drops: {}", a.missing_verbs.join(", ")),
                Style::new().fg(Color::Yellow),
            )));
        }
    }
    if agent_lines.is_empty() {
        agent_lines.push(Line::from(Span::styled(
            "no agents configured",
            Style::new().dim(),
        )));
    }

    let agents_h = (agent_lines.len() as u16 + 2).clamp(3, 14);
    let [agents_area, viewers_area] =
        Layout::vertical([Constraint::Length(agents_h), Constraint::Min(3)]).areas(main);

    let agents = Paragraph::new(agent_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Agents (read-only — * default)"),
    );
    frame.render_widget(agents, agents_area);

    // Viewers: every viewer `open` can run — the built-ins with the user's
    // tables layered over them, each carrying its provenance like the agents
    // pane above, the default starred — then the anonymous [viewer] as a
    // read-only trailing note when present. A built-in row is dimmed, since
    // e/d refuse it: it is overridden, not edited.
    let mut viewer_items: Vec<ListItem> = Vec::new();
    for v in &app.config_viewers {
        let marker = if v.is_default { "* " } else { "  " };
        let name = if v.editable {
            Span::raw(format!("{:<14}", v.name))
        } else {
            Span::styled(format!("{:<14}", v.name), Style::new().dim())
        };
        viewer_items.push(ListItem::new(Line::from(vec![
            Span::raw(marker),
            name,
            Span::styled(format!("{:<14}", v.provenance), Style::new().dim()),
            Span::styled(v.cmd.clone(), Style::new().dim()),
        ])));
    }
    let named = app.config_viewers.len();
    if let Some(cmd) = &app.config_anon_viewer {
        viewer_items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("  {:<14}", "[viewer]"), Style::new().dim()),
            Span::styled(
                format!("{cmd}  (anonymous — name it in voro.toml to edit)"),
                Style::new().dim(),
            ),
        ])));
    }
    // The selection only ever lands on a named viewer, never the anonymous note.
    let selected = if named == 0 {
        None
    } else {
        Some(app.config_sel)
    };
    let mut state = ListState::default().with_selected(selected);
    let viewers = List::new(viewer_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Viewers — a add · e edit · d delete · V default · A default agent")
                .title_bottom(
                    Line::from(format!(" {} ", app.config_path().display())).right_aligned(),
                ),
        )
        .highlight_style(SELECTED);
    frame.render_stateful_widget(viewers, viewers_area, &mut state);
    // Same rule as the selection: the anonymous note below the named viewers is
    // not selectable, so it is not clickable either.
    hits.push_list(viewers_area, state.offset(), named, Hit::ViewerRow);

    draw_status(frame, app, status);
}

/// How many lines the status region needs at this frame size (DESIGN.md §9).
/// The key line is always one; a message takes as many as it wraps to, up to
/// half the screen — a message Voro cannot fit in half a terminal is past the
/// point where growing the region further helps.
fn status_height(app: &App, area: Rect) -> u16 {
    let Some(msg) = &app.status else {
        return 1;
    };
    let needed = wrap_status(msg, area.width).len() as u16;
    needed.clamp(1, (area.height / 2).max(1))
}

/// Greedy word wrap for the status line. Voro's errors end with the actionable
/// half — the key to press instead — so truncating them at the pane width hides
/// exactly the part worth reading (DESIGN.md §9). Wrapping here rather than
/// through `Wrap` keeps the drawn line count knowable before the layout is
/// split, so the region can be sized to the message. A word wider than the pane
/// gets its own line and truncates there, having nowhere else to go.
fn wrap_status(msg: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    for paragraph in msg.split('\n') {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let fits = Span::raw(current.as_str()).width() + 1 + Span::raw(word).width() <= width;
            if !current.is_empty() && !fits {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    lines
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    // A red status message overrides the key line, as before.
    if let Some(msg) = &app.status {
        let lines: Vec<Line> = wrap_status(msg, area.width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Red))))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
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

/// Whether the selection is a brief refine can still rewrite — a proposal or a
/// ready task (DESIGN.md §6).
fn selection_is_refinable(app: &App) -> bool {
    app.selected_task_id()
        .is_some_and(|id| app.is_refinable(id))
}

/// Every slot the current screen's key line can hold, each flagged with whether
/// this selection earns it. [`key_hints`] is this list filtered, so the two can
/// never disagree about which keys the line advertises — which is what lets the
/// drift test check the whole set against [`key_map`] from one App.
fn hint_candidates(app: &App) -> Vec<(&'static str, &'static str, bool)> {
    // `enter_hint` yields "⏎ <verb>"; split the glyph from the verb so the
    // glyph renders as the bold key and the verb as the dim label.
    let enter = app.enter_hint().and_then(|h| h.split_once(' '));
    let enter = ("⏎", enter.map_or("act", |(_, verb)| verb), enter.is_some());
    match app.screen {
        Screen::Cockpit => vec![
            enter,
            ("d/D", "dispatch", app.selected_can_dispatch()),
            ("r/R", "refine", selection_is_refinable(app)),
            ("C", "cancel refine", app.selected_is_refining()),
            ("s", "state", true),
            ("!", "deep", app.selected_can_go_deep()),
            ("w", "wait", app.selected_can_hand_off()),
            ("o", "open", app.selected_has_a_diff()),
            ("g", "PR", app.selected_is_in_review()),
            ("a/A", "message", app.selected_can_message()),
            ("n/N", "new", true),
            ("e", "edit", true),
            ("?", "keys", true),
            ("tab", "tasks", true),
            ("q", "quit", true),
        ],
        Screen::Tasks => vec![
            enter,
            ("w", "wait", app.selected_can_hand_off()),
            ("o", "open", app.selected_has_a_diff()),
            ("g", "PR", app.selected_is_in_review()),
            ("r/R", "refine", selection_is_refinable(app)),
            ("C", "cancel refine", app.selected_is_refining()),
            ("s", "state", true),
            ("!", "deep", app.selected_can_go_deep()),
            ("a/A", "message", app.selected_can_message()),
            ("n/N", "new", true),
            ("e", "edit", true),
            ("?", "keys", true),
            ("tab", "projects", true),
            ("q", "quit", true),
        ],
        // `a`/`A` and the rest of this screen's uppercase keys are unrelated
        // actions sharing a letter, not variants of one action, so they keep
        // their own slots.
        Screen::Projects => vec![
            ("0-5", "weight", true),
            ("r", "rename", true),
            ("a", "add", true),
            ("A", "archive", true),
            ("d", "delete", true),
            ("v", "viewer", true),
            ("?", "keys", true),
            ("tab", "config", true),
            ("q", "quit", true),
        ],
        Screen::Config => {
            let viewers = !app.config_viewers.is_empty();
            // While the gate holds (DESIGN.md §9) `tab` cycles the shorter
            // Projects ↔ Config ring, so the slot has to name where it lands.
            let next = if app.projects.is_empty() {
                "projects"
            } else {
                "cockpit"
            };
            vec![
                ("a", "add viewer", true),
                ("e", "edit", viewers),
                ("d", "delete", viewers),
                ("V", "default viewer", viewers),
                ("A", "default agent", true),
                ("?", "keys", true),
                ("tab", next, true),
                ("q", "quit", true),
            ]
        }
    }
}

/// The contextual per-screen key line (DESIGN.md §9): the actions that apply on
/// the current screen and selection, as key/label pairs the caller renders
/// key-bold, label-dim. A lowercase/uppercase pair of one action takes a single
/// slot keyed on the pair (`d/D dispatch`), with the uppercase variant's gloss
/// left to `?`. The line carries what changes a task's state or destiny;
/// navigation, display toggles and browsing conveniences live in the key map
/// only, so `?` is always present. Selection-only actions drop out when there
/// is nothing to act on, and the refine keys appear only on a task whose body is
/// still a brief — a proposal or a ready task.
fn key_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    hint_candidates(app)
        .into_iter()
        .filter(|(_, _, shown)| *shown)
        .map(|(key, label, _)| (key, label))
        .collect()
}

/// The lowercase/uppercase pairs the key line renders as one slot (DESIGN.md
/// §9). This map is the only place the uppercase variants are glossed, so the
/// three lines are worded to one shape, the one the case convention asks for:
/// the lowercase acts headlessly and stays in the TUI, the uppercase names the
/// surface it opens.
const DISPATCH_KEYS: [(&str, &str); 2] = [
    ("d", "dispatch to the resolved agent"),
    ("D", "dispatch, choosing the agent"),
];
const REFINE_KEYS: [(&str, &str); 2] = [
    ("r", "refine a brief from a note, headless"),
    ("R", "refine a brief in an agent session"),
];
const NEW_KEYS: [(&str, &str); 2] = [
    ("n", "new task, proposed headless"),
    ("N", "new task, planned in an agent session"),
];

/// The uppercase keys DESIGN.md §9 names as standing outside the case
/// convention, because none is the shifted half of a pair: `C` and the projects
/// screen's `A` share a letter with an unrelated action, `J`/`K` scroll the
/// card, and the Config screen's `V`/`A` pick defaults. Every other uppercase
/// binding has to be the interactive half of a pair, which the test below
/// enforces screen by screen.
#[cfg(test)]
const CASE_EXCEPTIONS: [(Screen, &str); 7] = [
    (Screen::Cockpit, "C"),
    (Screen::Cockpit, "J"),
    (Screen::Cockpit, "K"),
    (Screen::Tasks, "C"),
    (Screen::Projects, "A"),
    (Screen::Config, "V"),
    (Screen::Config, "A"),
];
const MESSAGE_KEYS: [(&str, &str); 2] = [
    ("a", "message the task's session, headless"),
    ("A", "message in person — attach or resume"),
];

/// A titled group of key/label pairs in the key map.
type KeySection = (&'static str, Vec<(&'static str, &'static str)>);

/// A screen's complete key map (DESIGN.md §9), grouped into actions,
/// navigation, and screen switching. Unlike [`key_hints`] it is ungated by
/// selection — it is the map, so it lists every key the screen binds, including
/// the ones the line has no room to advertise. `no_projects` is the one thing
/// it does gate on, because a map that advertised a jump the gate refuses would
/// be promising a refusal.
fn key_map(screen: Screen, no_projects: bool) -> Vec<KeySection> {
    let pairs = |set: [(&'static str, &'static str); 2]| set.into_iter();
    let screens = |current: &'static str| {
        let mut keys = vec![("tab", current)];
        if !no_projects {
            keys.push(("alt-1", "cockpit"));
            keys.push(("alt-2", "tasks"));
        }
        keys.push(("alt-3", "projects"));
        keys.push(("alt-4", "config"));
        ("Screens", keys)
    };
    match screen {
        Screen::Cockpit => {
            let mut actions = vec![("⏎", "act on the selected row")];
            actions.extend(pairs(DISPATCH_KEYS));
            actions.extend(pairs(REFINE_KEYS));
            actions.extend([
                ("0-3", "set the task's priority"),
                ("s", "change state"),
                ("!", "toggle deep — the agent's best model"),
                ("c", "link and unlink documents"),
                ("C", "cancel a refine in flight"),
                ("x", "fold the score decomposition in"),
                ("h", "fold the task's history in"),
                ("o", "open the local diff in a viewer"),
                ("g", "open the PR on GitHub"),
            ]);
            actions.extend(pairs(MESSAGE_KEYS));
            actions.extend([
                ("u", "nudge capped sessions past their reset"),
                ("l", "page the session log"),
                ("w", "hand a review task off, to wait"),
            ]);
            actions.extend(pairs(NEW_KEYS));
            actions.push(("ctrl-n", "new task, written by hand in $EDITOR"));
            actions.push(("e", "edit the selected task"));
            vec![
                ("Actions", actions),
                (
                    "Navigation",
                    vec![
                        ("j/k", "move the selection"),
                        ("J/K", "scroll the card"),
                        ("PgUp/PgDn", "page the card"),
                        ("ctrl-r", "refresh"),
                        ("?", "this key map"),
                        ("q", "quit"),
                    ],
                ),
                screens("next screen"),
            ]
        }
        Screen::Tasks => {
            let mut actions = vec![("⏎", "open the task's detail")];
            actions.extend(pairs(DISPATCH_KEYS));
            actions.extend(pairs(REFINE_KEYS));
            actions.extend([
                ("0-3", "set the task's priority"),
                ("s", "change state"),
                ("!", "toggle deep — the agent's best model"),
                ("c", "link and unlink documents"),
                ("C", "cancel a refine in flight"),
                ("o", "open the local diff in a viewer"),
                ("g", "open the PR on GitHub"),
            ]);
            actions.extend(pairs(MESSAGE_KEYS));
            actions.extend([
                ("u", "nudge capped sessions past their reset"),
                ("l", "page the session log"),
                ("w", "hand a review task off, to wait"),
            ]);
            actions.extend(pairs(NEW_KEYS));
            actions.push(("ctrl-n", "new task, written by hand in $EDITOR"));
            actions.push(("e", "edit the selected task"));
            vec![
                ("Actions", actions),
                (
                    "Navigation",
                    vec![
                        ("j/k", "move the selection"),
                        ("ctrl-r", "refresh"),
                        ("?", "this key map"),
                        ("q", "quit"),
                    ],
                ),
                screens("next screen"),
            ]
        }
        Screen::Projects => vec![
            (
                "Actions",
                vec![
                    ("0-5", "set the project's weight"),
                    ("r", "rename or re-path the project"),
                    ("a", "add a project"),
                    ("A", "archive or unarchive the project"),
                    ("d", "delete the project — only when it is empty"),
                    ("v", "pick the project's viewer"),
                ],
            ),
            (
                "Navigation",
                vec![
                    ("j/k", "move the selection"),
                    ("?", "this key map"),
                    ("q", "quit"),
                ],
            ),
            screens("next screen"),
        ],
        Screen::Config => vec![
            (
                "Actions",
                vec![
                    ("a", "add a viewer"),
                    ("⏎/e", "edit the selected viewer's command"),
                    ("d", "delete the selected viewer"),
                    ("V", "pick the default viewer"),
                    ("A", "pick the default agent"),
                ],
            ),
            (
                "Navigation",
                vec![
                    ("j/k", "move the selection"),
                    ("?", "this key map"),
                    ("q", "quit"),
                ],
            ),
            screens("next screen"),
        ],
    }
}

/// The widest key and the widest gloss in a column's sections — the two halves
/// of its natural width.
fn key_map_widths(sections: &[KeySection]) -> (usize, usize) {
    let entries = || sections.iter().flat_map(|(_, entries)| entries.iter());
    let width = |f: fn(&(&'static str, &'static str)) -> &'static str| {
        entries().map(|e| f(e).chars().count()).max().unwrap_or(0)
    };
    (width(|(key, _)| key), width(|(_, label)| label))
}

/// The key map's rows for one column, keys right-aligned to the column's widest
/// and glosses held to `label_w`, over-long ones ending in an ellipsis.
fn key_map_column(sections: &[KeySection], label_w: usize) -> Vec<Vec<Span<'static>>> {
    let (key_w, _) = key_map_widths(sections);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    for (i, (title, entries)) in sections.iter().enumerate() {
        if i > 0 {
            rows.push(Vec::new());
        }
        rows.push(vec![Span::styled(*title, Style::new().bold())]);
        for (key, label) in entries {
            let label = if label.chars().count() <= label_w {
                (*label).to_string()
            } else {
                let kept: String = label.chars().take(label_w.saturating_sub(1)).collect();
                format!("{kept}…")
            };
            rows.push(vec![
                Span::styled(format!("{key:>key_w$}  "), Style::new().bold()),
                Span::styled(label, Style::new().dim()),
            ]);
        }
    }
    rows
}

/// The `?` overlay: the current screen's whole key map, actions in one column
/// and navigation over screen switching in the other. Both dimensions are the
/// terminal's rather than the content's (DESIGN.md §9) — the Actions glosses
/// give up width before the Navigation column beside them clips, and what will
/// not fit the height moves to a further page `tab` turns to — so every entry
/// stays reachable however many keys the map grows.
fn draw_key_map(frame: &mut Frame, app: &App, page: usize) {
    const GAP: usize = 3;
    /// Under this a gloss says nothing whatever it does, so the Actions column
    /// stops giving width up and the overlay clips as any pane does.
    const MIN_LABEL: usize = 8;

    let sections = key_map(app.screen, app.projects.is_empty());
    let (actions, rest) = sections.split_at(1);
    let (key_l, natural_l) = key_map_widths(actions);
    let (key_r, label_r) = key_map_widths(rest);
    let chrome = key_l + 2 + GAP + key_r + 2 + 2;
    let avail = frame.area().width as usize;
    let label_l = if chrome + natural_l + label_r > avail {
        avail.saturating_sub(chrome + label_r).max(MIN_LABEL)
    } else {
        natural_l
    };
    let (left, right) = (
        key_map_column(actions, label_l),
        key_map_column(rest, label_r),
    );
    let width_of =
        |row: &Vec<Span<'static>>| -> usize { row.iter().map(|s| s.content.chars().count()).sum() };
    let left_w = left.iter().map(width_of).max().unwrap_or(0);
    let right_w = right.iter().map(width_of).max().unwrap_or(0);

    let lines: Vec<Line<'static>> = (0..left.len().max(right.len()))
        .map(|i| {
            let mut spans = left.get(i).cloned().unwrap_or_default();
            if let Some(row) = right.get(i) {
                let pad = (left_w + GAP).saturating_sub(width_of(&spans));
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(row.iter().cloned());
            }
            Line::from(spans)
        })
        .collect();

    // Pages are as few as the height allows and then evenly filled, so a map
    // one row too tall splits down the middle rather than stranding that row
    // alone on a second page; the box keeps one height throughout, so turning
    // a page does not resize the overlay under the reader.
    let box_h = (frame.area().height as usize).saturating_sub(2).max(1);
    let pages = lines.len().div_ceil(box_h).max(1);
    let per_page = lines.len().div_ceil(pages).max(1);
    let page = page % pages;
    let shown: Vec<Line<'static>> = lines
        .into_iter()
        .skip(page * per_page)
        .take(per_page)
        .collect();

    let screen = match app.screen {
        Screen::Cockpit => "cockpit",
        Screen::Tasks => "tasks",
        Screen::Projects => "projects",
        Screen::Config => "config",
    };
    let title = if pages > 1 {
        format!(
            "Keys — {screen} — {}/{pages}, tab pages — any key closes",
            page + 1
        )
    } else {
        format!("Keys — {screen} — any key closes")
    };
    let width = (left_w + GAP + right_w + 2) as u16;
    let area = popup_area(frame, width, per_page as u16 + 2);
    let para = Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(para, area);
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
    use voro_core::{LivenessSource, Priority, Task, TaskState};

    /// The refusal `g` gives on a checkout `gh` cannot address (DESIGN.md §8) —
    /// the shape every wrapping test here cares about: long, and closing on the
    /// key to press instead.
    const GH_REFUSAL: &str = "/home/michael/Projects/demoproj is not a GitHub repository, \
         so there is no pull request to open — use `o` to see this task's diff in a viewer";

    /// Jump to a screen the way the operator does, with the alt-digit binding
    /// (DESIGN.md §9) — a bare digit is a weight or a priority.
    fn alt_screen(app: &mut crate::app::App, digit: char) {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        app.on_key(KeyEvent::new(KeyCode::Char(digit), KeyModifiers::ALT));
    }

    /// Every screen's status region, read back as one whitespace-normalised
    /// string, so a message that wrapped across rows reads as itself again.
    fn screen_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let rows: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        rows.join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// An app showing `status`, over `queued` ready tasks and `running`
    /// dispatched ones — the two counts are what decide how much of the cockpit
    /// the queue and the running strip claim.
    fn app_with_status(status: &str, queued: usize, running: usize) -> crate::app::App {
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let mut task = |title: String| {
            store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title,
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                })
                .unwrap()
                .id
        };
        for i in 0..queued {
            task(format!("queued {i}"));
        }
        let live: Vec<i64> = (0..running).map(|i| task(format!("live {i}"))).collect();
        for id in live {
            store
                .record_dispatch(id, "claude", None, LivenessSource::Listing, None)
                .unwrap();
        }
        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = crate::app::App::new(store, ctx).unwrap();
        app.status = Some(status.into());
        app
    }

    /// The cockpit is gated behind having a project (DESIGN.md §9), so its
    /// empty box has one case left to explain — the drained queue — and `n` is
    /// what fills it.
    #[test]
    fn an_empty_queue_points_at_n() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut registered = app_with_status("", 0, 0);
        registered.status = None;
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &registered);
            })
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("nothing to do — press n"), "{text}");
    }

    /// The key map may not advertise a jump the gate would refuse (DESIGN.md
    /// §9): with no project registered the Screens section drops `alt-1` and
    /// `alt-2`, and gets them back the moment one exists.
    #[test]
    fn the_key_map_hides_the_gated_screen_jumps() {
        for screen in [
            Screen::Cockpit,
            Screen::Tasks,
            Screen::Projects,
            Screen::Config,
        ] {
            let jumps = |no_projects: bool| -> Vec<&'static str> {
                key_map(screen, no_projects)
                    .into_iter()
                    .flat_map(|(_, entries)| entries)
                    .map(|(key, _)| key)
                    .filter(|key| key.starts_with("alt-"))
                    .collect()
            };
            assert_eq!(jumps(true), vec!["alt-3", "alt-4"], "{screen:?}");
            assert_eq!(
                jumps(false),
                vec!["alt-1", "alt-2", "alt-3", "alt-4"],
                "{screen:?}"
            );
        }
    }

    #[test]
    fn wrap_status_breaks_on_words_and_keeps_every_one() {
        let lines = wrap_status(GH_REFUSAL, 40);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(
                line.chars().count() <= 40,
                "{line:?} is wider than the pane"
            );
        }
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            GH_REFUSAL.split_whitespace().collect::<Vec<_>>(),
        );
    }

    /// A word with nowhere to break — a long path — takes its own line rather
    /// than pushing the rest of the message off the pane.
    #[test]
    fn wrap_status_gives_an_overlong_word_its_own_line() {
        let lines = wrap_status("at /a/very/long/path/that/exceeds/the/pane use `o`", 12);
        assert_eq!(
            lines,
            vec!["at", "/a/very/long/path/that/exceeds/the/pane", "use `o`"]
        );
    }

    /// The bug this fixes: at an ordinary terminal width the closing half of an
    /// error — the part naming what to press instead — was cut off. The cockpit
    /// now grows its status region to fit the whole message.
    #[test]
    fn cockpit_status_wraps_instead_of_truncating() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let app = app_with_status(GH_REFUSAL, 1, 0);
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();

        let normalised = GH_REFUSAL.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            screen_text(&terminal).contains(&normalised),
            "the whole message should be on screen, got:\n{}",
            screen_text(&terminal)
        );
    }

    /// A cockpit whose queue and running strip are both at their tallest still
    /// shows the message whole — three times the length of the longest real
    /// one, on a narrow screen — the panes above it giving up the rows.
    #[test]
    fn a_crowded_cockpit_still_shows_the_whole_message() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let long = format!("{GH_REFUSAL} {GH_REFUSAL} {GH_REFUSAL}");
        let app = app_with_status(&long, 20, 12);
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();

        let normalised = long.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            screen_text(&terminal).contains(&normalised),
            "the whole message should survive a full cockpit, got:\n{}",
            screen_text(&terminal)
        );
    }

    /// The same holds on every other screen, since each splits its own layout.
    #[test]
    fn every_screen_wraps_a_long_status() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let normalised = GH_REFUSAL.split_whitespace().collect::<Vec<_>>().join(" ");
        for (key, screen) in [
            ('2', Screen::Tasks),
            ('3', Screen::Projects),
            ('4', Screen::Config),
        ] {
            let mut app = app_with_status(GH_REFUSAL, 1, 0);
            alt_screen(&mut app, key);
            assert_eq!(app.screen, screen);
            // Switching screens is free to clear the message; re-arm it.
            app.status = Some(GH_REFUSAL.into());

            let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
            terminal
                .draw(|f| {
                    draw(f, &app);
                })
                .unwrap();
            assert!(
                screen_text(&terminal).contains(&normalised),
                "{screen:?} truncated the message:\n{}",
                screen_text(&terminal)
            );
        }
    }

    /// A short message and the key line still occupy one row, so the panes keep
    /// the space they had whenever there is nothing long to say.
    #[test]
    fn status_region_stays_one_line_for_short_messages() {
        let area = Rect::new(0, 0, 110, 24);
        let mut app = app_with_status("task 9 has no session on record", 1, 0);
        assert_eq!(status_height(&app, area), 1);
        app.status = None;
        assert_eq!(status_height(&app, area), 1);
    }

    /// The region grows only to half the screen: a terminal too small to hold
    /// the message is better served by keeping its lists than by burying them.
    #[test]
    fn status_region_stops_at_half_the_screen() {
        let app = app_with_status(&GH_REFUSAL.repeat(20), 1, 0);
        assert_eq!(status_height(&app, Rect::new(0, 0, 40, 24)), 12);
        assert_eq!(status_height(&app, Rect::new(0, 0, 40, 3)), 1);
    }

    /// End-to-end: the Config screen renders the read-only agents (with the
    /// default marked) over the editable named viewers, drawn through the real
    /// screen draw path (DESIGN.md §5).
    #[test]
    fn config_screen_renders_agents_and_viewers() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::Store;

        let dir = std::env::temp_dir().join(format!(
            "voro-ui-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agents_path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&agents_path, "[viewers.zed]\ncmd = \"zed {path}\"\n").unwrap();

        let store = Store::open_in_memory().unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '4');
        assert_eq!(app.screen, Screen::Config);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(rendered.contains("Agents"), "{rendered}");
        assert!(rendered.contains("claude"), "{rendered}");
        assert!(rendered.contains("Viewers"), "{rendered}");
        assert!(
            rendered.contains("zed") && rendered.contains("zed {path}"),
            "{rendered}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn row(state: TaskState, blockers: Vec<DepRef>) -> TaskRow {
        TaskRow {
            task: Task {
                id: 9,
                project_id: 1,
                repo_id: None,
                title: "waiting".into(),
                body: String::new(),
                priority: Priority::P2,
                state,
                agent: None,
                human: false,
                deep: false,
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
    fn question_summary_collapses_a_multi_line_question_to_its_first_line() {
        // a single-line question is unchanged
        assert_eq!(question_summary("Schema A or B?"), "Schema A or B?");
        // a multi-line one collapses to the first line with a trailing ellipsis,
        // the marker that there is more to read in the detail pane
        assert_eq!(
            question_summary("Which schema?\nA: normalised\nB: flat"),
            "Which schema?…"
        );
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

    /// An empty browser explains itself rather than drawing a blank box. The
    /// gate (DESIGN.md §9) leaves it one case to explain — a project exists and
    /// has no tasks — so `n` is always the way in.
    #[test]
    fn browser_render_explains_an_empty_list() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::Store;

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut store = Store::open_in_memory().unwrap();
        store.create_project("voro", "/tmp/voro").unwrap();
        let mut app = App::new(store, ctx).unwrap();
        app.screen = crate::app::Screen::Tasks;

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| draw_tasks(f, &app, &mut HitMap::default()))
            .unwrap();
        let no_tasks: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            no_tasks.contains("no tasks yet — press n to add one"),
            "empty browser did not point at n: {no_tasks}"
        );
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
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
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

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.toggle_screen();

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| {
                draw_tasks(f, &app, &mut HitMap::default());
            })
            .unwrap();

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

    /// The detail card advertises what the project can actually do (DESIGN.md
    /// §8): in a checkout with no remote, `g` has nowhere to open a pull
    /// request, so the card names the local viewer and the key that reaches it.
    #[test]
    fn the_detail_card_advertises_the_local_path_without_a_remote() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::process::{Command, Stdio};
        use voro_core::{Action, NewTask, Store};

        let project = std::env::temp_dir().join(format!(
            "voro-ui-remoteless-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let mut store = Store::open_in_memory().unwrap();
        let p = store
            .create_project("voro", project.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "the finished work".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap()
            .id;
        store
            .record_dispatch(task, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        store
            .apply(task, Action::Complete(Some("did it".into())))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| draw_cockpit(f, &app, &mut HitMap::default()))
            .unwrap();
        let out: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(out.contains("next: open"), "{out}");
        assert!(out.contains("(o shows the diff in a viewer)"), "{out}");
        assert!(!out.contains("next: pr"), "{out}");

        std::fs::remove_dir_all(&project).ok();
    }

    /// The half-written report withholds `pr` and nothing else (DESIGN.md §8).
    /// In a checkout with no remote the card advertises `open`, which reads a
    /// diff and needs no summary, so the recommendation stands and the marker
    /// sits beside it — the operator sees both what is missing and what they
    /// can do about the work today.
    #[test]
    fn an_incomplete_report_leaves_the_local_review_verb_standing() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::process::{Command, Stdio};
        use voro_core::{Action, NewTask, Store};

        let project = std::env::temp_dir().join(format!(
            "voro-ui-half-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&project).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let mut store = Store::open_in_memory().unwrap();
        let p = store
            .create_project("voro", project.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "the unreported work".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap()
            .id;
        store.set_branch(task, Some("feat/thing")).unwrap();
        store
            .record_dispatch(task, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        // A branch and no summary: the half-written report.
        store.apply(task, Action::Complete(None)).unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| draw_cockpit(f, &app, &mut HitMap::default()))
            .unwrap();
        let out: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(out.contains("next: open"), "{out}");
        assert!(out.contains("[incomplete report]"), "{out}");

        std::fs::remove_dir_all(&project).ok();
    }

    /// End-to-end through the real cockpit draw (DESIGN.md §9): a hand-off
    /// rides the strip as its own kind of row, badged with what it is holding
    /// up and whether a PR tracks it, while staying out of the queue.
    #[test]
    fn a_hand_off_renders_on_the_strip_with_its_badges() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let mut task = |title: &str| {
            store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title: title.into(),
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                })
                .unwrap()
                .id
        };
        let handed_off = task("dependency bump");
        let gated = task("the work behind it");
        let closed_dependent = task("already landed");
        let under_way = task("still being written");

        store.add_dep(gated, handed_off, DepKind::Blocks).unwrap();
        store
            .add_dep(closed_dependent, handed_off, DepKind::Blocks)
            .unwrap();
        store.apply(closed_dependent, Action::Abandon).unwrap();

        store
            .record_dispatch(handed_off, "claude", None, LivenessSource::Pid, None)
            .unwrap();
        store.apply(handed_off, Action::Complete(None)).unwrap();
        store.apply(handed_off, Action::HandOff).unwrap();
        store
            .set_pr(handed_off, Some("https://github.com/o/r/pull/9"))
            .unwrap();
        store
            .record_dispatch(under_way, "claude", None, LivenessSource::Pid, None)
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| draw_cockpit(f, &app, &mut HitMap::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines: Vec<String> = buffer
            .content()
            .chunks(buffer.area().width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let out = lines.join("\n");

        // The detail pane names the selected task too, so read the strip's own
        // rows: everything below its block header.
        let strip = lines
            .iter()
            .position(|l| l.contains("Running"))
            .map(|i| &lines[i + 1..])
            .unwrap_or_else(|| panic!("no running strip was drawn:\n{out}"));
        let waiting_row = strip
            .iter()
            .find(|l| l.contains("dependency bump"))
            .unwrap_or_else(|| panic!("the hand-off is not on the strip:\n{out}"));
        assert!(waiting_row.contains('⏳'), "{waiting_row}");
        // one open dependent, not two: the abandoned one no longer waits on it
        assert!(waiting_row.contains("blocks 1"), "{waiting_row}");
        assert!(waiting_row.contains("  PR"), "{waiting_row}");
        assert!(
            !waiting_row.contains("no live session"),
            "a hand-off has nothing left to be live: {waiting_row}"
        );
        assert!(out.contains("waiting 1"), "the header counts it: {out}");

        // Work under way sorts above the hand-off, and the double-width
        // hourglass leaves the columns aligned across both row kinds.
        let running_at = strip
            .iter()
            .position(|l| l.contains("still being written"))
            .unwrap();
        let waiting_at = strip
            .iter()
            .position(|l| l.contains("dependency bump"))
            .unwrap();
        assert!(running_at < waiting_at, "{out}");
        // by cell, not by byte: the buffer spells a double-width glyph as its
        // symbol plus a blank, so one char is one column
        let column = |line: &str, needle: &str| {
            line.find(needle)
                .map(|byte| line[..byte].chars().count())
                .unwrap()
        };
        assert_eq!(
            column(&strip[running_at], "still being written"),
            column(&strip[waiting_at], "dependency bump"),
            "the strip's title column moved between row kinds:\n{out}"
        );
    }

    /// The badge a capped-but-alive dispatch earns (DESIGN.md §8), end to end
    /// through the real cockpit draw. The session is `running` throughout and
    /// stays that way: a cap is a display fact about a session that will resume
    /// on its own, not a death to be redispatched, so nothing here touches the
    /// state machine.
    #[test]
    fn a_capped_session_is_badged_on_the_strip() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{CapReading, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "the held one".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap()
            .id;
        store
            .record_dispatch(task, "claude", None, LivenessSource::Listing, None)
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        // The detail pane names the selected task too, so read the strip's own
        // rows: everything below its block header.
        let row = |app: &App| {
            let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
            terminal
                .draw(|f| draw_cockpit(f, app, &mut HitMap::default()))
                .unwrap();
            let buffer = terminal.backend().buffer().clone();
            let lines: Vec<String> = buffer
                .content()
                .chunks(buffer.area().width as usize)
                .map(|r| r.iter().map(|c| c.symbol()).collect())
                .collect();
            let at = lines
                .iter()
                .position(|l| l.contains("Running"))
                .unwrap_or_else(|| panic!("no running strip was drawn:\n{}", lines.join("\n")));
            lines[at + 1..]
                .iter()
                .find(|l| l.contains("the held one"))
                .unwrap_or_else(|| {
                    panic!("the dispatch is not on the strip:\n{}", lines.join("\n"))
                })
                .clone()
        };

        // No reading, no badge: an ordinary dispatch is unmarked.
        assert!(!row(&app).contains("capped"), "{}", row(&app));

        // A cap whose window is still an hour out names when it reopens.
        app.caps.insert(
            task,
            CapReading {
                reset_minutes: Some(21 * 60 + 50),
            },
        );
        app.now_minutes = Some(20 * 60 + 50);
        let line = row(&app);
        assert!(line.contains("⚠ capped"), "{line}");
        assert!(line.contains("↻21:50"), "{line}");
        assert!(!line.contains("reset passed"), "{line}");
        assert_eq!(
            app.store.task(task).unwrap().state,
            TaskState::Running,
            "badging a cap must not move the task"
        );

        // Past that time the window is open and the session is only waiting to
        // be nudged — a different situation, said differently.
        app.now_minutes = Some(22 * 60);
        let line = row(&app);
        assert!(line.contains("⚠ capped · reset passed"), "{line}");

        // A cap the agent named no time for still badges: the time is the
        // optional half, and withholding the badge for want of it would trade
        // the signal for a detail.
        app.caps.insert(task, CapReading::default());
        let line = row(&app);
        assert!(line.contains("⚠ capped"), "{line}");
        assert!(!line.contains('↻'), "{line}");
        assert!(!line.contains("reset passed"), "{line}");

        // And the badge clears itself once the reading goes away, which is what
        // continuing the session does on the next pass.
        app.caps.remove(&task);
        assert!(!row(&app).contains("capped"), "{}", row(&app));
    }

    /// A full slate of in-flight work — the dispatch cap's worth of running
    /// sessions plus the hand-offs that do not count against it — fits on the
    /// strip at once rather than being cut off behind a scroll.
    #[test]
    fn a_full_slate_of_in_flight_work_all_shows_on_the_strip() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let mut task = |title: &str| {
            store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title: title.into(),
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                })
                .unwrap()
                .id
        };
        let running: Vec<(i64, String)> = (1..=6)
            .map(|n| {
                let title = format!("under way {n}");
                (task(&title), title)
            })
            .collect();
        let handed_off: Vec<(i64, String)> = (1..=2)
            .map(|n| {
                let title = format!("handed off {n}");
                (task(&title), title)
            })
            .collect();

        for (id, _) in &running {
            store
                .record_dispatch(*id, "claude", None, LivenessSource::Listing, None)
                .unwrap();
        }
        for (id, _) in &handed_off {
            store
                .record_dispatch(*id, "claude", None, LivenessSource::Listing, None)
                .unwrap();
            store.apply(*id, Action::Complete(None)).unwrap();
            store.apply(*id, Action::HandOff).unwrap();
        }

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        assert_eq!(app.running.len(), 8, "the fixture must fill the strip");

        let mut terminal = Terminal::new(TestBackend::new(90, 30)).unwrap();
        terminal
            .draw(|f| draw_cockpit(f, &app, &mut HitMap::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines: Vec<String> = buffer
            .content()
            .chunks(buffer.area().width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect();
        let out = lines.join("\n");

        // The detail pane names the selected task too, so read the strip's own
        // rows: everything below its block header.
        let strip = lines
            .iter()
            .position(|l| l.contains("Running"))
            .map(|i| &lines[i + 1..])
            .unwrap_or_else(|| panic!("no running strip was drawn:\n{out}"));
        for (_, title) in running.iter().chain(handed_off.iter()) {
            assert!(
                strip.iter().any(|l| l.contains(title.as_str())),
                "{title} did not fit on the strip:\n{out}"
            );
        }
    }

    /// End-to-end through the real cockpit draw (DESIGN.md §6/§9): a refine
    /// round shows on the running strip as its own kind of row with elapsed
    /// time, and the proposal it holds is nowhere in the triage queue. Once the
    /// round concludes, the row is gone and the proposal is back, marked.
    #[test]
    fn a_refine_round_renders_on_the_strip_and_leaves_the_queue() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, RefineOutcome, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "sloppy proposal".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Proposed,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store
            .record_refine_launch(
                task.id,
                "name the files",
                "claude",
                None,
                LivenessSource::Pid,
                None,
            )
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| draw_cockpit(f, app, &mut HitMap::default()))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect()
        };

        let out = render(&app, &mut terminal);
        assert!(out.contains("⟳ refining"), "{out}");
        assert!(out.contains("sloppy proposal"), "{out}");
        assert!(out.contains("refining 1"), "the header counts it: {out}");
        assert!(
            !out.contains("awaiting triage"),
            "a refining proposal must not ride the triage digest: {out}"
        );

        app.store
            .conclude_refine(task.id, RefineOutcome::Applied)
            .unwrap();
        app.refresh().unwrap();
        let out = render(&app, &mut terminal);
        assert!(!out.contains("⟳ refining"), "{out}");
        assert!(out.contains("↻ 1 refined"), "{out}");
    }

    /// A round that died says so where the operator triages, in its own words —
    /// a failed refine must never read as a proposal nobody refined.
    #[test]
    fn a_failed_refine_round_renders_its_own_marker() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, RefineOutcome, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "sloppy proposal".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Proposed,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store
            .record_refine_launch(
                task.id,
                "name the files",
                "claude",
                None,
                LivenessSource::Pid,
                None,
            )
            .unwrap();
        store
            .conclude_refine(task.id, RefineOutcome::Failed)
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.toggle_screen();

        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|f| draw_tasks(f, &app, &mut HitMap::default()))
            .unwrap();
        let out: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(out.contains("⚠ refine failed"), "{out}");
        assert!(!out.contains("↻ refined"), "{out}");
    }

    /// The queue's state cell is coloured only where the state is stuck on the
    /// operator, and never bold — the row markers own that weight.
    #[test]
    fn state_cell_colours_only_the_states_stuck_on_the_operator() {
        let cases = [
            (TaskState::NeedsInput, Some(Color::Cyan)),
            (TaskState::Review, Some(Color::Green)),
            (TaskState::Stalled, Some(Color::Red)),
            (TaskState::Ready, None),
            (TaskState::Proposed, None),
        ];
        for (state, fg) in cases {
            let span = state_span(state);
            assert_eq!(span.style.fg, fg, "{}", state.as_str());
            assert!(
                !span.style.add_modifier.contains(Modifier::BOLD),
                "{}",
                state.as_str()
            );
        }
        assert!(
            state_span(TaskState::Proposed)
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !state_span(TaskState::Ready)
                .style
                .add_modifier
                .contains(Modifier::DIM)
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
                repo_id: None,
                title: "hands-on".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: true,
                deep: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
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

    /// End-to-end: a deep task carries the `!` marker beside the priority cell
    /// on its queue and browser rows and the spelled-out line in the cockpit
    /// detail pane; a task on the workhorse carries neither (task #241).
    #[test]
    fn deep_flag_renders_in_queue_browser_and_detail() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str, deep: bool| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep,
        };
        store.create_task(new("the hard one", true)).unwrap();
        store.create_task(new("the ordinary one", false)).unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
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
            cockpit.contains("P2! voro: the hard one"),
            "queue row should mark the deep task: {cockpit}"
        );
        assert!(
            cockpit.contains("P2  voro: the ordinary one"),
            "a workhorse row keeps the column blank: {cockpit}"
        );
        assert!(
            cockpit.contains("deep — dispatches on the agent's strongest model"),
            "detail pane should spell the flag out: {cockpit}"
        );

        app.toggle_screen();
        let browser = render(&app, &mut terminal);
        assert!(
            browser.contains("P2! w3"),
            "browser row should mark the deep task: {browser}"
        );
        assert!(
            browser.contains("P2  w3"),
            "a workhorse row keeps the column blank: {browser}"
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
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
            deep: false,
        };
        store.create_task(new("open", TaskState::Ready)).unwrap();
        let closed = store.create_task(new("closed", TaskState::Ready)).unwrap();
        store
            .apply(closed.id, voro_core::Action::Start)
            .and_then(|_| store.apply(closed.id, voro_core::Action::Complete(None)))
            .and_then(|_| store.apply(closed.id, voro_core::Action::Accept))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        key_to_projects(&mut app);
        assert_eq!(app.screen, Screen::Projects);

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
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
        alt_screen(app, '3');
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
                repo_id: None,
                title: "a task".into(),
                body: "body".into(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Char('h')));

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("base w×(p+s+u)"),
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
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
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

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
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
        alt_screen(&mut app, '2');
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

    /// End-to-end: every queue row names the task's state (DESIGN.md §3), in
    /// the cockpit's own rows and under an expanded digest alike, rendered
    /// through the real draw path. The two `ready` arms — by hand and
    /// dispatchable — are told apart by the `[human]` marker, not the column.
    #[test]
    fn cockpit_queue_shows_the_task_state_on_each_row() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        let new = |title: &str, state, human| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human,
            deep: false,
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
            .record_dispatch(redispatch.id, "claude", Some(1), LivenessSource::Pid, None)
            .unwrap();
        store.reconcile_session(session.id, false, false).unwrap();
        let do_ = store
            .create_task(new("by hand", TaskState::Ready, true))
            .unwrap();
        let dispatch = store
            .create_task(new("startable", TaskState::Ready, false))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut render = |app: &App| {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        let rendered = render(&app);

        // A proposal is collapsed into its digest row rather than one of its
        // own (DESIGN.md §7); every other state renders in place.
        assert!(
            rendered.contains("▲ 1 proposal awaiting triage"),
            "the digest should stand in for #{}: {rendered}",
            triage.id
        );
        let state_cell =
            |id: i64, state: TaskState| format!("{} {:11}", task_ref(id), state.as_str());
        for (task, state) in [
            (&answer, TaskState::NeedsInput),
            (&pr, TaskState::Review),
            (&review_pr, TaskState::Review),
            (&redispatch, TaskState::Stalled),
            (&do_, TaskState::Ready),
            (&dispatch, TaskState::Ready),
        ] {
            assert!(
                rendered.contains(&state_cell(task.id, state)),
                "queue row for #{} should carry '{state}': {rendered}",
                task.id
            );
        }
        assert!(
            rendered.contains(&format!("voro: {}  [human]", do_.title)),
            "the by-hand row should carry the human marker: {rendered}"
        );

        // Folding the digest open lists the proposal itself, which names its
        // state in the same column as every other row.
        let digest_index = app
            .queue
            .rows
            .iter()
            .position(|r| matches!(r, voro_core::QueueRow::Digest(_)))
            .expect("the proposal should have been collapsed into a digest");
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, crate::app::CockpitRow::Queue(i) if *i == digest_index))
            .expect("the digest should be a selectable cockpit row");
        app.on_key(KeyEvent::from(KeyCode::Enter));
        let expanded = render(&app);
        assert!(
            expanded.contains(&state_cell(triage.id, TaskState::Proposed)),
            "the expanded proposal should carry 'proposed': {expanded}"
        );
    }

    /// End-to-end: with the fleet full the cockpit offers no dispatch rows and
    /// says why in the pane's own header (DESIGN.md §7), while the rows that
    /// cost only attention stay put.
    #[test]
    fn cockpit_shows_the_capacity_line_instead_of_dispatch_rows() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
        };
        // Fill the default cap of five, then leave one startable task and one
        // question behind it.
        for i in 0..5 {
            let t = store.create_task(new(&format!("in flight {i}"))).unwrap();
            store.apply(t.id, Action::Start).unwrap();
        }
        store.create_task(new("startable")).unwrap();
        let asking = store.create_task(new("asking")).unwrap();
        store.apply(asking.id, Action::Start).unwrap();
        store
            .apply(asking.id, Action::Ask("A or B?".into()))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        assert_eq!(app.queue_task_ids(), vec![asking.id]);

        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();

        assert!(
            rendered.contains("⏸ dispatch at capacity (5/5 running)"),
            "{rendered}"
        );
        assert!(!rendered.contains("startable"), "{rendered}");
        assert!(rendered.contains("asking"), "{rendered}");
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
                repo_id: None,
                title: "a long task".into(),
                body,
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(40, 16)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
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

    /// A `body` event's detail is a whole replaced brief (DESIGN.md §8), so the
    /// history folds it to a marker naming the event that holds it rather than
    /// spilling a superseded body across the pane. Every other kind reads as-is.
    #[test]
    fn history_folds_a_replaced_body_to_a_recovery_marker() {
        let events = vec![
            Event {
                id: 4,
                task_id: Some(62),
                at: "2026-08-01 10:00:00".into(),
                kind: "priority".into(),
                detail: Some("P1".into()),
            },
            Event {
                id: 5,
                task_id: Some(62),
                at: "2026-08-01 10:01:00".into(),
                kind: "body".into(),
                detail: Some("the brief\nline two\nline three".into()),
            },
        ];
        let rendered: Vec<String> = history_lines(&events)
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("priority") && l.contains("P1"))
        );
        let body = rendered
            .iter()
            .find(|l| l.contains("body"))
            .expect("the body event renders");
        assert!(body.contains("replaced body kept (3 lines)"), "{body}");
        assert!(body.contains("voro show 62 --event 5"), "{body}");
        assert!(!body.contains("line two"), "{body}");
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
                repo_id: None,
                title: "a task".into(),
                body: "body".into(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '2'); // tasks screen
        app.on_key(KeyEvent::from(KeyCode::Enter)); // open the Detail popup
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Char('h')));
        assert!(matches!(app.mode, Mode::Detail { .. }));

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("base w×(p+s+u)"),
            "score decomposition should fold into the Detail popup: {rendered}"
        );
        assert!(
            rendered.contains("History") && rendered.contains("created"),
            "history should fold into the Detail popup: {rendered}"
        );
    }

    /// The cockpit detail pane answers "what happened" for a stalled task
    /// (task #73): the dead session's outcome, agent, end time, and log path
    /// render under the metadata. A capped session reads `capped`; a clean
    /// ready task carries none of it.
    #[test]
    fn detail_pane_shows_a_stalled_tasks_session_post_mortem() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let ctx = || {
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            ))
        };
        let app_with_session = |capped: bool| {
            let mut store = Store::open_in_memory().unwrap();
            let p = store.create_project("voro", "/tmp/voro").unwrap();
            store.set_weight(p.id, 3).unwrap();
            let task = store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title: "went quiet".into(),
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                })
                .unwrap();
            let (_, session) = store
                .record_dispatch(
                    task.id,
                    "claude",
                    Some(1),
                    LivenessSource::Pid,
                    Some("/tmp/voro/s.log"),
                )
                .unwrap();
            store.reconcile_session(session.id, false, capped).unwrap();
            App::new(store, ctx()).unwrap()
        };
        let render = |app: &App| {
            let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
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

        let capped = app_with_session(true);
        assert!(render(&capped).contains("last session: capped"));

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "fresh".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
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
                repo_id: None,
                title: "mid-flight".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store
            .record_dispatch(
                task.id,
                "claude",
                Some(1),
                LivenessSource::Pid,
                Some("/tmp/voro/open.log"),
            )
            .unwrap();
        store.apply(task.id, Action::Ask("A or B?".into())).unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
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
    }

    /// A multi-line question renders across multiple lines in the cockpit
    /// detail pane (DESIGN.md §6), each behind the agent-voice gutter that
    /// marks the block as the agent's own words (task #430).
    #[test]
    fn detail_pane_renders_a_multi_line_question_across_lines() {
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
                repo_id: None,
                title: "mid-flight".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store
            .record_dispatch(
                task.id,
                "claude",
                Some(1),
                LivenessSource::Pid,
                Some("/tmp/voro/open.log"),
            )
            .unwrap();
        store
            .apply(
                task.id,
                Action::Ask("Pick a schema:\nAlpha option\nBravo option".into()),
            )
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();
        let width: u16 = 100;
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        // Reassemble the buffer into rows so each question line is checked on
        // its own terminal row — a single-span rendering would collapse them.
        let rows: Vec<String> = terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect();
        for expected in [
            "│ question:",
            "│ Pick a schema:",
            "│ Alpha option",
            "│ Bravo option",
        ] {
            assert!(rows.iter().any(|r| r.contains(expected)), "{rows:?}");
        }
    }

    /// The agent's blocks are parsed as markdown, not printed raw (task #430):
    /// bold and inline code are styled rather than showing their markers, and
    /// every visual line of the block — continuations included, at a pane too
    /// narrow to hold the line — carries the cyan gutter.
    #[test]
    fn agent_voice_blocks_parse_markdown_behind_a_gutter() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;
        use voro_core::{Action, NewTask, Store};

        let summary = "**Landed** the `parser`.\n\n- rewrote the lexer so every token \
                       carries its span through to the reporter\n- covered it";
        let app = |width: u16| {
            let mut store = Store::open_in_memory().unwrap();
            let p = store.create_project("voro", "/tmp/voro").unwrap();
            store.set_weight(p.id, 3).unwrap();
            let task = store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title: "parse it".into(),
                    body: "acceptance: **it parses**".into(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                })
                .unwrap();
            store.apply(task.id, Action::Start).unwrap();
            store
                .apply(task.id, Action::Complete(Some(summary.into())))
                .unwrap();
            let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            ));
            let app = App::new(store, ctx).unwrap();
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal
                .draw(|f| {
                    draw(f, &app);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        // The detail pane is the right-hand half of the cockpit, so a row of
        // the buffer holds other panes too; the block's rows are the ones with
        // a gutter, and a cell is looked up by its position in the row.
        let rows = |buf: &ratatui::buffer::Buffer, width: u16| -> Vec<String> {
            buf.content()
                .chunks(width as usize)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect()
        };

        // The pane's own border is a `│` too, so the gutter is identified by
        // the column it sits in rather than by the glyph alone.
        let col_of = |row: &str, needle: &str| row.find(needle).map(|b| row[..b].chars().count());
        let char_at = |row: &str, col: usize| row.chars().nth(col).unwrap_or(' ');
        let locate = |rows: &[String], needle: &str| -> (usize, usize) {
            rows.iter()
                .enumerate()
                .find_map(|(y, r)| col_of(r, needle).map(|c| (y, c)))
                .unwrap_or_else(|| panic!("no {needle:?} in {rows:?}"))
        };

        let wide = app(140);
        let wide_rows = rows(&wide, 140);
        let (heading, gutter) = locate(&wide_rows, "│ completion summary:");
        for expected in [
            "│ completion summary:",
            "│ Landed the parser.",
            "│ • rewrote the lexer",
            "│ • covered it",
        ] {
            assert!(
                wide_rows.iter().any(|r| r.contains(expected)),
                "{wide_rows:?}"
            );
        }
        // The bar runs the block's full height — the blank line between the
        // summary and its bullets included.
        for y in heading..heading + 5 {
            assert_eq!(
                char_at(&wide_rows[y], gutter),
                '│',
                "row {y}: {wide_rows:?}"
            );
        }
        // The markers themselves are gone: styling replaced them.
        let all: String = wide_rows.concat();
        assert!(!all.contains("**Landed**"), "{all}");
        assert!(!all.contains("`parser`"), "{all}");

        // The styling lands on the right cells: bold "Landed", cyan "parser",
        // and neither colour bleeding onto the plain text between them.
        let row = heading as u16 + 1;
        let cell = |dx: u16| wide.cell((gutter as u16 + dx, row)).unwrap();
        assert_eq!(cell(0).symbol(), "│");
        assert_eq!(cell(0).fg, Color::Cyan);
        assert!(cell(2).modifier.contains(Modifier::BOLD), "'L' of Landed");
        assert_eq!(cell(2).fg, Color::Reset, "bold, not cyan");
        // "│ Landed the parser." — the 'p' of the inline code is 13 in.
        assert_eq!(cell(13).symbol(), "p");
        assert_eq!(cell(13).fg, Color::Cyan, "inline code stays cyan");
        assert!(!cell(13).modifier.contains(Modifier::BOLD));
        // The body is named, and named in the operator's voice: no gutter, so
        // its text starts in the column the bar occupies above.
        let (task_row, task_col) = locate(&wide_rows, "task:");
        assert_eq!(task_col, gutter, "{wide_rows:?}");

        // Narrow enough that the bullet cannot fit on one row: the
        // continuation keeps the gutter, which is what pre-wrapping buys.
        let narrow_rows = rows(&app(60), 60);
        let (bullet, ncol) = locate(&narrow_rows, "│ • rewrote the lexer");
        assert!(
            !narrow_rows[bullet].contains("reporter"),
            "the line was meant to be too long to fit: {narrow_rows:?}"
        );
        assert_eq!(
            char_at(&narrow_rows[bullet + 1], ncol),
            '│',
            "continuation lost the gutter: {narrow_rows:?}"
        );
        let (tail, _) = locate(&narrow_rows, "reporter");
        assert!(tail > bullet && tail <= task_row + 6, "{narrow_rows:?}");
    }

    /// A review card leads with the agent's account of what it did, above the
    /// body it was given (task #407) — on a first review, where there is no
    /// rejection behind it, and on a rework, where the feedback heads it.
    #[test]
    fn review_card_shows_the_completion_summary_above_the_body() {
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
                repo_id: None,
                title: "greet the reader".into(),
                body: "acceptance: the greeting renders".into(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store.apply(task.id, Action::Start).unwrap();
        store
            .apply(
                task.id,
                Action::Complete(Some("README.md: +2 lines\nmain.rs: untouched".into())),
            )
            .unwrap();

        let width: u16 = 100;
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        let rows = |app: &App, terminal: &mut Terminal<TestBackend>| -> Vec<String> {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .chunks(width as usize)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect()
        };
        let row_of = |rows: &[String], needle: &str| -> Option<usize> {
            rows.iter().position(|r| r.contains(needle))
        };

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        let first = rows(&app, &mut terminal);
        let heading =
            row_of(&first, "│ completion summary:").unwrap_or_else(|| panic!("{first:?}"));
        // Each summary line on its own row behind the gutter, as the question
        // block renders (task #430).
        assert!(
            row_of(&first, "│ README.md: +2 lines").is_some(),
            "{first:?}"
        );
        let last = row_of(&first, "│ main.rs: untouched").unwrap_or_else(|| panic!("{first:?}"));
        let named = row_of(&first, "task:").unwrap_or_else(|| panic!("{first:?}"));
        let body = row_of(&first, "acceptance: the greeting renders")
            .unwrap_or_else(|| panic!("{first:?}"));
        assert!(heading < last && last < named && named < body, "{first:?}");

        // Sent back and completed again: the same block, headed by the feedback
        // the new summary answers rather than by the neutral title.
        app.store
            .apply(task.id, Action::RejectWork("tests missing".into()))
            .unwrap();
        app.store
            .apply(task.id, Action::Complete(Some("added the tests".into())))
            .unwrap();
        app.refresh().unwrap();
        let second = rows(&app, &mut terminal);
        assert!(
            row_of(&second, "│ response to the review feedback:").is_some(),
            "{second:?}"
        );
        assert!(row_of(&second, "│ added the tests").is_some(), "{second:?}");
        assert!(
            row_of(&second, "completion summary:").is_none(),
            "{second:?}"
        );
    }

    /// The cockpit key line only advertises the selection-only actions while a
    /// task is selected — with an empty queue there is nothing for them to act
    /// on, so they drop out.
    #[test]
    fn cockpit_key_line_drops_the_selection_only_actions_without_a_selection() {
        use crate::app::App;
        use voro_core::{NewTask, Store};

        let ctx = || {
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            ))
        };

        let mut empty = App::new(Store::open_in_memory().unwrap(), ctx()).unwrap();
        // A project-less database opens on Projects (DESIGN.md §9); the cockpit
        // this test is about is the one reached by tabbing back to it.
        empty.screen = Screen::Cockpit;
        assert!(empty.selected_task_id().is_none());
        let labels: Vec<&str> = key_hints(&empty).iter().map(|(_, l)| *l).collect();
        for dropped in ["dispatch", "deep"] {
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
                repo_id: None,
                title: "a task".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        let selected = App::new(store, ctx()).unwrap();
        assert!(selected.selected_task_id().is_some());
        let labels: Vec<&str> = key_hints(&selected).iter().map(|(_, l)| *l).collect();
        for shown in ["dispatch", "deep"] {
            assert!(
                labels.contains(&shown),
                "cockpit with a selection should advertise {shown}: {labels:?}"
            );
        }
    }

    /// The review cluster's gating (DESIGN.md §9): `o` is advertised wherever
    /// there is a diff to look at — a `review` or `running` task carrying a
    /// branch — `g` only on `review`, where the PR is the review, and `!`
    /// nowhere past dispatch. A `ready` row, which has none of the three
    /// states, shows the mirror image.
    #[test]
    fn the_key_line_advertises_the_review_keys_only_where_they_act() {
        use crate::app::App;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = |title: &str| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
        };
        store.create_task(task("ready to go")).unwrap();
        let running = store.create_task(task("under way")).unwrap();
        store.apply(running.id, Action::Start).unwrap();
        store
            .set_branch(running.id, Some("feat/under-way"))
            .unwrap();
        let reviewed = store.create_task(task("in review")).unwrap();
        store.apply(reviewed.id, Action::Start).unwrap();
        store.apply(reviewed.id, Action::Complete(None)).unwrap();
        store
            .set_branch(reviewed.id, Some("feat/in-review"))
            .unwrap();

        let mut app = App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();
        app.screen = Screen::Tasks;

        let keys_for = |app: &mut App, title: &str| {
            app.tasks_sel = app
                .all
                .iter()
                .position(|row| row.task.title == title)
                .unwrap_or_else(|| panic!("no {title:?} row"));
            key_hints(app)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };

        let ready = keys_for(&mut app, "ready to go");
        assert!(!ready.contains(&"o"), "{ready:?}");
        assert!(!ready.contains(&"g"), "{ready:?}");
        assert!(ready.contains(&"!"), "{ready:?}");

        let running = keys_for(&mut app, "under way");
        assert!(running.contains(&"o"), "{running:?}");
        assert!(!running.contains(&"g"), "{running:?}");
        assert!(running.contains(&"!"), "{running:?}");

        let review = keys_for(&mut app, "in review");
        assert!(review.contains(&"o"), "{review:?}");
        assert!(review.contains(&"g"), "{review:?}");
        assert!(!review.contains(&"!"), "{review:?}");
    }

    /// The other half of that gating: both review keys are earned by having
    /// something to show, not by the state alone (DESIGN.md §9). A task whose
    /// whole product is its summary reaches `review` with no branch and no PR,
    /// and there the line advertises neither — the same row whose next-action
    /// is *accept* rather than *pr*. A tracked PR earns `g` back on its own;
    /// `o` needs the branch it diffs.
    #[test]
    fn the_review_keys_need_a_branch_or_a_pr_to_show_for() {
        use crate::app::App;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = |title: &str| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
        };
        let reviewed = |store: &mut Store, title: &str| {
            let t = store.create_task(task(title)).unwrap();
            store.apply(t.id, Action::Start).unwrap();
            store.apply(t.id, Action::Complete(None)).unwrap();
            t.id
        };
        let bare = reviewed(&mut store, "nothing to show");
        let tracked = reviewed(&mut store, "a PR and no branch");
        store
            .set_pr(tracked, Some("https://github.com/o/r/pull/1"))
            .unwrap();
        // A dispatch that has not named a branch yet has nothing to diff either.
        let running = store.create_task(task("under way")).unwrap();
        store.apply(running.id, Action::Start).unwrap();

        let mut app = App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();

        // Both lines read the same selection, so each screen is walked to the
        // row that names the task rather than indexed into directly.
        let keys_for = |app: &mut App, id: i64| {
            let rows = match app.screen {
                Screen::Cockpit => app.cockpit_rows.len(),
                _ => app.all.len(),
            };
            let found = (0..rows).any(|i| {
                match app.screen {
                    Screen::Cockpit => app.cockpit_sel = i,
                    _ => app.tasks_sel = i,
                }
                app.selected_task_id() == Some(id)
            });
            assert!(found, "{:?} has no row for task {id}", app.screen);
            key_hints(app)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };

        for screen in [Screen::Cockpit, Screen::Tasks] {
            app.screen = screen;
            let keys = keys_for(&mut app, bare);
            assert!(!keys.contains(&"o"), "{screen:?}: {keys:?}");
            assert!(!keys.contains(&"g"), "{screen:?}: {keys:?}");
            // The rest of the review row is untouched — the slots freed are
            // exactly the two that could not act.
            assert!(keys.contains(&"w"), "{screen:?}: {keys:?}");

            let keys = keys_for(&mut app, tracked);
            assert!(keys.contains(&"g"), "{screen:?}: {keys:?}");
            assert!(!keys.contains(&"o"), "{screen:?}: {keys:?}");

            let keys = keys_for(&mut app, running.id);
            assert!(!keys.contains(&"o"), "{screen:?}: {keys:?}");
        }
    }

    /// A lowercase/uppercase pair of one action takes a single slot keyed on
    /// the pair, and the line stays short enough to scan: eleven slots or fewer
    /// on every row of a queue holding one of each kind (DESIGN.md §9). Eleven
    /// rather than ten because a `review` row carries the whole review cluster
    /// — `⏎`, `w`, `o`, `g` — the one moment all four are live options; `!`
    /// dropping off that row is what keeps even eleven reachable.
    #[test]
    fn the_key_line_pairs_its_slots_and_stays_at_eleven() {
        use crate::app::App;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        // A proposal earns the refine slot and a review task the hand-off slot;
        // between them every conditional cockpit slot is covered.
        let task = |title: &str, state: TaskState| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
            deep: false,
        };
        store
            .create_task(task("a proposal", TaskState::Proposed))
            .unwrap();
        store
            .create_task(task("ready to go", TaskState::Ready))
            .unwrap();
        // The review task carries a session and a branch, which is the worst
        // case for the line: it earns the hand-off slot and the message slot at
        // once, and the branch is what earns it the two review slots.
        let reviewed = store
            .create_task(task("in review", TaskState::Ready))
            .unwrap();
        store
            .record_dispatch(reviewed.id, "claude", None, LivenessSource::Pid, None)
            .unwrap();
        store.apply(reviewed.id, Action::Complete(None)).unwrap();
        store
            .set_branch(reviewed.id, Some("feat/in-review"))
            .unwrap();
        // ...and a refine in flight for the cancel slot, which rides the strip
        // rather than the queue.
        let refining = store
            .create_task(task("being rewritten", TaskState::Proposed))
            .unwrap();
        store
            .record_refine_launch(
                refining.id,
                "thin body",
                "claude",
                None,
                LivenessSource::Pid,
                None,
            )
            .unwrap();
        let mut app = App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();

        let mut seen_refine = false;
        let mut seen_wait = false;
        let mut seen_cancel = false;
        let mut seen_message = false;
        let mut seen_review_keys = false;
        for screen in [
            Screen::Cockpit,
            Screen::Tasks,
            Screen::Projects,
            Screen::Config,
        ] {
            app.screen = screen;
            let mut i = 0;
            loop {
                let rows = match screen {
                    // Re-read each time: folding a digest open below adds rows.
                    Screen::Cockpit => app.cockpit_rows.len(),
                    Screen::Tasks => app.all.len(),
                    _ => 1,
                };
                if i >= rows {
                    break;
                }
                match screen {
                    Screen::Cockpit => app.cockpit_sel = i,
                    Screen::Tasks => app.tasks_sel = i,
                    _ => {}
                }
                // Proposals ride as a digest row; fold it open so the proposal
                // itself — the row the refine slot answers on — is selectable.
                if app.enter_hint() == Some("⏎ expand") {
                    app.on_key(KeyEvent::from(KeyCode::Enter));
                }
                let keys: Vec<&str> = key_hints(&app).iter().map(|(k, _)| *k).collect();
                assert!(
                    keys.len() <= 11,
                    "{screen:?} row {i} shows {} slots: {keys:?}",
                    keys.len()
                );
                assert!(keys.contains(&"?"), "{screen:?} must advertise ?: {keys:?}");
                // The task screens pair a lowercase key with its uppercase
                // variant; the other two bind unrelated actions to the same
                // letter, so their slots stay apart.
                if matches!(screen, Screen::Cockpit | Screen::Tasks) {
                    for lone in ["d", "D", "r", "R", "n", "N"] {
                        assert!(
                            !keys.contains(&lone),
                            "{screen:?} should pair {lone} into one slot: {keys:?}"
                        );
                    }
                }
                seen_refine |= keys.contains(&"r/R");
                seen_wait |= keys.contains(&"w");
                seen_cancel |= keys.contains(&"C");
                seen_message |= keys.contains(&"a/A");
                seen_review_keys |= keys.contains(&"o") && keys.contains(&"g");
                // Dispatch is advertised only where it can act, so the line
                // never offers a verb whose only answer is the state it
                // refuses — which is also what buys the message slot its room.
                assert!(
                    !(keys.contains(&"d/D") && keys.contains(&"a/A")),
                    "{screen:?} row {i}: {keys:?}"
                );
                // The refine keys and the cancel are mutually exclusive by
                // state, which is what keeps the line within its eleven slots.
                assert!(
                    !(keys.contains(&"r/R") && keys.contains(&"C")),
                    "{screen:?} row {i}: {keys:?}"
                );
                i += 1;
            }
        }
        assert!(
            seen_refine && seen_wait && seen_cancel && seen_message && seen_review_keys,
            "the conditional slots never showed"
        );
    }

    /// The line may drop a key, but the map may not: every key the hint line
    /// can show is in that screen's key map, so trimming the line never makes
    /// a key undiscoverable (DESIGN.md §9).
    #[test]
    fn every_hinted_key_appears_in_the_key_map() {
        use crate::app::App;

        // A combined slot (`d/D`, `j/k`) stands for its individual keys on both
        // sides, so compare key by key.
        let split = |key: &'static str| key.split('/').collect::<Vec<_>>();
        for screen in [
            Screen::Cockpit,
            Screen::Tasks,
            Screen::Projects,
            Screen::Config,
        ] {
            let mut app = App::new(
                voro_core::Store::open_in_memory().unwrap(),
                crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                    "/nonexistent/voro.db",
                )),
            )
            .unwrap();
            app.screen = screen;
            let mapped: Vec<&str> = key_map(screen, app.projects.is_empty())
                .iter()
                .flat_map(|(_, entries)| entries.iter())
                .flat_map(|(key, _)| split(key))
                .collect();
            for (key, ..) in hint_candidates(&app) {
                for one in split(key) {
                    assert!(
                        mapped.contains(&one),
                        "{screen:?} hints {key:?} but its key map omits {one:?}: {mapped:?}"
                    );
                }
            }
        }
    }

    /// The case convention (DESIGN.md §9): an uppercase key is the interactive
    /// half of a lowercase/uppercase pair, or one of the exceptions the doc
    /// names. A new uppercase binding that is neither fails here, which is the
    /// point — it is the prompt to decide which of the two it is.
    #[test]
    fn every_uppercase_key_is_paired_or_a_named_exception() {
        let paired: Vec<&str> = [DISPATCH_KEYS, REFINE_KEYS, NEW_KEYS, MESSAGE_KEYS]
            .iter()
            .flat_map(|set| set.iter())
            .map(|(key, _)| *key)
            .collect();
        for screen in [
            Screen::Cockpit,
            Screen::Tasks,
            Screen::Projects,
            Screen::Config,
        ] {
            for no_projects in [false, true] {
                let uppercase = key_map(screen, no_projects)
                    .into_iter()
                    .flat_map(|(_, entries)| entries)
                    .flat_map(|(key, _)| key.split('/').collect::<Vec<_>>())
                    .filter(|key| key.chars().count() == 1 && key.chars().all(char::is_uppercase));
                for key in uppercase {
                    assert!(
                        paired.contains(&key) || CASE_EXCEPTIONS.contains(&(screen, key)),
                        "{screen:?} binds {key:?} uppercase, but it is neither half of a pair \
                         nor a documented exception — see DESIGN.md §9"
                    );
                }
            }
        }
    }

    /// `?` opens the current screen's map from any screen, listing the keys the
    /// line has no room for and the gloss for each uppercase variant; any key
    /// closes it again (DESIGN.md §9).
    #[test]
    fn the_key_map_overlay_opens_on_question_mark_and_closes_on_any_key() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};

        let mut app = App::new(
            voro_core::Store::open_in_memory().unwrap(),
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();
        app.screen = Screen::Projects;
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(matches!(app.mode, Mode::KeyMap { .. }));

        let width: u16 = 120;
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Keys — projects"), "{rendered}");
        assert!(rendered.contains("archive or unarchive"), "{rendered}");

        // Any key is a dismissal, including one the screen binds.
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// The map has to *hold* what it lists: on the smallest terminal worth
    /// supporting, every entry of every screen is reachable — paged to with
    /// `tab` where one screenful cannot carry it, and never clipped, key and
    /// whole gloss both. Asserting a handful of entries instead is what let the
    /// list outgrow the overlay in the first place: the two that fell off the
    /// bottom were simply not among the ones checked.
    #[test]
    fn every_key_map_entry_is_reachable_on_a_small_terminal() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};

        const W: u16 = 80;
        const H: u16 = 24;

        for projects in [0, 1] {
            let mut store = voro_core::Store::open_in_memory().unwrap();
            for i in 0..projects {
                store.create_project(&format!("p{i}"), "/tmp").unwrap();
            }
            let mut app = App::new(
                store,
                crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                    "/nonexistent/voro.db",
                )),
            )
            .unwrap();
            let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();

            for screen in [
                Screen::Cockpit,
                Screen::Tasks,
                Screen::Projects,
                Screen::Config,
            ] {
                app.screen = screen;
                app.on_key(KeyEvent::from(KeyCode::Char('?')));
                // More turns than any screen's map has pages; the page index
                // wraps, so the extra ones re-read a page already seen.
                let mut seen = String::new();
                for _ in 0..8 {
                    terminal
                        .draw(|f| {
                            draw(f, &app);
                        })
                        .unwrap();
                    seen.push_str(
                        &terminal
                            .backend()
                            .buffer()
                            .content()
                            .chunks(W as usize)
                            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                    seen.push('\n');
                    app.on_key(KeyEvent::from(KeyCode::Tab));
                }
                app.on_key(KeyEvent::from(KeyCode::Esc));

                for (title, entries) in key_map(screen, app.projects.is_empty()) {
                    assert!(seen.contains(title), "{screen:?}: no {title:?}:\n{seen}");
                    for (key, label) in entries {
                        // Keys are right-aligned with two trailing spaces, so
                        // the pair is one substring of the row it renders on.
                        let row = format!("{key}  {label}");
                        assert!(
                            seen.contains(&row),
                            "{screen:?} with {projects} project(s): {row:?} is in the key map but \
                             no page of it at {W}x{H} shows that entry whole — the map has \
                             outgrown its overlay (DESIGN.md §9):\n{seen}"
                        );
                    }
                }
            }
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
            refining: 2,
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
        // A proposal mid-rewrite has left the triage count, so it is named here
        // rather than silently missing from the backlog (DESIGN.md §6/§12).
        assert!(text.contains("refining 2"), "{text}");
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
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
            deep: false,
        };
        store.create_task(new("idea", TaskState::Proposed)).unwrap();
        store.create_task(new("go", TaskState::Ready)).unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
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

    /// The create-PR modal spells out every consequence of confirming, the
    /// browser jump included (DESIGN.md §8), so the key's second half is not a
    /// surprise.
    #[test]
    fn confirm_pr_modal_announces_the_browser_jump() {
        use crate::app::{App, Mode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "ship it".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.mode = Mode::ConfirmPr {
            task_id: task.id,
            branch: "feat/ship".into(),
            title: "ship it".into(),
        };

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("open it in the browser"),
            "modal should name the browser jump: {rendered}"
        );
    }

    // --- mouse (DESIGN.md §9) ---
    //
    // Every click test goes through the real draw, so what it clicks is the
    // geometry the operator sees: the row is found by the text on it rather
    // than by a coordinate the test picked.

    type TestTerminal = ratatui::Terminal<ratatui::backend::TestBackend>;

    /// Draw a frame and keep the hit-map it built — the pair the event loop
    /// holds between a draw and the next click.
    fn frame(app: &crate::app::App, terminal: &mut TestTerminal) -> HitMap {
        let mut hits = HitMap::default();
        terminal.draw(|f| hits = draw(f, app)).unwrap();
        hits
    }

    /// One drawn row, cell by cell — the column of a cell is its index, which
    /// is what a click is addressed in.
    fn cells_at(terminal: &TestTerminal, y: u16) -> Vec<String> {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn line_at(terminal: &TestTerminal, y: u16) -> String {
        cells_at(terminal, y).concat()
    }

    /// Where on screen some text was drawn, as the point a click on it would
    /// land — so a test aims at what the operator sees rather than at a
    /// coordinate it worked out for itself.
    fn point_of(terminal: &TestTerminal, needle: &str) -> (u16, u16) {
        let height = terminal.backend().buffer().area.height;
        for y in 0..height {
            let cells = cells_at(terminal, y);
            for x in 0..cells.len() {
                if cells[x..].concat().starts_with(needle) {
                    return (x as u16, y);
                }
            }
        }
        panic!("'{needle}' is not on screen");
    }

    fn row_of(terminal: &TestTerminal, needle: &str) -> u16 {
        point_of(terminal, needle).1
    }

    fn ready_task(store: &mut voro_core::Store, project_id: i64, title: &str) -> voro_core::Task {
        store
            .create_task(voro_core::NewTask {
                project_id,
                repo_id: None,
                title: title.into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap()
    }

    fn test_app(store: voro_core::Store) -> crate::app::App {
        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        crate::app::App::new(store, ctx).unwrap()
    }

    /// A click on a cockpit row selects it — in the queue and in the running
    /// strip alike, which are one selection despite being two panes — and the
    /// detail pane follows, since it reads the same selection the keys move.
    #[test]
    fn cockpit_click_selects_the_row_under_the_pointer() {
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let first = ready_task(&mut store, p.id, "first");
        let second = ready_task(&mut store, p.id, "second");
        let third = ready_task(&mut store, p.id, "third");
        let live = ready_task(&mut store, p.id, "in flight");
        store
            .record_dispatch(live.id, "claude", None, LivenessSource::Pid, None)
            .unwrap();

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        let hits = frame(&app, &mut terminal);
        assert_eq!(app.selected_task_id(), Some(first.id));

        // A queue row two below the selection: the click moves the selection
        // there and nowhere else — no transition menu, no dispatch.
        let (x, y) = point_of(&terminal, &task_ref(third.id));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_task_id(), Some(third.id));
        assert!(matches!(app.mode, Mode::Normal));

        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, &task_ref(second.id));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_task_id(), Some(second.id));

        // The running strip is a separate pane over the same row space.
        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, &task_ref(live.id));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_task_id(), Some(live.id));
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Running(_)
        ));
    }

    /// The panes that show rather than list — the detail card, the header, the
    /// status line — are dead zones, so a click in one changes nothing.
    #[test]
    fn clicks_outside_a_list_do_nothing() {
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let first = ready_task(&mut store, p.id, "first");
        ready_task(&mut store, p.id, "second");

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        let hits = frame(&app, &mut terminal);

        // The header, the detail pane's own border and body, the status line.
        let detail = row_of(&terminal, "Detail");
        for y in [0, detail, detail + 2, 23] {
            app.on_mouse(5, y, &hits);
            assert_eq!(
                app.selected_task_id(),
                Some(first.id),
                "a click at row {y} should not move the selection"
            );
        }
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// A full-screen list scrolled past its first page still maps the line
    /// under the pointer to the item drawn on it, because the hit-map is built
    /// from the scroll offset ratatui itself computed while rendering.
    #[test]
    fn tasks_browser_click_selects_the_right_row_when_scrolled() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        for i in 0..40 {
            ready_task(&mut store, p.id, &format!("task {i}"));
        }

        let mut app = test_app(store);
        alt_screen(&mut app, '2');
        assert_eq!(app.screen, Screen::Tasks);
        // Walk the selection off the bottom of the pane so the list scrolls.
        for _ in 0..35 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }

        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        let hits = frame(&app, &mut terminal);
        // The first row on screen is no longer the first task, so a hit-map
        // that ignored the offset would be off by exactly that much.
        assert!(
            !line_at(&terminal, 1).contains(&task_ref(app.all[0].task.id)),
            "the list should have scrolled: {}",
            line_at(&terminal, 1)
        );

        let expected = app.all[30].task.id;
        let (x, y) = point_of(&terminal, &task_ref(expected));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.all[app.tasks_sel].task.id, expected);
        assert!(matches!(app.mode, Mode::Normal), "a click opens no popup");
    }

    /// The other two screens list the same way and click the same way.
    #[test]
    fn projects_and_config_clicks_select_rows() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let dir = std::env::temp_dir().join(format!(
            "voro-ui-mouse-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let agents_path = dir.join("voro.toml");
        std::fs::write(
            &agents_path,
            "[viewers.zed]\ncmd = \"zed {path}\"\n\n[viewers.diff]\ncmd = \"git diff {base}\"\n",
        )
        .unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store.create_project("alpha", "/tmp/alpha").unwrap();
        store.create_project("beta", "/tmp/beta").unwrap();
        store.create_project("gamma", "/tmp/gamma").unwrap();

        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = crate::app::App::new(store, ctx).unwrap();
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();

        alt_screen(&mut app, '3');
        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, "gamma");
        app.on_mouse(x, y, &hits);
        assert_eq!(app.projects[app.projects_sel].name, "gamma");

        // A bare digit on the projects screen is a weight, so leave by the keys
        // that are not: tab across to Config.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Config);
        let hits = frame(&app, &mut terminal);
        let target = app.config_viewers[1].name.clone();
        let (x, y) = point_of(&terminal, &target);
        app.on_mouse(x, y, &hits);
        assert_eq!(app.config_viewers[app.config_sel].name, target);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// In a picker a click moves the cursor, and a second click on the option
    /// already under it confirms — the same ⏎ the keyboard would send, so the
    /// transition runs through the state machine unchanged.
    #[test]
    fn picker_click_selects_then_a_second_click_confirms() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = ready_task(&mut store, p.id, "startable");

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        // ready → [start, park, abandon]; park is the one below the cursor.
        assert!(matches!(app.mode, Mode::Transition { sel: 0, .. }));

        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, "park → parked");
        app.on_mouse(x, y, &hits);
        assert!(
            matches!(app.mode, Mode::Transition { sel: 1, .. }),
            "the first click should only move the cursor"
        );
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Ready);

        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, "park → parked");
        app.on_mouse(x, y, &hits);
        assert!(matches!(app.mode, Mode::Normal), "the picker should close");
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Parked);
    }

    /// A popup owns the pointer: a click beside it is not a dismiss, and the
    /// list it covers is not clickable through it. Text-entry modes have no
    /// options to click, so they ignore the mouse entirely.
    #[test]
    fn clicks_outside_a_popup_and_in_text_entry_modes_do_nothing() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let first = ready_task(&mut store, p.id, "first");
        let second = ready_task(&mut store, p.id, "second");

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();

        // The row the popup does not cover would have been a target a moment ago.
        let hits = frame(&app, &mut terminal);
        let (x_second, y_second) = point_of(&terminal, &task_ref(second.id));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Transition { .. }));

        let hits_modal = frame(&app, &mut terminal);
        assert!(hits.at(x_second, y_second).is_some());
        for (col, row) in [(0, 0), (x_second, y_second), (99, 23)] {
            app.on_mouse(col, row, &hits_modal);
        }
        assert!(
            matches!(app.mode, Mode::Transition { sel: 0, .. }),
            "clicks outside the popup should neither dismiss nor move it"
        );
        assert_eq!(app.selected_task_id(), Some(first.id));

        // A text prompt: nothing in the frame is a click target at all.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.mode = Mode::Prompt {
            task_id: first.id,
            kind: crate::app::PromptKind::Ask,
            buffer: "half typed".into(),
        };
        let hits = frame(&app, &mut terminal);
        for y in 0..24 {
            for x in [0, 5, 50, 99] {
                assert!(
                    hits.at(x, y).is_none(),
                    "a text prompt should offer no click target at ({x}, {y})"
                );
            }
        }
    }

    /// A prompt buffer wider than the popup wraps, and the box grows with the
    /// wrapped text so the tail being typed — and the cursor — stay on screen.
    #[test]
    fn prompt_popup_grows_with_wrapped_text() {
        let buffer = format!("HEADMARK {}TAILMARK", "padding ".repeat(12));
        let rendered = render_prompt(&buffer);
        assert!(
            rendered.contains("HEADMARK"),
            "the start of a wrapped note stays visible: {rendered}"
        );
        assert!(
            rendered.contains("TAILMARK▏"),
            "the tail and the cursor stay visible: {rendered}"
        );
    }

    /// Past the height clamp the popup stops growing, so it scrolls to the end
    /// of the text: the tail shows, the head scrolls away.
    #[test]
    fn prompt_popup_scrolls_to_the_tail_when_it_overflows() {
        let buffer = format!("HEADMARK {}TAILMARK", "padding ".repeat(200));
        let rendered = render_prompt(&buffer);
        assert!(
            rendered.contains("TAILMARK▏"),
            "the tail and the cursor stay visible: {rendered}"
        );
        assert!(
            !rendered.contains("HEADMARK"),
            "the head scrolls out of the clamped box: {rendered}"
        );
    }

    /// Draws a `Mode::Prompt` over an otherwise empty app and returns the
    /// terminal's cells as one string.
    fn render_prompt(buffer: &str) -> String {
        use crate::app::{App, Mode, PromptKind};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::Store;

        let store = Store::open_in_memory().unwrap();
        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.mode = Mode::Prompt {
            task_id: 1,
            kind: PromptKind::RefineNote,
            buffer: buffer.to_string(),
        };

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }
}
