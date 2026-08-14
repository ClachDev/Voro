use std::path::PathBuf;

use crate::model::TaskState;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error(
        "this database is at schema version {version}, but this build of voro knows only {known} \
         migration(s) — a newer build migrated it, and this one cannot read it safely. {remedy}"
    )]
    SchemaAhead {
        version: usize,
        known: usize,
        remedy: String,
    },

    #[error(
        "{} is protected and has {pending} pending migration(s) (schema {version} -> {known}); \
         the operator's store never migrates as a side effect of being opened. The operator \
         applies them by launching `voro` at a terminal, which asks first, or with \
         `voro migrate`. If you are an agent or a script seeing this, stop and tell the \
         operator; `voro migrate --yes` consents on their behalf and is recorded in the \
         migration journal. A snapshot is written to backups/ beside the store before anything \
         is applied.",
        path.display()
    )]
    MigrationsPending {
        path: PathBuf,
        pending: usize,
        version: usize,
        known: usize,
    },

    #[error(
        "this database's migration {idx} differs from the one this build carries: it was applied \
         {applied_at} by {applied_by}. The version numbers agree, so nothing else will catch \
         this — and since the two migrations are not the same text, this build cannot assume the \
         schema it is about to query. {remedy}"
    )]
    SchemaDiverged {
        idx: usize,
        applied_at: String,
        applied_by: String,
        remedy: String,
    },

    #[error("task {0} not found")]
    TaskNotFound(i64),

    #[error("project {0} not found")]
    ProjectNotFound(i64),

    #[error(
        "project {id} has {count} task(s) and cannot be deleted — set its weight to 0 to park \
         it, or `voro project archive` to retire it; both keep its history"
    )]
    ProjectHasTasks { id: i64, count: i64 },

    #[error("project '{name}' is archived — `voro project unarchive {name}` first")]
    ProjectArchived { name: String },

    #[error("no repo named '{name}' in project '{project}'; its repos: {known}")]
    RepoNotFound {
        project: String,
        name: String,
        known: String,
    },

    #[error("repo {0} not found")]
    RepoIdNotFound(i64),

    #[error("document {0} not found — `voro doc list` shows the registered ones")]
    DocNotFound(i64),

    #[error(
        "repo '{name}' is the only repo in project '{project}' — a project always has a \
         checkout, so add another repo before removing this one"
    )]
    LastRepo { project: String, name: String },

    #[error(
        "repo '{name}' is the default repo of project '{project}' — `voro repo default \
         {project} <other>` first, so tasks without a repo still resolve"
    )]
    DefaultRepo { project: String, name: String },

    #[error(
        "repo '{name}' is used by {count} task(s) — re-point them with `voro set <task-id> \
         --repo <other>` before removing it"
    )]
    RepoInUse { name: String, count: i64 },

    #[error("session {0} not found")]
    SessionNotFound(i64),

    #[error("cannot {action} a task in state '{from}'")]
    InvalidTransition { from: TaskState, action: String },

    #[error("task {id} is human-only: {reason}")]
    HumanTask { id: i64, reason: String },

    #[error("blocks dependency would create a cycle: {0}")]
    DependencyCycle(String),

    #[error(
        "no default agent: none of the built-in agents ({probed}) were found on PATH; \
         install one, or set `default_agent` in {} to a configured agent",
        path.display()
    )]
    NoDefaultAgent { probed: String, path: PathBuf },

    #[error("invalid agents config at {}: {message}", path.display())]
    AgentConfigInvalid { path: PathBuf, message: String },

    #[error(
        "no viewer set up — run `voro viewer add <name> '<cmd>'`, e.g. \
         `voro viewer add zed 'zed {{path}}'`; none of the built-in {probed} are on PATH"
    )]
    NoViewer { probed: String },

    #[error(
        "no viewer named '{name}' — run `voro viewer add {name} '<cmd>'` to define it in {}; \
         available viewers: {known}",
        path.display()
    )]
    UnknownViewer {
        name: String,
        known: String,
        path: PathBuf,
    },

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
