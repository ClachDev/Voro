use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::model::{
    Blocker, Dep, DepKind, Event, Priority, Project, RunningRow, Session, SessionOutcome, Task,
    TaskState,
};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_rename_backlog_to_parked.sql"),
    include_str!("../migrations/0003_track_pr.sql"),
    include_str!("../migrations/0004_add_session_ref.sql"),
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

    /// Tasks reference a project by id, not name, so renaming is a pure
    /// label change — no task or dependency is touched.
    pub fn rename_project(&mut self, project_id: i64, name: &str) -> Result<Project> {
        let changed = self.conn.execute(
            "UPDATE projects SET name = ?1 WHERE id = ?2",
            params![name, project_id],
        )?;
        if changed == 0 {
            return Err(Error::ProjectNotFound(project_id));
        }
        self.project(project_id)
    }

    pub fn set_path(&mut self, project_id: i64, path: &str) -> Result<Project> {
        let changed = self.conn.execute(
            "UPDATE projects SET path = ?1 WHERE id = ?2",
            params![path, project_id],
        )?;
        if changed == 0 {
            return Err(Error::ProjectNotFound(project_id));
        }
        self.project(project_id)
    }

    /// Delete a project outright — only ever safe when it has no tasks, since
    /// tasks reference their project by id and deleting from under them would
    /// orphan history. A project with tasks in any state (including `done` and
    /// `rejected`) refuses; weight 0 is the designed way to snooze a project
    /// without losing its history.
    pub fn delete_project(&mut self, project_id: i64) -> Result<()> {
        self.project(project_id)?;
        let task_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )?;
        if task_count > 0 {
            return Err(Error::ProjectHasTasks {
                id: project_id,
                count: task_count,
            });
        }
        self.conn
            .execute("DELETE FROM projects WHERE id = ?1", [project_id])?;
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

    /// Track (or, with `None`, untrack) a GitHub PR on a task (DESIGN.md §11c).
    /// The URL is stored verbatim — validation and canonicalisation are the
    /// caller's job, since only the `voro` crate knows a PR reference from any
    /// other string — and the change is logged so the audit trail records when
    /// a task became reviewable-by-PR. Leaves task state untouched.
    pub fn set_pr(&mut self, id: i64, pr_url: Option<&str>) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET pr_url = ?1 WHERE id = ?2",
            params![pr_url, id],
        )?;
        if changed == 0 {
            return Err(Error::TaskNotFound(id));
        }
        log_event(&self.conn, id, "pr", pr_url.or(Some("cleared")))?;
        self.task(id)
    }

    // --- deps ---

    pub fn add_dep(&mut self, task_id: i64, depends_on: i64, kind: DepKind) -> Result<()> {
        if kind != DepKind::Blocks && task_id == depends_on {
            return Err(Error::Invalid("a task cannot depend on itself".into()));
        }
        let tx = self.conn.transaction()?;
        if kind == DepKind::Blocks {
            crate::transition::reject_blocks_cycle(&tx, task_id, depends_on)?;
        }
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

    /// Every `blocks` dependency in the store, keyed by the dependent task and
    /// resolved to each blocker's current state. One query feeds the whole
    /// browser so the render path never issues a per-row lookup.
    pub fn blockers_by_task(&self) -> Result<HashMap<i64, Vec<Blocker>>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.task_id, d.depends_on, b.state
             FROM deps d JOIN tasks b ON b.id = d.depends_on
             WHERE d.kind = 'blocks'
             ORDER BY d.task_id, d.depends_on",
        )?;
        let rows = stmt.query_map([], |row| {
            let task_id: i64 = row.get(0)?;
            let blocker = Blocker {
                id: row.get(1)?,
                state: row.get(2)?,
            };
            Ok((task_id, blocker))
        })?;
        let mut map: HashMap<i64, Vec<Blocker>> = HashMap::new();
        for row in rows {
            let (task_id, blocker) = row?;
            map.entry(task_id).or_default().push(blocker);
        }
        Ok(map)
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
        let id = insert_session(&self.conn, task_id, agent, pid, log_path)?;
        self.session(id)
    }

    /// Record the agent's own reference for a session (task #75), captured
    /// after launch — the row necessarily exists before the reference does,
    /// so this is an update rather than a `create_session` parameter.
    pub fn set_session_ref(&mut self, id: i64, session_ref: &str) -> Result<Session> {
        let changed = self.conn.execute(
            "UPDATE sessions SET session_ref = ?1 WHERE id = ?2",
            params![session_ref, id],
        )?;
        if changed == 0 {
            return Err(Error::SessionNotFound(id));
        }
        self.session(id)
    }

    /// Close a session with its outcome, stamping `ended_at`.
    pub fn end_session(&mut self, id: i64, outcome: SessionOutcome) -> Result<Session> {
        if set_session_outcome(&self.conn, id, outcome)? == 0 {
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

    /// Whether `task_id`'s most recent session ended `failed` or `capped` —
    /// the redispatch flag (DESIGN.md §8). Derived from session history on
    /// every read rather than stored on the task, per the queue/task browser
    /// rendering it rather than owning it. Only ever true for a task that is
    /// currently `ready`: that is the only way a task with a failed/capped
    /// last session can be sitting still, since a fresh dispatch opens a new
    /// session ahead of any later failure.
    pub fn redispatch_flag(&self, task_id: i64) -> Result<bool> {
        let outcome: Option<SessionOutcome> = self
            .conn
            .query_row(
                "SELECT outcome FROM sessions WHERE task_id = ?1 ORDER BY id DESC LIMIT 1",
                [task_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(matches!(
            outcome,
            Some(SessionOutcome::Failed | SessionOutcome::Capped)
        ))
    }

    /// Rows for the cockpit's running strip (DESIGN.md §9): every live session
    /// joined with its task (newest first), followed by every `running` task
    /// that has no live session. The latter is a task nothing is actively
    /// driving — started by hand, or a session that died before the reconciler
    /// demoted it — surfaced because it is in the wrong state and needs
    /// attention; its `session_id`/`agent` are `NULL` and its elapsed time is
    /// measured from when it entered `running` rather than from a session
    /// start. Elapsed time is computed against the database's clock so the TUI
    /// only formats it.
    pub fn running_rows(&self) -> Result<Vec<RunningRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id AS session_id, s.task_id, t.title, t.state, s.agent, s.started_at,
                    CAST(strftime('%s', 'now') - strftime('%s', s.started_at) AS INTEGER),
                    0 AS orphan
             FROM sessions s JOIN tasks t ON t.id = s.task_id
             WHERE s.ended_at IS NULL
             UNION ALL
             SELECT NULL, t.id, t.title, t.state, NULL, t.state_since,
                    CAST(strftime('%s', 'now') - strftime('%s', t.state_since) AS INTEGER),
                    1 AS orphan
             FROM tasks t
             WHERE t.state = 'running'
               AND NOT EXISTS (
                   SELECT 1 FROM sessions s
                   WHERE s.task_id = t.id AND s.ended_at IS NULL
               )
             ORDER BY orphan ASC, session_id DESC, task_id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RunningRow {
                session_id: row.get(0)?,
                task_id: row.get(1)?,
                task_title: row.get(2)?,
                task_state: row.get(3)?,
                agent: row.get(4)?,
                started_at: row.get(5)?,
                elapsed_secs: row.get(6)?,
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
                                       question, pr_url, state_since, created_at, closed_at";

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
        pr_url: row.get(8)?,
        state_since: row.get(9)?,
        created_at: row.get(10)?,
        closed_at: row.get(11)?,
    })
}

pub(crate) const SESSION_COLUMNS: &str =
    "id, task_id, agent, pid, session_ref, log_path, started_at, ended_at, outcome";

pub(crate) fn get_session(conn: &Connection, id: i64) -> Result<Option<Session>> {
    Ok(conn
        .query_row(
            &format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?1"),
            [id],
            session_from_row,
        )
        .optional()?)
}

/// The id of a task's most recent session — the one that last put it in
/// `running`, matching the newest-first ordering of [`Store::sessions_for`].
/// `None` when the task has no sessions. Used by reconciliation to tell a
/// task's live session apart from an older, superseded one.
pub(crate) fn latest_session_id(conn: &Connection, task_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT MAX(id) FROM sessions WHERE task_id = ?1",
            [task_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        task_id: row.get(1)?,
        agent: row.get(2)?,
        pid: row.get(3)?,
        session_ref: row.get(4)?,
        log_path: row.get(5)?,
        started_at: row.get(6)?,
        ended_at: row.get(7)?,
        outcome: row.get(8)?,
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

/// Insert a session row, stamping `started_at`, and return its id. Shared by
/// [`Store::create_session`] and the dispatch transaction in `transition.rs`.
pub(crate) fn insert_session(
    conn: &Connection,
    task_id: i64,
    agent: &str,
    pid: Option<i64>,
    log_path: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO sessions (task_id, agent, pid, log_path, started_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        params![task_id, agent, pid, log_path],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Stamp `ended_at` and record `outcome` on a session, returning the number
/// of rows changed (0 if the id is unknown). Shared by [`Store::end_session`]
/// and reconciliation (`transition.rs`), which folds it into the same
/// transaction as the task's `running → ready` drop.
pub(crate) fn set_session_outcome(
    conn: &Connection,
    id: i64,
    outcome: SessionOutcome,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE sessions SET ended_at = datetime('now'), outcome = ?1 WHERE id = ?2",
        params![outcome, id],
    )?)
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
    use crate::transition::{Action, Triage};

    #[test]
    fn rename_project_updates_name_and_leaves_task_references_intact() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("old-name", "/tmp/old").unwrap();
        let task = s
            .create_task(NewTask {
                project_id: p.id,
                title: "t".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();

        let renamed = s.rename_project(p.id, "new-name").unwrap();
        assert_eq!(renamed.id, p.id);
        assert_eq!(renamed.name, "new-name");

        // the task still resolves to the same project by id, under its new name
        let reloaded = s.task(task.id).unwrap();
        assert_eq!(reloaded.project_id, p.id);
        assert_eq!(s.project(reloaded.project_id).unwrap().name, "new-name");
    }

    #[test]
    fn rename_project_rejects_unknown_id() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.rename_project(999, "x"),
            Err(Error::ProjectNotFound(999))
        ));
    }

    #[test]
    fn set_pr_tracks_clears_and_logs() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "review me".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();
        assert!(s.task(t.id).unwrap().pr_url.is_none());

        let tracked = s
            .set_pr(t.id, Some("https://github.com/acme/widget/pull/42"))
            .unwrap();
        assert_eq!(
            tracked.pr_url.as_deref(),
            Some("https://github.com/acme/widget/pull/42")
        );
        // state is untouched by tracking a PR
        assert_eq!(tracked.state, TaskState::Ready);

        let cleared = s.set_pr(t.id, None).unwrap();
        assert!(cleared.pr_url.is_none());

        let events = s.events_for(t.id).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["created", "pr", "pr"]);
        assert!(matches!(s.set_pr(999, None), Err(Error::TaskNotFound(999))));
    }

    #[test]
    fn set_path_updates_path() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/old").unwrap();
        let updated = s.set_path(p.id, "/tmp/new").unwrap();
        assert_eq!(updated.path, "/tmp/new");
        assert_eq!(s.project(p.id).unwrap().path, "/tmp/new");
    }

    #[test]
    fn set_path_rejects_unknown_id() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.set_path(999, "/tmp"),
            Err(Error::ProjectNotFound(999))
        ));
    }

    #[test]
    fn delete_project_removes_a_taskless_project() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("empty", "/tmp/empty").unwrap();
        s.delete_project(p.id).unwrap();
        assert!(matches!(s.project(p.id), Err(Error::ProjectNotFound(_))));
        assert!(s.projects().unwrap().is_empty());
    }

    #[test]
    fn delete_project_rejects_unknown_id() {
        let mut s = Store::open_in_memory().unwrap();
        assert!(matches!(
            s.delete_project(999),
            Err(Error::ProjectNotFound(999))
        ));
    }

    /// Walk a fresh task into `state` through the transition API, mirroring
    /// the equivalent helper in `transition.rs`'s own tests.
    fn task_in_state(s: &mut Store, project_id: i64, state: TaskState) -> i64 {
        use TaskState::*;
        let create = |s: &mut Store, state| {
            s.create_task(NewTask {
                project_id,
                title: format!("task in {state}"),
                body: String::new(),
                priority: Priority::P1,
                state,
                agent: None,
            })
            .unwrap()
            .id
        };
        match state {
            Proposed | Parked | Ready => create(s, state),
            Running => {
                let id = create(s, Ready);
                s.apply(id, Action::Start).unwrap();
                id
            }
            NeedsInput => {
                let id = task_in_state(s, project_id, Running);
                s.apply(id, Action::Ask("which schema?".into())).unwrap();
                id
            }
            Review => {
                let id = task_in_state(s, project_id, Running);
                s.apply(id, Action::Complete).unwrap();
                id
            }
            Done => {
                let id = task_in_state(s, project_id, Review);
                s.apply(id, Action::Accept).unwrap();
                id
            }
            Rejected => {
                let id = create(s, Proposed);
                s.apply(id, Action::Triage(Triage::Reject)).unwrap();
                id
            }
        }
    }

    #[test]
    fn delete_project_refuses_with_a_task_in_any_state() {
        for state in TaskState::ALL {
            let mut s = Store::open_in_memory().unwrap();
            let p = s.create_project("proj", "/tmp/proj").unwrap();
            task_in_state(&mut s, p.id, state);

            let err = s.delete_project(p.id).unwrap_err();
            assert!(
                matches!(err, Error::ProjectHasTasks { id, count } if id == p.id && count == 1),
                "state {state}: expected ProjectHasTasks, got {err}"
            );
            // the refusal must not have touched the project
            assert!(s.project(p.id).is_ok());
        }
    }

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
        assert_eq!(version, MIGRATIONS.len() as i64);
        // 0004 gave the sessions table its session_ref column
        let refs: i64 = store
            .conn
            .query_row("SELECT COUNT(session_ref) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(refs, 0);
    }

    /// A project + running task to hang sessions off of.
    fn task_fixture(s: &mut Store) -> i64 {
        s.conn
            .execute(
                "INSERT OR IGNORE INTO projects (name, path) VALUES ('voro', '/tmp/voro')",
                [],
            )
            .unwrap();
        let project_id: i64 = s
            .conn
            .query_row("SELECT id FROM projects WHERE name = 'voro'", [], |r| {
                r.get(0)
            })
            .unwrap();
        s.conn
            .execute(
                "INSERT INTO tasks (project_id, title, state, state_since, created_at)
                 VALUES (?1, 'run me', 'running', datetime('now'), datetime('now'))",
                params![project_id],
            )
            .unwrap();
        s.conn.last_insert_rowid()
    }

    /// `events_for` must return the audit trail oldest-first (newest last),
    /// since that's the order the history popup renders it in.
    #[test]
    fn events_for_orders_oldest_first() {
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let task = s
            .create_task(NewTask {
                project_id: p.id,
                title: "trace me".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();
        s.apply(task.id, Action::Start).unwrap();
        s.apply(task.id, Action::Ask("A or B?".into())).unwrap();
        s.apply(task.id, Action::Answer("B".into())).unwrap();

        let events = s.events_for(task.id).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "created",
                "transition",
                "transition",
                "transition",
                "answer"
            ]
        );
        // ids strictly increase with insertion order
        assert!(events.windows(2).all(|w| w[0].id < w[1].id));
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
        assert!(opened.session_ref.is_none());
        assert!(opened.log_path.is_none());
    }

    #[test]
    fn set_session_ref_records_and_rejects_unknown_ids() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let opened = s.create_session(task_id, "claude", None, None).unwrap();
        assert!(opened.session_ref.is_none());

        let updated = s
            .set_session_ref(opened.id, "3f6c0e6e-1111-2222-3333-444455556666")
            .unwrap();
        assert_eq!(
            updated.session_ref.as_deref(),
            Some("3f6c0e6e-1111-2222-3333-444455556666")
        );
        assert_eq!(s.session(opened.id).unwrap(), updated);

        assert!(matches!(
            s.set_session_ref(999, "x"),
            Err(Error::SessionNotFound(999))
        ));
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
    fn running_rows_join_current_task_fields() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let session = s.create_session(task_id, "claude", None, None).unwrap();

        let rows = s.running_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, Some(session.id));
        assert_eq!(rows[0].task_id, task_id);
        assert_eq!(rows[0].task_title, "run me");
        assert_eq!(rows[0].task_state, TaskState::Running);
        assert_eq!(rows[0].agent.as_deref(), Some("claude"));
        assert!(rows[0].elapsed_secs >= 0);
    }

    #[test]
    fn running_rows_exclude_ended_sessions_and_order_newest_first() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let done = s.create_session(task_id, "claude", None, None).unwrap();
        let live = s.create_session(task_id, "codex", None, None).unwrap();
        s.end_session(done.id, SessionOutcome::Completed).unwrap();

        let rows = s.running_rows().unwrap();
        assert_eq!(
            rows.iter().map(|r| r.session_id).collect::<Vec<_>>(),
            vec![Some(live.id)]
        );
        assert_eq!(rows[0].agent.as_deref(), Some("codex"));
    }

    #[test]
    fn running_rows_compute_elapsed_from_started_at() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let session = s.create_session(task_id, "claude", None, None).unwrap();
        s.conn
            .execute(
                "UPDATE sessions SET started_at = datetime('now', '-90 seconds') WHERE id = ?1",
                params![session.id],
            )
            .unwrap();

        let rows = s.running_rows().unwrap();
        assert_eq!(rows.len(), 1);
        // allow a couple of seconds of test-execution slack either side
        assert!(
            (85..=95).contains(&rows[0].elapsed_secs),
            "expected ~90s elapsed, got {}",
            rows[0].elapsed_secs
        );
    }

    /// A task can be `running` with no live session — started by hand, or a
    /// session that died before the reconciler demoted it. The running strip
    /// must still surface it (DESIGN.md §9), with no session id or agent and
    /// elapsed measured from when it entered `running`.
    #[test]
    fn running_rows_include_running_task_without_live_session() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        s.conn
            .execute(
                "UPDATE tasks SET state_since = datetime('now', '-90 seconds') WHERE id = ?1",
                params![task_id],
            )
            .unwrap();

        let rows = s.running_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, None);
        assert_eq!(rows[0].agent, None);
        assert_eq!(rows[0].task_id, task_id);
        assert_eq!(rows[0].task_state, TaskState::Running);
        assert!(
            (85..=95).contains(&rows[0].elapsed_secs),
            "expected ~90s in running, got {}",
            rows[0].elapsed_secs
        );
    }

    /// A running task whose only session has ended is session-less too, so it
    /// stays visible rather than dropping off the strip.
    #[test]
    fn running_rows_include_task_whose_sessions_all_ended() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        let done = s.create_session(task_id, "claude", None, None).unwrap();
        s.end_session(done.id, SessionOutcome::Failed).unwrap();

        let rows = s.running_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, None);
        assert_eq!(rows[0].task_id, task_id);
    }

    /// Live sessions sort ahead of session-less running tasks, so what an agent
    /// is actively driving stays at the top of the strip.
    #[test]
    fn running_rows_order_live_sessions_before_session_less_tasks() {
        let mut s = Store::open_in_memory().unwrap();
        let live_task = task_fixture(&mut s);
        let session = s.create_session(live_task, "claude", None, None).unwrap();
        let orphan_task = task_fixture(&mut s);

        let rows = s.running_rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].session_id, Some(session.id));
        assert_eq!(rows[0].task_id, live_task);
        assert_eq!(rows[1].session_id, None);
        assert_eq!(rows[1].task_id, orphan_task);
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

    #[test]
    fn blockers_by_task_resolves_state_and_ignores_non_blocks_deps() {
        use crate::model::{DepKind, Priority};
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str, state: TaskState| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
        };
        let open = s.create_task(new("open", TaskState::Ready)).unwrap();
        let closed = s.create_task(new("closed", TaskState::Ready)).unwrap();
        s.apply(closed.id, Action::Start).unwrap();
        s.apply(closed.id, Action::Complete).unwrap();
        s.apply(closed.id, Action::Accept).unwrap();
        let sibling = s.create_task(new("sibling", TaskState::Ready)).unwrap();
        let waiting = s.create_task(new("waiting", TaskState::Ready)).unwrap();

        s.set_blocks_deps(waiting.id, &[open.id, closed.id])
            .unwrap();
        // A non-blocks dep must not appear as a blocker.
        s.add_dep(waiting.id, sibling.id, DepKind::Related).unwrap();

        let map = s.blockers_by_task().unwrap();
        let blockers = &map[&waiting.id];
        assert_eq!(blockers.len(), 2);
        assert!(blockers.iter().all(|b| b.id != sibling.id));

        let open_b = blockers.iter().find(|b| b.id == open.id).unwrap();
        assert!(open_b.is_open());
        let closed_b = blockers.iter().find(|b| b.id == closed.id).unwrap();
        assert_eq!(closed_b.state, TaskState::Done);
        assert!(!closed_b.is_open());

        // Tasks without blocks deps are simply absent from the map.
        assert!(!map.contains_key(&open.id));
    }
}
