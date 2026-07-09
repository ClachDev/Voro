use std::path::PathBuf;

use crate::model::TaskState;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("task {0} not found")]
    TaskNotFound(i64),

    #[error("project {0} not found")]
    ProjectNotFound(i64),

    #[error(
        "project {id} has {count} task(s) and cannot be deleted — set its weight to 0 to park \
         it instead, which snoozes it without losing history"
    )]
    ProjectHasTasks { id: i64, count: i64 },

    #[error("session {0} not found")]
    SessionNotFound(i64),

    #[error("cannot {action} a task in state '{from}'")]
    InvalidTransition { from: TaskState, action: String },

    #[error("blocks dependency would create a cycle: {0}")]
    DependencyCycle(String),

    #[error(
        "agents config not found at {}; expected a TOML file with `default = \"<name>\"` \
         and an [agents.<name>] table per agent",
        .0.display()
    )]
    AgentConfigMissing(PathBuf),

    #[error("invalid agents config at {}: {message}", path.display())]
    AgentConfigInvalid { path: PathBuf, message: String },

    #[error("no agent named '{name}' ({origin}) in {}; defined agents: {known}", path.display())]
    UnknownAgent {
        name: String,
        origin: &'static str,
        path: PathBuf,
        known: String,
    },

    #[error("{0}")]
    Invalid(String),
}
