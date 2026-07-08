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

    #[error("session {0} not found")]
    SessionNotFound(i64),

    #[error("cannot {action} a task in state '{from}'")]
    InvalidTransition { from: TaskState, action: String },

    #[error("{0}")]
    Invalid(String),
}
