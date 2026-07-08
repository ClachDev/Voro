use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::model::{Dep, DepKind, Event, Priority, Project, Task, TaskState};

const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

/// Owns the SQLite database. All writes go through this type; task state in
/// particular is only ever changed by the transition API in `transition.rs`.
pub struct Store {
    pub(crate) conn: Connection,
}

/// Initial state for a task created by a human. `proposed` is quick capture;
/// `backlog`/`ready` mean the creator has already triaged their own task.
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

    /// `$XDG_DATA_HOME/focus/focus.db`, defaulting to `~/.local/share`.
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
        data_home.join("focus/focus.db")
    }

    fn from_connection(conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "foreign_keys", true)?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
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
            TaskState::Proposed | TaskState::Backlog | TaskState::Ready
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
