use std::fmt;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskState {
    Proposed,
    Parked,
    Ready,
    Running,
    NeedsInput,
    Review,
    Stalled,
    Done,
    Rejected,
}

impl TaskState {
    pub const ALL: [TaskState; 9] = [
        TaskState::Proposed,
        TaskState::Parked,
        TaskState::Ready,
        TaskState::Running,
        TaskState::NeedsInput,
        TaskState::Review,
        TaskState::Stalled,
        TaskState::Done,
        TaskState::Rejected,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Proposed => "proposed",
            TaskState::Parked => "parked",
            TaskState::Ready => "ready",
            TaskState::Running => "running",
            TaskState::NeedsInput => "needs-input",
            TaskState::Review => "review",
            TaskState::Stalled => "stalled",
            TaskState::Done => "done",
            TaskState::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Result<TaskState> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == s)
            .ok_or_else(|| Error::Invalid(format!("unknown task state '{s}'")))
    }

    /// Closed states: nothing leaves them, and they do not block dependants.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Rejected)
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl FromSql for TaskState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        TaskState::parse(s).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for TaskState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    pub fn from_int(n: i64) -> Result<Priority> {
        match n {
            0 => Ok(Priority::P0),
            1 => Ok(Priority::P1),
            2 => Ok(Priority::P2),
            3 => Ok(Priority::P3),
            _ => Err(Error::Invalid(format!("priority {n} out of range 0-3"))),
        }
    }

    pub fn as_int(self) -> i64 {
        match self {
            Priority::P0 => 0,
            Priority::P1 => 1,
            Priority::P2 => 2,
            Priority::P3 => 3,
        }
    }

    /// The geometric value used by the attention score (DESIGN.md §7).
    pub fn value(self) -> f64 {
        match self {
            Priority::P0 => 8.0,
            Priority::P1 => 4.0,
            Priority::P2 => 2.0,
            Priority::P3 => 1.0,
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Priority::P0 => "P0",
            Priority::P1 => "P1",
            Priority::P2 => "P2",
            Priority::P3 => "P3",
        };
        f.pad(s)
    }
}

impl FromSql for Priority {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Priority::from_int(value.as_i64()?).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for Priority {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_int().into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DepKind {
    Blocks,
    DiscoveredFrom,
    Parent,
    Related,
}

impl DepKind {
    pub const ALL: [DepKind; 4] = [
        DepKind::Blocks,
        DepKind::DiscoveredFrom,
        DepKind::Parent,
        DepKind::Related,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            DepKind::Blocks => "blocks",
            DepKind::DiscoveredFrom => "discovered-from",
            DepKind::Parent => "parent",
            DepKind::Related => "related",
        }
    }

    pub fn parse(s: &str) -> Result<DepKind> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| Error::Invalid(format!("unknown dep kind '{s}'")))
    }
}

impl fmt::Display for DepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromSql for DepKind {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        DepKind::parse(value.as_str()?).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for DepKind {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionOutcome {
    Completed,
    Asked,
    Failed,
    Capped,
    Aborted,
}

impl SessionOutcome {
    pub const ALL: [SessionOutcome; 5] = [
        SessionOutcome::Completed,
        SessionOutcome::Asked,
        SessionOutcome::Failed,
        SessionOutcome::Capped,
        SessionOutcome::Aborted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SessionOutcome::Completed => "completed",
            SessionOutcome::Asked => "asked",
            SessionOutcome::Failed => "failed",
            SessionOutcome::Capped => "capped",
            SessionOutcome::Aborted => "aborted",
        }
    }

    pub fn parse(s: &str) -> Result<SessionOutcome> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == s)
            .ok_or_else(|| Error::Invalid(format!("unknown session outcome '{s}'")))
    }
}

impl fmt::Display for SessionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromSql for SessionOutcome {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        SessionOutcome::parse(value.as_str()?).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl ToSql for SessionOutcome {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.as_str().into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub weight: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: i64,
    pub project_id: i64,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub state: TaskState,
    pub agent: Option<String>,
    pub question: Option<String>,
    /// The canonical URL of a GitHub PR tracked on this task (DESIGN.md §11c),
    /// or `None`. Naming the PR's base repo, it survives forks where the
    /// checkout's `origin` is not that repo.
    pub pr_url: Option<String>,
    /// The git branch this task's work lives on (task #81), or `None`. Holds
    /// the *intended* name dispatch injects into the agent's prompt, later
    /// overwritten by the branch the agent *reports*. Voro never runs git; it
    /// only passes this name into the prompt and records what comes back.
    pub branch: Option<String>,
    pub state_since: String,
    pub created_at: String,
    pub closed_at: Option<String>,
    /// Marks a task no agent can execute at all — hands-on work at real
    /// hardware, say (DESIGN.md §3/§6). Dispatch, continuation, `ask`, and the
    /// agent override refuse it; completion goes `running → done` directly,
    /// since the human is both executor and acceptor. The default (`false`)
    /// means dispatchable, with the human still free to start it by hand.
    pub human: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub task_id: i64,
    pub depends_on: i64,
    pub kind: DepKind,
}

/// A `blocks` dependency resolved to the blocker's current state, so callers
/// can tell an open blocker from one that has already closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blocker {
    pub id: i64,
    pub state: TaskState,
}

impl Blocker {
    /// A blocker still holding its dependant back: not yet in a closed state.
    pub fn is_open(self) -> bool {
        !self.state.is_terminal()
    }
}

/// A dependency edge resolved for display: the task at the *other* end of the
/// edge with its current title and state, plus the edge's kind. Which end is
/// "other" depends on the query — the dependency for
/// [`Store::deps_by_task`](crate::Store::deps_by_task), the dependant for
/// [`Store::dependents_by_task`](crate::Store::dependents_by_task).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepRef {
    pub id: i64,
    pub title: String,
    pub state: TaskState,
    pub kind: DepKind,
}

impl DepRef {
    /// The referenced task is not yet in a closed state.
    pub fn is_open(&self) -> bool {
        !self.state.is_terminal()
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub id: i64,
    pub task_id: Option<i64>,
    pub at: String,
    pub kind: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: i64,
    pub task_id: i64,
    pub agent: String,
    pub pid: Option<i64>,
    /// The agent's own reference for this session (a Claude session UUID, a
    /// Codex session id, a tmux session name), captured after launch and
    /// substituted into the agent's attach/resume/continue verb templates.
    /// `None` when the agent has no capture story or capture failed.
    pub session_ref: Option<String>,
    pub log_path: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<SessionOutcome>,
}

/// A row of the cockpit's running strip (DESIGN.md §9): one per `running` task,
/// joined with its open session if it has one. The strip filters on task state,
/// so `review`/`needs-input` tasks — whose session stays open behind the scenes
/// (§8) — never appear; they belong to the queue, as does a `stalled` task
/// whose dispatch died. A task with no open session (started by hand, so
/// nothing was ever dispatched) still shows, with `session_id`/`agent` set
/// to `None`. `elapsed_secs` is computed in SQL against the database's clock —
/// a live session's age, or how long a session-less task has sat in `running` —
/// so the TUI only has to format it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunningRow {
    pub session_id: Option<i64>,
    pub task_id: i64,
    pub task_title: String,
    pub task_state: TaskState,
    pub agent: Option<String>,
    pub started_at: String,
    pub elapsed_secs: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_state_display_honors_width() {
        assert_eq!(format!("{:11}", TaskState::Ready), "ready      ");
        assert_eq!(format!("{:>6}", TaskState::Done), "  done");
        assert_eq!(format!("{:>6}", TaskState::NeedsInput), "needs-input");
    }

    #[test]
    fn priority_display_honors_width() {
        assert_eq!(format!("{:>6}", Priority::P0), "    P0");
        assert_eq!(format!("{:>6}", Priority::P2), "    P2");
    }
}
