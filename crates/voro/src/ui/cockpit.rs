//! Rendering for the cockpit (DESIGN.md §9): the header's counts, the queue,
//! the running strip and the focus card.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use voro_core::{ActionRow, DepKind, DigestRow, QueueRow, StateCounts, TaskState};

use super::milestones;
use super::rows::{
    agent_voice_block, blocks_span, branch_span, capped_span, completion_lines, conflict_span,
    deep_line, deep_marker, dep_lines, doc_lines, history_lines, human_line, human_span,
    incomplete_report_span, pr_span, question_summary, refine_failed_span, refined_span,
    refining_span, repo_span, score_lines, score_span, session_lines, state_span, strip_pr_span,
    waiting_span,
};
use super::status::{draw_status, status_height};
use super::{Hit, HitMap, SELECTED, task_ref};
use crate::app::{App, CockpitRow};

pub(super) fn draw_cockpit(frame: &mut Frame, app: &App, hits: &mut HitMap) {
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

/// One task's queue row: the score it was ranked by, its state, and
/// the markers the row carries. Shared by the top-level rows and the proposals
/// listed under an expanded digest, which differ only in indent and dimming.
/// `width` is the pane's inner width, whose right edge the milestone label
/// sits against.
fn action_row_line(app: &App, row: &ActionRow, indent: &str, width: u16) -> Line<'static> {
    let c = &row.candidate;
    let untriaged = c.task.state == TaskState::Proposed;
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
    let head = vec![
        score,
        Span::styled(format!("{indent}{} ", task_ref(c.task.id)), style),
        state_span(c.task.state),
        Span::styled(format!(" {}", c.task.priority), style),
        deep_marker(c.task.deep),
        Span::styled(format!(" {}: ", c.project_name), style),
    ];
    let title = Span::styled(c.task.title.clone(), style);
    let mut spans = Vec::new();
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
    milestones::row_line(app, c.task.id, width, head, title, spans)
}

/// A project's collapsed proposals (DESIGN.md §7): one row scored as its best
/// child, so a triage backlog stays felt without swamping the queue.
fn digest_line(app: &App, digest: &DigestRow) -> Line<'static> {
    let mut spans = vec![
        Span::styled(format!("{:5.1} ", digest.score), Style::new().dim()),
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
    let width = area.width.saturating_sub(2);
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected: Option<usize> = None;
    // The pane skips the running rows, so a rendered line stands for the
    // cockpit row this records, not for its own position.
    let mut rows: Vec<usize> = Vec::new();
    for (i, row) in app.cockpit_rows.iter().enumerate() {
        let item = match row {
            CockpitRow::Queue(idx) => match app.queue.rows.get(*idx) {
                Some(QueueRow::Action(row)) => ListItem::new(action_row_line(app, row, "", width)),
                Some(QueueRow::Digest(digest)) => ListItem::new(digest_line(app, digest)),
                None => continue,
            },
            CockpitRow::Proposal(i, j) => match app.digest_child(*i, *j) {
                Some(row) => ListItem::new(action_row_line(app, row, "  ↳ ", width)),
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
    // The cap cut rows that tie the last one shown, whose order among
    // themselves means nothing, so the pane counts them (DESIGN.md §7).
    if let Some(cut) = app.queue.tied_cut {
        block = block.title_bottom(
            Line::from(Span::styled(format!(" {cut} "), Style::new().dim())).right_aligned(),
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
            Span::styled(
                format!("{:5.1} ", row.candidate.score.total),
                Style::new().dim(),
            ),
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
                Some(row.candidate.score.total),
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
                Some(row.candidate.score.total),
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
                // No branch, so no PR and no diff — the report above is the
                // whole deliverable and ⏎ is where the verdict lives
                // (DESIGN.md §6).
                voro_core::NextAction::Accept => "  (⏎ offers accept)",
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
    lines.extend(milestones::detail_lines(app, task.id));
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
        lines.extend(score_lines(&b));
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
    let width = area.width.saturating_sub(2);
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
            let head = vec![
                Span::raw(format!("{} ", task_ref(r.task_id))),
                agent,
                state,
                Span::styled(
                    format!("{:>6}  ", format_elapsed(r.elapsed_secs)),
                    Style::new().dim(),
                ),
            ];
            let title = Span::raw(r.task_title.clone());
            let mut spans = Vec::new();
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
            if let Some(window) = app.cap_window(r.task_id) {
                spans.push(capped_span(&window));
            }
            // A hand-off has nothing left to be live: the work is with someone
            // else, so a closed session is the expected shape, not an orphan.
            if r.session_id.is_none() && !waiting {
                spans.push(Span::styled(
                    "  ⚠ no live session",
                    Style::new().fg(Color::Yellow),
                ));
            }
            items.push(ListItem::new(milestones::row_line(
                app, r.task_id, width, head, title, spans,
            )));
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ui::draw;
    use crate::ui::status::key_hints;
    use crate::ui::tasks::draw_tasks;
    use crate::ui::tests::{app_with_status, ready_task, screen_text, test_app};
    use voro_core::{LivenessSource, Priority};

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

    /// A `review` task carrying everything the key line can advertise: a branch
    /// (`o`), a pull request (`g`), and a session to message (`a/A`).
    pub(crate) fn app_in_review_with_everything() -> crate::app::App {
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "a task under review".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        store
            .record_dispatch(task.id, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        store.apply(task.id, Action::Complete(None)).unwrap();
        store.set_branch(task.id, Some("feat/x")).unwrap();
        store
            .set_pr(task.id, Some("https://github.com/o/r/pull/1"))
            .unwrap();

        let mut app = crate::app::App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/home/op/deeply/nested/scratch/store/voro.db",
            )),
        )
        .unwrap();
        app.status = None;
        app
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

        let project = tempfile::Builder::new()
            .prefix("voro-ui-remoteless-")
            .tempdir()
            .unwrap()
            .keep();
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
                milestone: false,
            })
            .unwrap()
            .id;
        store
            .record_dispatch(task, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        store
            .apply(task, Action::Complete(Some("did it".into())))
            .unwrap();
        // The degrade is about the medium of a diff, so the task must have one
        // to show: with no branch it would advertise `accept` (DESIGN.md §6).
        store.set_branch(task, Some("feat/finished")).unwrap();

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

        let project = tempfile::Builder::new()
            .prefix("voro-ui-half-report-")
            .tempdir()
            .unwrap()
            .keep();
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
                milestone: false,
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

    /// A review task with nothing to push — an investigation, an audit — asks
    /// to be accepted, not for a PR that could only fail (DESIGN.md §6). The
    /// card names that verb and the key that reaches it, and asks the checkout
    /// nothing: there is no medium to resolve, which is why this renders in a
    /// path that is no git repository at all.
    #[test]
    fn the_detail_card_advertises_accept_with_nothing_to_push() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store
            .create_project("voro", "/tmp/voro-not-a-repo")
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "the investigation".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap()
            .id;
        store
            .record_dispatch(task, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        store
            .apply(task, Action::Complete(Some("what I found".into())))
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
        assert!(out.contains("next: accept"), "{out}");
        assert!(out.contains("(⏎ offers accept)"), "{out}");
        assert!(!out.contains("next: pr"), "{out}");
        assert!(!out.contains("next: open"), "{out}");
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
                    milestone: false,
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
                milestone: false,
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
                retrying: false,
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

        // A session retrying the rejected request says so, and never says its
        // reset has passed however long ago the named time went by: it is
        // mid-turn, so the time is when its own request goes out rather than
        // when the operator should step in.
        app.caps.insert(
            task,
            CapReading {
                reset_minutes: Some(21 * 60 + 50),
                retrying: true,
            },
        );
        app.now_minutes = Some(22 * 60);
        let line = row(&app);
        assert!(line.contains("⚠ capped · retrying ↻21:50"), "{line}");
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
                    milestone: false,
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
                milestone: false,
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
                milestone: false,
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
                milestone: false,
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
            milestone: false,
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
            milestone: false,
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

    /// The queue pane counts the rows its cap cut at the last row's score, in
    /// the words the inbox uses (DESIGN.md §7).
    #[test]
    fn the_queue_pane_counts_rows_cut_at_a_tie() {
        use voro_core::{Store, TiedCut};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        ready_task(&mut store, p.id, "shown");
        let mut app = test_app(store);
        app.queue.tied_cut = Some(TiedCut {
            count: 38,
            score: 8.0,
        });

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        assert!(
            screen_text(&terminal).contains("+38 more at 8.0"),
            "{}",
            screen_text(&terminal)
        );
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
                milestone: false,
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

    /// The cockpit detail pane answers "what happened" for a stalled task:
    /// the dead session's outcome, agent, end time, and log path
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
                    milestone: false,
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
                milestone: false,
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
    /// start time, and log path instead of a post-mortem, and the
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
                milestone: false,
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
    /// marks the block as the agent's own words.
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
                milestone: false,
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

    /// A review card leads with the agent's account of what it did, above the
    /// body it was given — on a first review, where there is no
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
                milestone: false,
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
        // block renders.
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
            milestone: false,
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
}
