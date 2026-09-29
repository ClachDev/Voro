//! Rendering for the task browser's tree arrangement (DESIGN.md §9): the
//! indent and fold marker before a row, the milestone marker and hidden
//! counts after it, and the one-line reference a repeated task prints as.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use voro_core::TreeRow;

use super::milestones::milestone_style;
use super::task_ref;
use crate::app::{App, TaskRow};

/// Two columns per level, then the fold marker: `▸` closed, `▾` open, blank
/// on a row with nothing beneath it.
pub(super) fn prefix(app: &App, node: &TreeRow) -> String {
    let marker = if !node.is_fold() {
        " "
    } else if app.open_folds.contains(&node.id) {
        "▾"
    } else {
        "▸"
    };
    format!("{}{marker}", "  ".repeat(node.depth))
}

/// After the row: the milestone marker, and on a closed fold the counts of
/// what it hides.
pub(super) fn suffix(app: &App, node: &TreeRow, row: &TaskRow) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if row.task.milestone {
        spans.push(Span::styled("  [milestone]", milestone_style()));
    }
    if node.is_fold() && !app.open_folds.contains(&node.id) {
        let (open, done) = app.fold_counts(node);
        spans.push(Span::styled(
            format!("  {open} open · {done} done"),
            Style::new().dim(),
        ));
    }
    spans
}

/// A task already printed in full under another dependent: its id and
/// title, dimmed, pointing up the tree.
pub(super) fn reference_line(node: &TreeRow, row: &TaskRow) -> Line<'static> {
    Line::from(Span::styled(
        format!(
            "{}↑ {} {}",
            "  ".repeat(node.depth),
            task_ref(row.task.id).trim(),
            row.task.title
        ),
        Style::new().dim(),
    ))
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use voro_core::{Priority, Store, TaskState};

    use crate::app::milestones::tests::{app_from, new_task};
    use crate::app::{App, Screen};

    fn frame(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|f| {
                crate::ui::draw(f, app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(100)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A milestone with two members that share a blocker: closed, the fold
    /// shows its marker and counts; open, the shared blocker prints in full
    /// once and as a reference once, each level two columns deeper.
    #[test]
    fn the_tree_draws_folds_markers_and_references() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Dock", "", Priority::P2, TaskState::Parked)
            .unwrap()
            .id;
        let x = new_task(&mut store, p, "first member", TaskState::Ready);
        let y = new_task(&mut store, p, "second member", TaskState::Ready);
        let shared = new_task(&mut store, p, "shared blocker", TaskState::Ready);
        store.block_tasks(x, &[m]).unwrap();
        store.block_tasks(y, &[m]).unwrap();
        store.block_tasks(shared, &[x, y]).unwrap();
        let mut app = app_from(store);
        app.screen = Screen::Tasks;
        app.on_key(KeyEvent::from(KeyCode::Char('t')));

        let closed = frame(&app);
        assert!(closed.contains("All tasks — by blockers"), "{closed}");
        assert!(closed.contains(&format!("▸  #{m} parked")), "{closed}");
        assert!(closed.contains("[milestone]  3 open · 0 done"), "{closed}");
        assert!(!closed.contains("first member"), "{closed}");

        for code in [' ', 'j', ' ', 'j', 'j', ' '] {
            app.on_key(KeyEvent::from(KeyCode::Char(code)));
        }
        let open = frame(&app);
        assert!(open.contains(&format!("  ▾  #{x} parked")), "{open}");
        assert!(open.contains(&format!("       #{shared} ready")), "{open}");
        assert!(
            open.contains(&format!("    ↑ #{shared} shared blocker")),
            "{open}"
        );
    }
}
