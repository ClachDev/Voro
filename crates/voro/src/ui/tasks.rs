//! Rendering for the task browser (DESIGN.md §9): its rows, their blockers,
//! and the review verbs a row advertises.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::rows::{
    deep_marker, human_span, incomplete_report_span, refine_failed_span, refined_span,
};
use super::status::{draw_status, status_height};
use super::{Hit, HitMap, SELECTED, task_ref};
use super::{milestones, tree};
use crate::app::{App, BrowserRow, TaskRow};

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

pub(super) fn draw_tasks(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let [list_area, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    let items: Vec<ListItem> = app
        .browser_rows
        .iter()
        .map(|row| {
            let (r, node) = match row {
                BrowserRow::Group(group) => {
                    return ListItem::new(milestones::group_line(app, *group));
                }
                BrowserRow::Task(i) => (&app.all[*i], None),
                BrowserRow::Node { row, task } => {
                    let node = &app.tree_rows[*row];
                    let r = &app.all[*task];
                    if node.reference {
                        return ListItem::new(tree::reference_line(node, r));
                    }
                    (r, Some(node))
                }
            };
            let indent = match node {
                Some(node) => tree::prefix(app, node),
                None if app.browse_by_milestone => "  ".to_string(),
                None => String::new(),
            };
            let closed = r.task.state.is_terminal();
            let style = if closed || r.weight == 0 {
                Style::new().dim()
            } else {
                Style::new()
            };
            let mut spans = vec![
                Span::styled(
                    format!(
                        "{indent}{} {:11} {}",
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
            if let Some(node) = node {
                spans.extend(tree::suffix(app, node, r));
            }
            spans.extend(blocker_spans(r));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let empty = app.all.is_empty();
    let mut state =
        ListState::default().with_selected(if empty { None } else { Some(app.tasks_sel) });
    let title = if app.browse_by_milestone {
        "All tasks — by milestone".to_string()
    } else if app.browse_tree {
        format!("All tasks — by blockers — {}", tree::hidden(app))
    } else {
        "All tasks".to_string()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, list_area, &mut state);
    hits.push_list(
        list_area,
        state.offset(),
        app.browser_rows.len(),
        Hit::TaskRow,
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;
    use voro_core::{DepKind, DepRef, Priority, Task, TaskState};

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
                milestone: false,
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
            milestone: false,
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

    #[test]
    fn non_parked_and_blockerless_rows_get_no_suffix() {
        assert!(
            blocker_spans(&row(TaskState::Ready, vec![blocker(4, TaskState::Done)])).is_empty()
        );
        assert!(blocker_spans(&row(TaskState::Parked, vec![])).is_empty());
    }
}
