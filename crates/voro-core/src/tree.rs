//! The blocker tree (DESIGN.md §9): tasks laid out by their `blocks` edges,
//! each task's blockers nested beneath it. The task browser draws it on `t`
//! and `voro tree` prints one task's part of it, from the same rows.

use std::collections::{HashMap, HashSet};

/// One row of a blocker tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub id: i64,
    /// Nesting level: 0 on a top-level row.
    pub depth: usize,
    /// The task already printed in full higher up, under another dependent;
    /// this row points at it and nests nothing.
    pub reference: bool,
    /// The task sits on a `blocks` cycle. The store refuses cycles, so this
    /// marks bad data.
    pub cycle: bool,
    /// Every distinct task nested beneath this row at any depth, in the
    /// order given to the builder. Empty on a leaf and on a reference.
    pub under: Vec<i64>,
}

impl TreeRow {
    pub fn is_fold(&self) -> bool {
        !self.under.is_empty()
    }
}

/// The browser's tree and the top-level rows it leaves out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    pub rows: Vec<TreeRow>,
    /// Tasks with no `blocks` edge in either direction.
    pub no_edges: usize,
    /// Closed tasks that head a tree: blocked by something, blocking nothing.
    pub closed_trees: usize,
}

/// The tree over `order`, the tasks in the order siblings print in. A
/// top-level row is a task that blocks nothing in `order`, is open, and has
/// at least one blocker; the other tasks that block nothing are counted, not
/// shown. Tasks on a cycle that no top-level row reaches print as top-level
/// rows of their own. `blockers` maps a task to the tasks that block it; an
/// edge to a task outside `order` is ignored.
pub fn blocks_tree(
    order: &[i64],
    blockers: &HashMap<i64, Vec<i64>>,
    is_open: impl Fn(i64) -> bool,
) -> Tree {
    let graph = Graph::new(order, blockers);
    let blocking: HashSet<usize> = graph.children.iter().flatten().copied().collect();
    let mut tree = Tree::default();
    let mut printed = HashSet::new();
    for i in (0..order.len()).filter(|i| !blocking.contains(i)) {
        if graph.children[i].is_empty() {
            tree.no_edges += 1;
        } else if !is_open(order[i]) {
            tree.closed_trees += 1;
        } else {
            graph.visit(i, 0, &mut printed, &mut tree.rows);
        }
    }
    for i in 0..order.len() {
        if graph.cycle[i] && !printed.contains(&i) {
            graph.visit(i, 0, &mut printed, &mut tree.rows);
        }
    }
    tree
}

/// One task's blockers as [`blocks_tree`] lays them out, the task itself the
/// single top-level row, whatever its state. Empty when `root` is not in
/// `order`.
pub fn blocker_tree(root: i64, order: &[i64], blockers: &HashMap<i64, Vec<i64>>) -> Vec<TreeRow> {
    let graph = Graph::new(order, blockers);
    let Some(&i) = graph.index.get(&root) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    graph.visit(i, 0, &mut HashSet::new(), &mut rows);
    rows
}

/// The `blocks` graph over `order` by index, reduced: an edge to a blocker
/// that another of the task's blockers already reaches is dropped, so each
/// task nests only its nearest blockers.
struct Graph<'a> {
    order: &'a [i64],
    index: HashMap<i64, usize>,
    /// Each task's nearest blockers, in `order`.
    children: Vec<Vec<usize>>,
    /// Each task's transitive blockers, in `order`.
    reach: Vec<Vec<usize>>,
    /// Whether each task sits on a cycle.
    cycle: Vec<bool>,
}

impl<'a> Graph<'a> {
    fn new(order: &'a [i64], blockers: &HashMap<i64, Vec<i64>>) -> Self {
        let index: HashMap<i64, usize> = order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        let direct: Vec<Vec<usize>> = order
            .iter()
            .map(|id| {
                let mut out: Vec<usize> = blockers
                    .get(id)
                    .map_or(&[][..], |b| b)
                    .iter()
                    .filter_map(|b| index.get(b).copied())
                    .filter(|b| order[*b] != *id)
                    .collect();
                out.sort_unstable();
                out.dedup();
                out
            })
            .collect();

        let mut reach: Vec<Option<HashSet<usize>>> = vec![None; order.len()];
        let mut on_path = vec![false; order.len()];
        for i in 0..order.len() {
            closure(i, &direct, &mut reach, &mut on_path);
        }
        let reach: Vec<HashSet<usize>> = reach.into_iter().map(Option::unwrap_or_default).collect();

        let cycle = on_cycle(&direct);
        let children = direct
            .iter()
            .map(|kids| {
                kids.iter()
                    .copied()
                    .filter(|&c| !kids.iter().any(|&o| o != c && reach[o].contains(&c)))
                    .collect()
            })
            .collect();
        let reach = reach
            .into_iter()
            .map(|set| {
                let mut v: Vec<usize> = set.into_iter().collect();
                v.sort_unstable();
                v
            })
            .collect();
        Self {
            order,
            index,
            children,
            reach,
            cycle,
        }
    }

    /// Depth-first from `i`: a task prints in full the first time it is
    /// reached and as a reference every time after.
    fn visit(&self, i: usize, depth: usize, printed: &mut HashSet<usize>, rows: &mut Vec<TreeRow>) {
        let id = self.order[i];
        if !printed.insert(i) {
            rows.push(TreeRow {
                id,
                depth,
                reference: true,
                cycle: self.cycle[i],
                under: Vec::new(),
            });
            return;
        }
        rows.push(TreeRow {
            id,
            depth,
            reference: false,
            cycle: self.cycle[i],
            under: self.reach[i].iter().map(|&j| self.order[j]).collect(),
        });
        for &child in &self.children[i] {
            self.visit(child, depth + 1, printed, rows);
        }
    }
}

/// Task `i`'s transitive blockers, memoised in `reach`. The store refuses a
/// `blocks` cycle; should one exist, the edge that closes it is skipped.
fn closure(
    i: usize,
    direct: &[Vec<usize>],
    reach: &mut Vec<Option<HashSet<usize>>>,
    on_path: &mut Vec<bool>,
) {
    if reach[i].is_some() || on_path[i] {
        return;
    }
    on_path[i] = true;
    let mut set = HashSet::new();
    for &c in &direct[i] {
        if on_path[c] {
            continue;
        }
        closure(c, direct, reach, on_path);
        set.insert(c);
        set.extend(reach[c].iter().flatten().copied());
    }
    set.remove(&i);
    on_path[i] = false;
    reach[i] = Some(set);
}

/// Which tasks sit on a cycle of `direct`: those in a strongly connected
/// component of more than one task, found by Tarjan's algorithm. `direct`
/// holds no self-edges.
fn on_cycle(direct: &[Vec<usize>]) -> Vec<bool> {
    struct Tarjan<'g> {
        direct: &'g [Vec<usize>],
        next: usize,
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        stack: Vec<usize>,
        on_stack: Vec<bool>,
        cycle: Vec<bool>,
    }

    impl Tarjan<'_> {
        fn connect(&mut self, v: usize) {
            self.index[v] = Some(self.next);
            self.low[v] = self.next;
            self.next += 1;
            self.stack.push(v);
            self.on_stack[v] = true;
            for &w in &self.direct[v] {
                match self.index[w] {
                    None => {
                        self.connect(w);
                        self.low[v] = self.low[v].min(self.low[w]);
                    }
                    Some(iw) if self.on_stack[w] => self.low[v] = self.low[v].min(iw),
                    Some(_) => {}
                }
            }
            if Some(self.low[v]) != self.index[v] {
                return;
            }
            let start = self.stack.iter().rposition(|&w| w == v).unwrap();
            let component = self.stack.split_off(start);
            for &w in &component {
                self.on_stack[w] = false;
                self.cycle[w] = component.len() > 1;
            }
        }
    }

    let n = direct.len();
    let mut t = Tarjan {
        direct,
        next: 0,
        index: vec![None; n],
        low: vec![0; n],
        stack: Vec::new(),
        on_stack: vec![false; n],
        cycle: vec![false; n],
    };
    for v in 0..n {
        if t.index[v].is_none() {
            t.connect(v);
        }
    }
    t.cycle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges(pairs: &[(i64, i64)]) -> HashMap<i64, Vec<i64>> {
        let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
        for &(task, blocker) in pairs {
            map.entry(task).or_default().push(blocker);
        }
        map
    }

    fn shape(rows: &[TreeRow]) -> Vec<(i64, usize, bool)> {
        rows.iter().map(|r| (r.id, r.depth, r.reference)).collect()
    }

    fn open_tree(order: &[i64], blockers: &HashMap<i64, Vec<i64>>) -> Vec<TreeRow> {
        blocks_tree(order, blockers, |_| true).rows
    }

    /// A task blocked by a chain of four, plus a redundant direct edge to the
    /// second: five rows, each task once, and the direct edge adds none.
    #[test]
    fn a_redundant_edge_adds_no_row() {
        let blockers = edges(&[(1, 2), (2, 3), (3, 4), (4, 5), (1, 3)]);
        let rows = open_tree(&[1, 2, 3, 4, 5], &blockers);
        assert_eq!(
            shape(&rows),
            vec![
                (1, 0, false),
                (2, 1, false),
                (3, 2, false),
                (4, 3, false),
                (5, 4, false)
            ]
        );
        assert_eq!(rows[0].under, vec![2, 3, 4, 5]);
        assert!(rows[4].under.is_empty());
    }

    /// One task blocking two others prints in full under the first and as a
    /// reference under the second.
    #[test]
    fn a_shared_blocker_prints_once_in_full() {
        let blockers = edges(&[(1, 3), (2, 3), (3, 4)]);
        let rows = open_tree(&[1, 2, 3, 4], &blockers);
        assert_eq!(
            shape(&rows),
            vec![
                (1, 0, false),
                (3, 1, false),
                (4, 2, false),
                (2, 0, false),
                (3, 1, true)
            ]
        );
        assert!(!rows[4].is_fold());
        assert_eq!(rows[3].under, vec![3, 4]);
    }

    /// Edges to tasks outside the set are dropped, so two tasks joined only
    /// through a third left out have no edges between them.
    #[test]
    fn the_tree_holds_only_the_given_tasks() {
        let blockers = edges(&[(1, 2), (2, 3)]);
        let tree = blocks_tree(&[1, 3], &blockers, |_| true);
        assert!(tree.rows.is_empty());
        assert_eq!(tree.no_edges, 2);
    }

    /// Two open tasks joined by an edge, three with none, and a closed task
    /// blocked by another closed one: one top-level row, the rest counted.
    #[test]
    fn only_open_tasks_with_blockers_head_the_tree() {
        let blockers = edges(&[(1, 2), (6, 7)]);
        let closed = [6, 7];
        let tree = blocks_tree(&[1, 2, 3, 4, 5, 6, 7], &blockers, |id| {
            !closed.contains(&id)
        });
        assert_eq!(shape(&tree.rows), vec![(1, 0, false), (2, 1, false)]);
        assert_eq!((tree.no_edges, tree.closed_trees), (3, 1));
    }

    /// A closed blocker stays in an open task's tree.
    #[test]
    fn a_closed_blocker_nests_under_an_open_task() {
        let blockers = edges(&[(1, 2)]);
        let tree = blocks_tree(&[1, 2], &blockers, |id| id == 1);
        assert_eq!(shape(&tree.rows), vec![(1, 0, false), (2, 1, false)]);
    }

    /// A cycle nothing outside it depends on still prints, every task on it
    /// marked.
    #[test]
    fn a_cycle_with_no_dependent_prints_marked() {
        let blockers = edges(&[(1, 2), (2, 1), (3, 4)]);
        let tree = blocks_tree(&[1, 2, 3, 4], &blockers, |_| true);
        assert_eq!(
            shape(&tree.rows),
            vec![
                (3, 0, false),
                (4, 1, false),
                (1, 0, false),
                (2, 1, false),
                (1, 2, true)
            ]
        );
        let marked: Vec<bool> = tree.rows.iter().map(|r| r.cycle).collect();
        assert_eq!(marked, vec![false, false, true, true, true]);
    }

    /// `blocker_tree` is the root's part of `blocks_tree`, row for row.
    #[test]
    fn one_tasks_tree_matches_its_part_of_the_whole() {
        let blockers = edges(&[(1, 2), (1, 3), (2, 4), (3, 4), (5, 6)]);
        let order = [1, 2, 3, 4, 5, 6];
        let whole = open_tree(&order, &blockers);
        let one = blocker_tree(1, &order, &blockers);
        assert_eq!(one, whole[..one.len()]);
        assert_eq!(
            shape(&one),
            vec![
                (1, 0, false),
                (2, 1, false),
                (4, 2, false),
                (3, 1, false),
                (4, 2, true)
            ]
        );
        assert!(blocker_tree(9, &order, &blockers).is_empty());
    }

    /// The store refuses a `blocks` cycle; the builder still terminates on one.
    #[test]
    fn a_cycle_does_not_loop() {
        let blockers = edges(&[(1, 2), (2, 1)]);
        let rows = blocker_tree(1, &[1, 2], &blockers);
        assert_eq!(
            shape(&rows),
            vec![(1, 0, false), (2, 1, false), (1, 2, true)]
        );
    }
}
