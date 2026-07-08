use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::model::{
    Dep, DepKind, Event, Priority, Project, Session, SessionOutcome, Task, TaskState,
};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_rename_backlog_to_parked.sql"),
];

/// Owns the SQLite database. All writes go through this type; task state in
/// particular is only ever changed by the transition API in `transition.rs`.
pub struct Store {
    pub(crate) conn: Connection,
}

/// Initial state for a task created by a human. `proposed` is quick capture;
/// `parked`/`ready` mean the creator has already triaged their own task.
#[derive(Debug, Clone)]
pub struct NewTask {
    pub project_id: i64,
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub state: TaskState,
    pub agent: Option<String>,
}

/// Content edits. State is deliberately absent — use `Store::apply`.
#[derive(Debug, Clone)]
pub struct TaskEdit {
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub agent: Option<String>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| Error::Invalid(format!("cannot create {}: {e}", dir.display())))?;
        }
        Store::from_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::from_connection(Connection::open_in_memory()?)
    }

    /// `$XDG_DATA_HOME/voro/voro.db`, defaulting to `~/.local/share`.
    pub fn default_db_path() -> PathBuf {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join(".local/share")
            });
        data_home.join("voro/voro.db")
    }

    fn from_connection(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    /// SQLite's `PRAGMA data_version`, which increments whenever another
    /// connection commits a change to the database. The value is stable across
    /// commits made on this connection, so a caller can poll it to detect
    /// external writes without reacting to its own mutations.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    /// Migrations may rebuild tables (SQLite cannot alter CHECK constraints),
    /// so foreign-key enforcement is suspended for the duration and integrity
    /// verified afterwards — the procedure SQLite documents for schema changes.
    fn migrate(&mut self) -> Result<()> {
        self.conn.pragma_update(None, "foreign_keys", false)?;
        let applied = self.apply_migrations();
        let restored = self.conn.pragma_update(None, "foreign_keys", true);
        applied?;
        restored?;
        let violations: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                    r.get(0)
                })?;
        if violations > 0 {
            return Err(Error::Invalid(format!(
                "{violations} foreign key violation(s) after migration"
            )));
        }
        Ok(())
    }

    fn apply_migrations(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        let version: usize =
            tx.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))? as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version) {
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        tx.commit()?;
        Ok(())
    }

    // --- projects ---

    pub fn create_project(&mut self, name: &str, path: &str) -> Result<Project> {
        self.conn.execute(
            "INSERT INTO projects (name, path) VALUES (?1, ?2)",
            params![name, path],
        )?;
        self.project(self.conn.last_insert_rowid())
    }

    pub fn project(&self, id: i64) -> Result<Project> {
        self.conn
            .query_row(
                "SELECT id, name, path, weight FROM projects WHERE id = ?1",
                [id],
                project_from_row,
            )
            .optional()?
            .ok_or(Error::ProjectNotFound(id))
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, weight FROM projects ORDER BY name")?;
        let rows = stmt.query_map([], project_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_weight(&mut self, project_id: i64, weight: i64) -> Result<()> {
        if !(0..=5).contains(&weight) {
            return Err(Error::Invalid(format!("weight {weight} out of range 0-5")));
        }
        let changed = self.conn.execute(
            "UPDATE projects SET weight = ?1 WHERE id = ?2",
            params![weight, project_id],
        )?;
        if changed == 0 {
            return Err(Error::ProjectNotFound(project_id));
        }
        Ok(())
    }

    // --- tasks ---

    pub fn create_task(&mut self, new: NewTask) -> Result<Task> {
        if !matches!(
            new.state,
            TaskState::Proposed | TaskState::Parked | TaskState::Ready
        ) {
            return Err(Error::Invalid(format!(
                "a task cannot be created in state '{}'",
                new.state
            )));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO tasks (project_id, title, body, priority, state, agent,
                                state_since, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'), datetime('now'))",
            params![
                new.project_id,
                new.title,
                new.body,
                new.priority,
                new.state,
                new.agent
            ],
        )?;
        let id = tx.last_insert_rowid();
        log_event(&tx, id, "created", Some(new.state.as_str()))?;
        tx.commit()?;
        self.task(id)
    }

    pub fn task(&self, id: i64) -> Result<Task> {
        get_task(&self.conn, id)?.ok_or(Error::TaskNotFound(id))
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {TASK_COLUMNS} FROM tasks ORDER BY id"))?;
        let rows = stmt.query_map([], task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn update_task(&mut self, id: i64, edit: TaskEdit) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET title = ?1, body = ?2, priority = ?3, agent = ?4 WHERE id = ?5",
            params![edit.title, edit.body, edit.priority, edit.agent, id],
        )?;
        if changed == 0 {
            return Err(Error::TaskNotFound(id));
        }
        self.task(id)
    }

    // --- deps ---

    pub fn add_dep(&mut self, task_id: i64, depends_on: i64, kind: DepKind) -> Result<()> {
        if task_id == depends_on {
            return Err(Error::Invalid("a task cannot depend on itself".into()));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO deps (task_id, depends_on, kind) VALUES (?1, ?2, ?3)",
            params![task_id, depends_on, kind],
        )?;
        if kind == DepKind::Blocks {
            crate::transition::reconcile_readiness(&tx, task_id)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_dep(&mut self, task_id: i64, depends_on: i64) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM deps WHERE task_id = ?1 AND depends_on = ?2",
            params![task_id, depends_on],
        )?;
        crate::transition::reconcile_readiness(&tx, task_id)?;
        tx.commit()?;
        Ok(())
    }

    pub fn deps_of(&self, task_id: i64) -> Result<Vec<Dep>> {
        let mut stmt = self.conn.prepare(
            "SELECT task_id, depends_on, kind FROM deps WHERE task_id = ?1 ORDER BY depends_on",
        )?;
        let rows = stmt.query_map([task_id], |row| {
            Ok(Dep {
                task_id: row.get(0)?,
                depends_on: row.get(1)?,
                kind: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- sessions ---

    /// Open a session for a running task, stamping `started_at`. `ended_at` and
    /// `outcome` stay NULL until [`end_session`](Store::end_session).
    pub fn create_session(
        &mut self,
        task_id: i64,
        agent: &str,
        pid: Option<i64>,
        log_path: Option<&str>,
    ) -> Result<Session> {
        self.conn.execute(
            "INSERT INTO sessions (task_id, agent, pid, log_path, started_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![task_id, agent, pid, log_path],
        )?;
        self.session(self.conn.last_insert_rowid())
    }

    /// Close a session with its outcome, stamping `ended_at`.
    pub fn end_session(&mut self, id: i64, outcome: SessionOutcome) -> Result<Session> {
        let changed = self.conn.execute(
            "UPDATE sessions SET ended_at = datetime('now'), outcome = ?1 WHERE id = ?2",
            params![outcome, id],
        )?;
        if changed == 0 {
            return Err(Error::SessionNotFound(id));
        }
        self.session(id)
    }

    pub fn session(&self, id: i64) -> Result<Session> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
                [id],
                session_from_row,
            )
            .optional()?
            .ok_or(Error::SessionNotFound(id))
    }

    pub fn sessions_for(&self, task_id: i64) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE task_id = ?1 ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map([task_id], session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Sessions that have not yet ended, newest first.
    pub fn live_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE ended_at IS NULL ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // --- events ---

    pub fn events_for(&self, task_id: i64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, at, kind, detail FROM events WHERE task_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([task_id], |row| {
            Ok(Event {
                id: row.get(0)?,
                task_id: row.get(1)?,
                at: row.get(2)?,
                kind: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

pub(crate) const TASK_COLUMNS: &str = "id, project_id, title, body, priority, state, agent, \
                                       question, state_since, created_at, closed_at";

pub(crate) fn get_task(conn: &Connection, id: i64) -> Result<Option<Task>> {
    Ok(conn
        .query_row(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?1"),
            [id],
            task_from_row,
        )
        .optional()?)
}

pub(crate) fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        project_id: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        priority: row.get(4)?,
        state: row.get(5)?,
        agent: row.get(6)?,
        question: row.get(7)?,
        state_since: row.get(8)?,
        created_at: row.get(9)?,
        closed_at: row.get(10)?,
    })
}

pub(crate) const SESSION_COLUMNS: &str =
    "id, task_id, agent, pid, log_path, started_at, ended_at, outcome";

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        task_id: row.get(1)?,
        agent: row.get(2)?,
        pid: row.get(3)?,
        log_path: row.get(4)?,
        started_at: row.get(5)?,
        ended_at: row.get(6)?,
        outcome: row.get(7)?,
    })
}

fn project_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        weight: row.get(3)?,
    })
}

pub(crate) fn log_event(
    conn: &Connection,
    task_id: i64,
    kind: &str,
    detail: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO events (task_id, at, kind, detail) VALUES (?1, datetime('now'), ?2, ?3)",
        params![task_id, kind, detail],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database created at schema version 1 (state still named 'backlog')
    /// must convert on open: rows renamed, deps/events surviving the table
    /// rebuild, version stamped.
    #[test]
    fn migration_0002_converts_backlog_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute("INSERT INTO projects (name, path) VALUES ('p', '/tmp')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, state, state_since, created_at)
             VALUES (1, 'blocker', 'ready', datetime('now'), datetime('now')),
                    (1, 'waiting', 'backlog', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO deps (task_id, depends_on) VALUES (2, 1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO events (task_id, at, kind, detail)
             VALUES (2, datetime('now'), 'created', 'backlog')",
            [],
        )
        .unwrap();

        let store = Store::from_connection(conn).unwrap();
        assert_eq!(store.task(2).unwrap().state, TaskState::Parked);
        assert_eq!(store.task(1).unwrap().state, TaskState::Ready);
        assert_eq!(store.deps_of(2).unwrap().len(), 1);
        // the event log is history and keeps its original wording
        assert_eq!(
            store.events_for(2).unwrap()[0].detail.as_deref(),
            Some("backlog")
        );
        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    /// A project + running task to hang sessions off of.
    fn task_fixture(s: &mut Store) -> i64 {
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        s.conn
            .execute(
                "INSERT INTO tasks (project_id, title, state, state_since, created_at)
                 VALUES (?1, 'run me', 'running', datetime('now'), datetime('now'))",
                params![p.id],
            )
            .unwrap();
        s.conn.last_insert_rowid()
    }

    #[test]
    fn session_create_end_round_trip() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);

        let opened = s
            .create_session(task_id, "claude", Some(4321), Some("/var/log/s.log"))
            .unwrap();
        assert_eq!(opened.task_id, task_id);
        assert_eq!(opened.agent, "claude");
        assert_eq!(opened.pid, Some(4321));
        assert_eq!(opened.log_path.as_deref(), Some("/var/log/s.log"));
        assert!(!opened.started_at.is_empty());
        assert!(opened.ended_at.is_none());
        assert!(opened.outcome.is_none());

        let ended = s.end_session(opened.id, SessionOutcome::Completed).unwrap();
        assert_eq!(ended.id, opened.id);
        assert!(ended.ended_at.is_some());
        assert_eq!(ended.outcome, Some(SessionOutcome::Completed));

        assert_eq!(s.session(opened.id).unwrap(), ended);
    }

    #[test]
    fn session_optional_fields_are_null() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let opened = s.create_session(task_id, "codex", None, None).unwrap();
        assert!(opened.pid.is_none());
        assert!(opened.log_path.is_none());
    }

    #[test]
    fn end_session_rejects_unknown_id() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.end_session(999, SessionOutcome::Aborted),
            Err(Error::SessionNotFound(999))
        ));
    }

    #[test]
    fn sessions_for_returns_newest_first() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let first = s.create_session(task_id, "claude", None, None).unwrap();
        let second = s.create_session(task_id, "claude", None, None).unwrap();

        let sessions = s.sessions_for(task_id).unwrap();
        assert_eq!(
            sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![second.id, first.id]
        );
    }

    #[test]
    fn live_sessions_excludes_ended() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let done = s.create_session(task_id, "claude", None, None).unwrap();
        let live = s.create_session(task_id, "claude", None, None).unwrap();
        s.end_session(done.id, SessionOutcome::Failed).unwrap();

        let ids = s.live_sessions().unwrap();
        assert_eq!(ids.iter().map(|s| s.id).collect::<Vec<_>>(), vec![live.id]);
    }

    #[test]
    fn session_outcome_serialises_for_all_variants() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        for outcome in SessionOutcome::ALL {
            let opened = s.create_session(task_id, "claude", None, None).unwrap();
            let ended = s.end_session(opened.id, outcome).unwrap();
            assert_eq!(ended.outcome, Some(outcome));
        }
    }

    /// A unique scratch database path under the OS temp dir.
    fn scratch_db() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("voro-dataversion-{}-{n}.db", std::process::id()))
    }

    #[test]
    fn data_version_tracks_external_commits_only() {
        let path = scratch_db();
        let mut a = Store::open(&path).unwrap();
        let mut b = Store::open(&path).unwrap();

        let start = a.data_version().unwrap();

        // Our own writes must not move the version this connection observes.
        a.create_project("alpha", "/tmp/alpha").unwrap();
        assert_eq!(a.data_version().unwrap(), start);

        // A commit from another connection must move it.
        b.create_project("beta", "/tmp/beta").unwrap();
        assert_ne!(a.data_version().unwrap(), start);

        drop(a);
        drop(b);
        let _ = std::fs::remove_file(&path);
    }
}
