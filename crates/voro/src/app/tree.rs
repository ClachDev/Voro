//! The task browser's tree arrangement (DESIGN.md §9): the browser's tasks
//! laid out by their `blocks` edges on `t`, each fold opened and closed on
//! space. `voro tree` prints one task's part of the same tree.

use std::collections::HashMap;

use voro_core::{DepKind, DepRef, TaskState, Tree, TreeRow};

use super::{App, BrowserRow};

/// The `blocks` edges out of a dependency map, as the tree builder takes
/// them: each task to the tasks that block it.
pub(crate) fn blocks_edges(deps: &HashMap<i64, Vec<DepRef>>) -> HashMap<i64, Vec<i64>> {
    deps.iter()
        .map(|(task, deps)| {
            let blockers = deps
                .iter()
                .filter(|d| d.kind == DepKind::Blocks)
                .map(|d| d.id)
                .collect();
            (*task, blockers)
        })
        .collect()
}

impl App {
    /// The browser's open work that has blockers, as a tree, every fold
    /// open, siblings in browse order.
    pub(super) fn build_tree(&self) -> Tree {
        let order: Vec<i64> = self.all.iter().map(|r| r.task.id).collect();
        voro_core::blocks_tree(&order, &blocks_edges(&self.deps), |id| {
            self.all_index
                .get(&id)
                .is_some_and(|&i| !self.all[i].task.state.is_terminal())
        })
    }

    /// The tree's rows left showing by the closed folds.
    pub(super) fn tree_browser_rows(&self) -> Vec<BrowserRow> {
        let index = &self.all_index;
        let mut rows = Vec::new();
        let mut closed_at = None;
        for (k, node) in self.tree_rows.iter().enumerate() {
            if closed_at.is_some_and(|depth| node.depth > depth) {
                continue;
            }
            closed_at =
                (node.is_fold() && !self.open_folds.contains(&node.id)).then_some(node.depth);
            if let Some(&task) = index.get(&node.id) {
                rows.push(BrowserRow::Node { row: k, task });
            }
        }
        rows
    }

    /// `t` on the browser: the tree, folds closed, or the flat list again.
    /// The tree turns milestone grouping off.
    pub(super) fn toggle_browse_tree(&mut self) {
        self.browse_tree = !self.browse_tree;
        self.browse_by_milestone = false;
        self.open_folds.clear();
        self.rebuild_browser();
    }

    /// Space on a tree row with children: open its fold, or close it. On a
    /// reference: open the folds above the task's full copy and select it.
    pub(super) fn toggle_selected_fold(&mut self) {
        let Some(&BrowserRow::Node { row, task }) = self.browser_rows.get(self.tasks_sel) else {
            return;
        };
        let node = &self.tree_rows[row];
        if node.reference {
            self.follow_reference(node.id, task);
            return;
        }
        if !node.is_fold() {
            return;
        }
        let id = node.id;
        if !self.open_folds.remove(&id) {
            self.open_folds.insert(id);
        }
        self.rebuild_browser();
    }

    fn follow_reference(&mut self, id: i64, task: usize) {
        let Some(full) = self
            .tree_rows
            .iter()
            .position(|n| n.id == id && !n.reference)
        else {
            return;
        };
        let mut depth = self.tree_rows[full].depth;
        for node in self.tree_rows[..full].iter().rev() {
            if depth == 0 {
                break;
            }
            if node.depth < depth {
                depth = node.depth;
                self.open_folds.insert(node.id);
            }
        }
        self.rebuild_browser();
        let target = BrowserRow::Node { row: full, task };
        if let Some(sel) = self.browser_rows.iter().position(|r| *r == target) {
            self.tasks_sel = sel;
        }
    }

    /// A fold's `open` and `done` counts over the tasks it nests.
    pub fn fold_counts(&self, node: &TreeRow) -> (usize, usize) {
        let states: Vec<TaskState> = node
            .under
            .iter()
            .filter_map(|id| self.all_index.get(id))
            .map(|&i| self.all[i].task.state)
            .collect();
        (
            states.iter().filter(|s| !s.is_terminal()).count(),
            states.iter().filter(|s| **s == TaskState::Done).count(),
        )
    }
}

#[cfg(test)]
mod tests {
    use ratatui::crossterm::event::{KeyCode, KeyEvent};
    use voro_core::{Action, Store, TaskState};

    use super::super::milestones::tests::{app_from, new_task};
    use super::*;
    use crate::app::{Mode, Screen};

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn browser(mut app: App) -> App {
        app.screen = Screen::Tasks;
        app
    }

    /// The tree with every fold open.
    fn unfold(app: &mut App) {
        let folds: Vec<i64> = app
            .tree_rows
            .iter()
            .filter(|n| n.is_fold())
            .map(|n| n.id)
            .collect();
        app.open_folds.extend(folds);
        app.rebuild_browser();
    }

    /// The visible rows as (task, depth, reference).
    fn shape(app: &App) -> Vec<(i64, usize, bool)> {
        app.browser_rows
            .iter()
            .map(|row| match row {
                BrowserRow::Node { row, .. } => {
                    let n = &app.tree_rows[*row];
                    (n.id, n.depth, n.reference)
                }
                other => panic!("not a tree row: {other:?}"),
            })
            .collect()
    }

    /// `top` blocked by a chain of four, `a` → `b` → `c` → `d`, plus a
    /// redundant direct edge from `top` to `b`.
    fn chain() -> (App, [i64; 5]) {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let top = new_task(&mut store, p, "top", TaskState::Ready);
        let a = new_task(&mut store, p, "a", TaskState::Ready);
        let b = new_task(&mut store, p, "b", TaskState::Ready);
        let c = new_task(&mut store, p, "c", TaskState::Ready);
        let d = new_task(&mut store, p, "d", TaskState::Ready);
        for task in [c, d] {
            for action in [Action::Start, Action::Complete(None), Action::Accept] {
                store.apply(task, action).unwrap();
            }
        }
        store.block_tasks(a, &[top]).unwrap();
        store.block_tasks(b, &[a, top]).unwrap();
        store.block_tasks(c, &[b]).unwrap();
        store.block_tasks(d, &[c]).unwrap();
        (browser(app_from(store)), [top, a, b, c, d])
    }

    #[test]
    fn a_chain_with_a_redundant_edge_unfolds_to_each_task_once() {
        let (mut app, [top, a, b, c, d]) = chain();
        key(&mut app, KeyCode::Char('t'));
        assert!(app.browse_tree);
        unfold(&mut app);
        assert_eq!(
            shape(&app),
            vec![
                (top, 0, false),
                (a, 1, false),
                (b, 2, false),
                (c, 3, false),
                (d, 4, false)
            ]
        );
    }

    #[test]
    fn folds_start_closed_and_count_what_they_hide() {
        let (mut app, [top, a, ..]) = chain();
        key(&mut app, KeyCode::Char('t'));
        assert_eq!(shape(&app), vec![(top, 0, false)]);
        let node = app.tree_rows[0].clone();
        assert_eq!(app.fold_counts(&node), (2, 2));

        key(&mut app, KeyCode::Char(' '));
        assert_eq!(shape(&app), vec![(top, 0, false), (a, 1, false)]);
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(shape(&app), vec![(top, 0, false)]);
    }

    #[test]
    fn a_shared_blocker_prints_in_full_once_and_as_a_reference_once() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let x = new_task(&mut store, p, "x", TaskState::Ready);
        let y = new_task(&mut store, p, "y", TaskState::Ready);
        let shared = new_task(&mut store, p, "shared", TaskState::Ready);
        store.block_tasks(shared, &[x, y]).unwrap();
        let mut app = browser(app_from(store));
        key(&mut app, KeyCode::Char('t'));
        unfold(&mut app);
        assert_eq!(
            shape(&app),
            vec![
                (x, 0, false),
                (shared, 1, false),
                (y, 0, false),
                (shared, 1, true)
            ]
        );
    }

    /// The tree lays out the tasks it is given: an edge to a task outside
    /// the set adds no row and nests nothing.
    #[test]
    fn a_filtered_set_holds_only_the_rows_the_filter_passes() {
        let (app, [top, a, b, ..]) = chain();
        let open: Vec<i64> = app
            .all
            .iter()
            .filter(|r| !r.task.state.is_terminal())
            .map(|r| r.task.id)
            .collect();
        let rows = voro_core::blocks_tree(&open, &blocks_edges(&app.deps), |_| true).rows;
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![top, a, b]);
    }

    fn close(store: &mut Store, task: i64) {
        for action in [Action::Start, Action::Complete(None), Action::Accept] {
            store.apply(task, action).unwrap();
        }
    }

    /// Two open tasks joined by an edge, three open tasks with none, and a
    /// closed task blocked by another closed one.
    #[test]
    fn the_tree_shows_open_work_with_edges_and_counts_the_rest() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let top = new_task(&mut store, p, "top", TaskState::Ready);
        let under = new_task(&mut store, p, "under", TaskState::Ready);
        for title in ["lone 1", "lone 2", "lone 3"] {
            new_task(&mut store, p, title, TaskState::Ready);
        }
        let old = new_task(&mut store, p, "old", TaskState::Ready);
        let older = new_task(&mut store, p, "older", TaskState::Ready);
        store.block_tasks(under, &[top]).unwrap();
        store.block_tasks(older, &[old]).unwrap();
        close(&mut store, older);
        close(&mut store, old);
        let mut app = browser(app_from(store));
        key(&mut app, KeyCode::Char('t'));
        unfold(&mut app);
        assert_eq!(shape(&app), vec![(top, 0, false), (under, 1, false)]);
        assert_eq!((app.tree_no_edges, app.tree_closed), (3, 1));
    }

    /// Space on a reference whose full copy sits inside a closed fold opens
    /// the folds down to it and selects it.
    #[test]
    fn space_on_a_reference_opens_the_way_to_the_full_copy() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let x = new_task(&mut store, p, "x", TaskState::Ready);
        let mid = new_task(&mut store, p, "mid", TaskState::Ready);
        let y = new_task(&mut store, p, "y", TaskState::Ready);
        let shared = new_task(&mut store, p, "shared", TaskState::Ready);
        let leaf = new_task(&mut store, p, "leaf", TaskState::Ready);
        store.block_tasks(mid, &[x]).unwrap();
        store.block_tasks(shared, &[mid, y]).unwrap();
        store.block_tasks(leaf, &[shared]).unwrap();
        let mut app = browser(app_from(store));
        key(&mut app, KeyCode::Char('t'));
        assert_eq!(shape(&app), vec![(x, 0, false), (y, 0, false)]);
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(
            shape(&app),
            vec![(x, 0, false), (y, 0, false), (shared, 1, true)]
        );
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char(' '));
        assert_eq!(
            shape(&app),
            vec![
                (x, 0, false),
                (mid, 1, false),
                (shared, 2, false),
                (y, 0, false),
                (shared, 1, true)
            ]
        );
        assert_eq!(app.tasks_sel, 2);
        assert_eq!(app.selected_task_id(), Some(shared));
    }

    /// `voro tree` prints a done task's tree, though the browser's tree
    /// leaves closed trees out.
    #[test]
    fn the_cli_prints_a_done_tasks_tree() {
        let (mut app, [.., c, d]) = chain();
        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let out =
            crate::cli::run(&mut app.store, vec!["tree".into(), c.to_string()], &ctx).unwrap();
        assert_eq!(out, format!("#{c} done c\n  #{d} done d\n"));
    }

    #[test]
    fn t_and_m_exclude_each_other() {
        let (mut app, _) = chain();
        key(&mut app, KeyCode::Char('t'));
        key(&mut app, KeyCode::Char('M'));
        assert!(app.browse_by_milestone);
        assert!(!app.browse_tree);
        assert!(matches!(app.browser_rows[0], BrowserRow::Group(_)));
        key(&mut app, KeyCode::Char('t'));
        assert!(app.browse_tree);
        assert!(!app.browse_by_milestone);
        key(&mut app, KeyCode::Char('t'));
        assert_eq!(app.browser_rows.len(), app.all.len());
        assert!(matches!(app.browser_rows[0], BrowserRow::Task(_)));
    }

    #[test]
    fn task_keys_work_on_a_tree_row() {
        let (mut app, [top, a, ..]) = chain();
        key(&mut app, KeyCode::Char('t'));
        key(&mut app, KeyCode::Char(' '));
        key(&mut app, KeyCode::Char('j'));
        assert_eq!(app.selected_task_id(), Some(a));
        assert_eq!(app.enter_hint(), Some("⏎ view"));
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Detail { task_id, .. } if task_id == a));
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char('0'));
        assert_eq!(app.store.task(a).unwrap().priority, voro_core::Priority::P0);
        key(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected_task_id(), Some(top));
    }

    /// `voro tree` prints the rows the unfolded browser tree shows for the
    /// same task.
    #[test]
    fn the_cli_prints_the_browser_rows() {
        let (mut app, [top, a, ..]) = chain();
        let p = app.projects[0].id;
        let e = new_task(&mut app.store, p, "e", TaskState::Ready);
        let shared = new_task(&mut app.store, p, "shared", TaskState::Ready);
        app.store.block_tasks(e, &[top]).unwrap();
        app.store.block_tasks(shared, &[a, e]).unwrap();
        app.refresh().unwrap();
        key(&mut app, KeyCode::Char('t'));
        unfold(&mut app);

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let out =
            crate::cli::run(&mut app.store, vec!["tree".into(), top.to_string()], &ctx).unwrap();
        let printed: Vec<(i64, usize, bool)> = out
            .lines()
            .map(|line| {
                let body = line.trim_start();
                let depth = (line.len() - body.len()) / 2;
                let id = body
                    .trim_start_matches("↑ ")
                    .trim_start_matches('#')
                    .split(' ')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                (id, depth, body.starts_with('↑'))
            })
            .collect();
        assert_eq!(printed, shape(&app));
        assert!(printed.iter().any(|r| r.2), "the fixture has a reference");
    }
}
