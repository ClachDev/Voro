//! The row types the screens select over, each an index into `App`'s loaded
//! data.

use voro_core::{DepRef, Task};

use super::DefaultKind;

/// An agent row on the Config screen (DESIGN.md §5): the effective set with
/// provenance and the default marked, read-only in this cut.
#[derive(Debug, Clone)]
pub struct ConfigAgentRow {
    pub name: String,
    pub dispatch: String,
    pub provenance: &'static str,
    pub is_default: bool,
    pub verbs: Vec<&'static str>,
    pub missing_verbs: Vec<&'static str>,
    /// What `{model}` resolves to for this agent as `(ordinary, deep, plan)`,
    /// with the fallbacks already applied; `None` when it names no model.
    pub models: Option<(String, String, String)>,
}

/// A named viewer row on the Config screen: every viewer `open` can run, the
/// built-ins included, so the starred default is always visible. Only the rows
/// backed by a `voro.toml` table are `editable` — a built-in is overridden
/// rather than changed in place (DESIGN.md §11a).
#[derive(Debug, Clone)]
pub struct ConfigViewerRow {
    pub name: String,
    pub cmd: String,
    pub is_default: bool,
    pub provenance: &'static str,
    pub editable: bool,
}

/// Which `voro.toml` value a settings row edits (DESIGN.md §5). The two
/// defaults carry the [`DefaultKind`] their shared picker is keyed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Default(DefaultKind),
    MaxRunning,
}

/// One setting row on the Config screen (DESIGN.md §5): a `voro.toml` value the
/// operator selects and edits with one key, showing what is in force and where
/// it came from — the file, or the rule Voro fell back to. Derived once per
/// refresh, like every other row on the screen.
#[derive(Debug, Clone)]
pub struct ConfigSettingRow {
    pub name: &'static str,
    pub value: String,
    /// Where `value` came from, parenthesised on screen: `voro.toml` when the
    /// key is set, else the resolution rule that produced it.
    pub source: String,
    pub kind: SettingKind,
}

/// One selectable row on the Config screen: the settings list and the viewers
/// list share a single selection, so both index into one flat row list the way
/// the cockpit's [`CockpitRow`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigRow {
    Setting(usize),
    Viewer(usize),
}

/// One selectable row on the cockpit; indices point into the App caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitRow {
    /// One row of the scheduler's queue, by index.
    Queue(usize),
    /// A proposal inside an expanded digest row (DESIGN.md §7): the digest's
    /// queue index, then the proposal's index within it.
    Proposal(usize, usize),
    Running(usize),
}

/// One row of the task browser. Ungrouped, every row is a task; grouped by
/// milestone (DESIGN.md §9), each milestone heads a fold of its members and a
/// final `None` fold holds the unattached tasks. In the tree arrangement every
/// row is a [`BrowserRow::Node`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRow {
    Group(Option<i64>),
    /// An index into `App::all`.
    Task(usize),
    /// An index into `App::tree_rows`, and the row's task as an index into
    /// `App::all`.
    Node {
        row: usize,
        task: usize,
    },
}

impl BrowserRow {
    /// The row's task as an index into `App::all`; `None` on a fold header.
    pub fn task_index(&self) -> Option<usize> {
        match self {
            BrowserRow::Task(i) | BrowserRow::Node { task: i, .. } => Some(*i),
            BrowserRow::Group(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: Task,
    pub project: String,
    pub weight: i64,
    /// The task's `blocks` dependencies with each blocker's state, so the
    /// browser can show a parked row what it is waiting on. Filtered from the
    /// same [`Store::deps_by_task`] load that feeds the detail panes.
    pub blockers: Vec<DepRef>,
}
