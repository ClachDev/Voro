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
    Done,
    Rejected,
}

impl TaskState {
    pub const ALL: [TaskState; 8] = [
        TaskState::Proposed,
        TaskState::Parked,
        TaskState::Ready,
        TaskState::Running,
        TaskState::NeedsInput,
        TaskState::Review,
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
        f.write_str(self.as_str())
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
        write!(f, "P{}", self.as_int())
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
    pub state_since: String,
    pub created_at: String,
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub task_id: i64,
    pub depends_on: i64,
    pub kind: DepKind,
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
    pub log_path: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub outcome: Option<SessionOutcome>,
}
