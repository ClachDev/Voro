use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{Error, Result};
use crate::model::{
    Dep, DepKind, DepRef, Event, Priority, Project, ReviewAction, RunningRow, Session,
    SessionOutcome, Task, TaskState,
};

const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_rename_backlog_to_parked.sql"),
    include_str!("../migrations/0003_track_pr.sql"),
    include_str!("../migrations/0004_add_session_ref.sql"),
    include_str!("../migrations/0005_add_branch.sql"),
    include_str!("../migrations/0006_one_open_session_per_task.sql"),
    include_str!("../migrations/0007_add_human.sql"),
    include_str!("../migrations/0008_add_stalled_state.sql"),
    include_str!("../migrations/0009_add_review_action.sql"),
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
    pub human: bool,
}

/// Content edits. State is deliberately absent — use `Store::apply`.
#[derive(Debug, Clone)]
pub struct TaskEdit {
    pub title: String,
    pub body: String,
    pub priority: Priority,
    pub agent: Option<String>,
    pub human: bool,
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
                "SELECT id, name, path, weight, review_action FROM projects WHERE id = ?1",
                [id],
                project_from_row,
            )
            .optional()?
            .ok_or(Error::ProjectNotFound(id))
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, weight, review_action FROM projects ORDER BY name")?;
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

    /// Set how `pr` shows this project's review diffs (DESIGN.md §8/§11a).
    /// `Auto` stores NULL — the medium goes back to being resolved at use.
    pub fn set_review_action(&mut self, project_id: i64, action: &ReviewAction) -> Result<Project> {
        let changed = self.conn.execute(
            "UPDATE projects SET review_action = ?1 WHERE id = ?2",
            params![action, project_id],
        )?;
        if changed == 0 {
            return Err(Error::ProjectNotFound(project_id));
        }
        self.project(project_id)
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
        if new.human && new.agent.is_some() {
            return Err(Error::Invalid(
                "a human-only task cannot carry an agent override — the override only \
                 selects a dispatch agent, and no agent can execute the task"
                    .into(),
            ));
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO tasks (project_id, title, body, priority, state, agent, human,
                                state_since, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), datetime('now'))",
            params![
                new.project_id,
                new.title,
                new.body,
                new.priority,
                new.state,
                new.agent,
                new.human
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
        let current = self.task(id)?;
        if edit.human && edit.agent.is_some() {
            return Err(Error::HumanTask {
                id,
                reason: "an agent override is meaningless on a task no agent can execute — \
                         clear one or the other"
                    .into(),
            });
        }
        // `needs-input`, `review`, and `stalled` are unreachable for human
        // tasks (§6): the executor cannot be blocked on their own decision,
        // completion skips review, and only a dispatched session can die. A
        // task sitting in any of them — or one an agent session is still open
        // on — is demonstrably agent-executed, so it cannot be marked
        // human-only as it stands.
        if edit.human && !current.human {
            if matches!(
                current.state,
                TaskState::NeedsInput | TaskState::Review | TaskState::Stalled
            ) {
                return Err(Error::HumanTask {
                    id,
                    reason: format!(
                        "a task in state '{}' was executed by an agent; resolve it first",
                        current.state
                    ),
                });
            }
            let open_sessions: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM sessions WHERE task_id = ?1 AND ended_at IS NULL",
                [id],
                |r| r.get(0),
            )?;
            if open_sessions > 0 {
                return Err(Error::HumanTask {
                    id,
                    reason: "an agent session is still open on it; complete or abort it first"
                        .into(),
                });
            }
        }
        self.conn.execute(
            "UPDATE tasks SET title = ?1, body = ?2, priority = ?3, agent = ?4, human = ?5
             WHERE id = ?6",
            params![
                edit.title,
                edit.body,
                edit.priority,
                edit.agent,
                edit.human,
                id
            ],
        )?;
        self.task(id)
    }

    /// Re-prioritise a task in isolation (DESIGN.md §7), the fast path used when
    /// the operator realises mid-review that a task's priority is wrong. Unlike
    /// `update_task` this touches only `priority`, and it logs the change so the
    /// audit trail records when a task was re-prioritised. Task state is left
    /// untouched; scoring picks up the new priority on the next refresh.
    pub fn set_priority(&mut self, id: i64, priority: Priority) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET priority = ?1 WHERE id = ?2",
            params![priority, id],
        )?;
        if changed == 0 {
            return Err(Error::TaskNotFound(id));
        }
        log_event(&self.conn, id, "priority", Some(&priority.to_string()))?;
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

    /// Record (or, with `None`, clear) the git branch a task's work lives on
    /// (task #81) — the intended name a human sets for dispatch to inject, or
    /// the name an agent reports back through `voro done --branch`. The value
    /// is stored verbatim; Voro never runs git, so it neither validates the
    /// name nor touches the checkout. The change is logged, and task state is
    /// left untouched.
    pub fn set_branch(&mut self, id: i64, branch: Option<&str>) -> Result<Task> {
        let changed = self.conn.execute(
            "UPDATE tasks SET branch = ?1 WHERE id = ?2",
            params![branch, id],
        )?;
        if changed == 0 {
            return Err(Error::TaskNotFound(id));
        }
        log_event(&self.conn, id, "branch", branch.or(Some("cleared")))?;
        self.task(id)
    }

    /// Set or replace a task's completion summary outside `done` (DESIGN.md
    /// §8): append a `summary` event, which [`latest_summary`] naturally
    /// supersedes with — the PR body, the detail view, and the
    /// incomplete-report flag all read the newest event, so every consumer
    /// picks up the replacement on its next read and the append-only log keeps
    /// the older account. This is how a stale PR body gets amended and how a
    /// `review` task flagged `[incomplete report]` gets its missing summary
    /// without a `reject` → re-`done` round trip. Allowed only on a `running`
    /// task (a resumed agent recording its account before `done`) or a
    /// `review` task (fixing the report after it); it never touches
    /// `tasks.state`, so it composes with the transition API rather than
    /// bypassing it.
    ///
    /// [`latest_summary`]: Store::latest_summary
    pub fn set_summary(&mut self, id: i64, summary: &str) -> Result<Task> {
        if summary.trim().is_empty() {
            return Err(Error::Invalid("a summary is required".into()));
        }
        let task = self.task(id)?;
        if !matches!(task.state, TaskState::Running | TaskState::Review) {
            return Err(Error::Invalid(format!(
                "a summary can only be set on a running or review task; task {} is {}",
                id, task.state
            )));
        }
        log_event(&self.conn, id, "summary", Some(summary.trim()))?;
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

    /// Every dependency edge of every kind, keyed by the depending task and
    /// resolved to the dependency's current title and state — the forward
    /// direction a detail view renders as `blocked by #N` or `<kind> #N`.
    /// One query feeds every pane, browser rows included, so the render path
    /// never issues a per-row lookup.
    pub fn deps_by_task(&self) -> Result<HashMap<i64, Vec<DepRef>>> {
        self.dep_refs(
            "SELECT d.task_id, t.id, t.title, t.state, d.kind
             FROM deps d JOIN tasks t ON t.id = d.depends_on
             ORDER BY d.task_id, t.id",
        )
    }

    /// The reverse edges: every dependency keyed by the task depended *on*,
    /// resolved to the depending task — who a task blocks (or spawned), which
    /// no forward query can answer.
    pub fn dependents_by_task(&self) -> Result<HashMap<i64, Vec<DepRef>>> {
        self.dep_refs(
            "SELECT d.depends_on, t.id, t.title, t.state, d.kind
             FROM deps d JOIN tasks t ON t.id = d.task_id
             ORDER BY d.depends_on, t.id",
        )
    }

    fn dep_refs(&self, sql: &str) -> Result<HashMap<i64, Vec<DepRef>>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            let key: i64 = row.get(0)?;
            let dep = DepRef {
                id: row.get(1)?,
                title: row.get(2)?,
                state: row.get(3)?,
                kind: row.get(4)?,
            };
            Ok((key, dep))
        })?;
        let mut map: HashMap<i64, Vec<DepRef>> = HashMap::new();
        for row in rows {
            let (key, dep) = row?;
            map.entry(key).or_default().push(dep);
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

    /// Every task's newest session, keyed by task id, in one query — what the
    /// TUI loads per refresh so the detail views and the log key can answer
    /// "what is/was this session doing?" for any task without querying the
    /// store mid-draw. Session ids are monotonic, so `max(id)` is the latest.
    pub fn latest_sessions(&self) -> Result<std::collections::HashMap<i64, Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions s
             WHERE s.id = (SELECT max(id) FROM sessions WHERE task_id = s.task_id)"
        ))?;
        let rows = stmt.query_map([], session_from_row)?;
        rows.map(|r| r.map(|s| (s.task_id, s)))
            .collect::<rusqlite::Result<_>>()
            .map_err(Into::into)
    }

    /// Sessions that have not yet ended, newest first.
    pub fn live_sessions(&self) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE ended_at IS NULL ORDER BY id DESC"
        ))?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Whether `task_id` is a `review` task carrying a *partial* completion
    /// report — exactly one of its branch and its summary is present (DESIGN.md
    /// §8). A dispatched agent should record both whatever the project's review
    /// medium: the summary is the review context and the reject-with-feedback
    /// source, the branch ties the task to its work, and on the GitHub medium
    /// `pr` additionally needs both to open a PR. One without the other is
    /// therefore a report left half-finished — `done` with `--branch` but no
    /// `--summary`, or the reverse, or the `SessionEnd` fallback recording a
    /// branch with no summary — an anomaly the operator must see. A review task
    /// with *neither* is deliberately not flagged — a planning or
    /// task-generation task legitimately produces no branch and no summary —
    /// and one with *both* is a complete report. Gated on `review` because that
    /// is where the report is read; derived fresh from task and event state on
    /// every read rather than stored on the task.
    pub fn incomplete_report_flag(&self, task_id: i64) -> Result<bool> {
        let row: Option<(TaskState, Option<String>)> = self
            .conn
            .query_row(
                "SELECT state, branch FROM tasks WHERE id = ?1",
                [task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((state, branch)) = row else {
            return Ok(false);
        };
        if state != TaskState::Review {
            return Ok(false);
        }
        let has_branch = branch.is_some();
        let has_summary = self.latest_summary(task_id)?.is_some();
        Ok(has_branch != has_summary)
    }

    /// Rows for the cockpit's running strip (DESIGN.md §9): every `running`
    /// task, joined with its open session if it has one. The strip filters on
    /// task *state*, not on "has an open session" — a `review` or `needs-input`
    /// task keeps its session open behind the scenes (for feedback/answer
    /// continuation, §8) but belongs to the queue, not the strip, so those
    /// never appear here even though their session row is still open. A task
    /// nothing is actively driving — started by hand, so no session was ever
    /// opened (a dead dispatch is stalled by reconcile instead, §8) — has no
    /// open session, so its `session_id`/`agent` are `NULL` and its elapsed
    /// time is measured from when it entered `running`. The one-open-session
    /// invariant (§8) makes the join at most one row per task, so a task
    /// appears exactly once. Elapsed time is computed against the database's
    /// clock so the TUI only formats it.
    pub fn running_rows(&self) -> Result<Vec<RunningRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id AS session_id, t.id, t.title, t.state, s.agent,
                    COALESCE(s.started_at, t.state_since) AS since,
                    CAST(strftime('%s', 'now')
                         - strftime('%s', COALESCE(s.started_at, t.state_since)) AS INTEGER)
             FROM tasks t
             LEFT JOIN sessions s ON s.task_id = t.id AND s.ended_at IS NULL
             WHERE t.state = 'running'
             ORDER BY (s.id IS NULL), s.id DESC, t.id DESC",
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

    /// The most recent completion summary a task recorded (DESIGN.md §8): the
    /// detail of its newest `summary` event, logged by `done --summary` or
    /// amended in place by `set --summary` ([`set_summary`]). This
    /// is the PR body when `pr` opens a pull request, and the presence check
    /// `done` warns on. `None` when the task has never carried a summary — a
    /// planning or task-generation task that produced no code, which is why the
    /// summary stays optional through the whole lifecycle.
    ///
    /// [`set_summary`]: Store::set_summary
    pub fn latest_summary(&self, task_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT detail FROM events WHERE task_id = ?1 AND kind = 'summary'
                 ORDER BY id DESC LIMIT 1",
                [task_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

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
                                       question, pr_url, branch, state_since, created_at, \
                                       closed_at, human";

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
        branch: row.get(9)?,
        state_since: row.get(10)?,
        created_at: row.get(11)?,
        closed_at: row.get(12)?,
        human: row.get(13)?,
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
        review_action: row.get(4)?,
    })
}

/// Insert a session row, stamping `started_at`, and return its id. Shared by
/// [`Store::create_session`] and the dispatch/continuation transactions in
/// `transition.rs`. Enforces the one-open-session invariant (DESIGN.md §8):
/// opening a new session first closes any predecessor still open for the same
/// task (stamped `aborted` — it was superseded), so a task never carries two
/// open rows at once. The partial unique index is the schema-level backstop.
pub(crate) fn insert_session(
    conn: &Connection,
    task_id: i64,
    agent: &str,
    pid: Option<i64>,
    log_path: Option<&str>,
) -> Result<i64> {
    close_open_session(conn, task_id, SessionOutcome::Aborted)?;
    conn.execute(
        "INSERT INTO sessions (task_id, agent, pid, log_path, started_at)
         VALUES (?1, ?2, ?3, ?4, datetime('now'))",
        params![task_id, agent, pid, log_path],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Close a task's currently-open session, if it has one, stamping `ended_at`
/// and `outcome`. The one-open-session invariant (DESIGN.md §8) means this
/// touches at most one row. Used both to supersede a predecessor when a new
/// session opens and to close the session on a terminal transition
/// (accept/abandon/abort) in `transition.rs`, all within the caller's
/// transaction. Returns the number of rows closed (0 if none was open).
pub(crate) fn close_open_session(
    conn: &Connection,
    task_id: i64,
    outcome: SessionOutcome,
) -> Result<usize> {
    Ok(conn.execute(
        "UPDATE sessions SET ended_at = datetime('now'), outcome = ?1
         WHERE task_id = ?2 AND ended_at IS NULL",
        params![outcome, task_id],
    )?)
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
                human: false,
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
    fn review_action_defaults_to_auto_and_round_trips() {
        use crate::model::ReviewAction;
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/proj").unwrap();
        assert_eq!(p.review_action, ReviewAction::Auto);

        let action = ReviewAction::Viewer(Some("zed".into()));
        let updated = s.set_review_action(p.id, &action).unwrap();
        assert_eq!(updated.review_action, action);
        assert_eq!(s.project(p.id).unwrap().review_action, action);
        assert_eq!(s.projects().unwrap()[0].review_action, action);

        // Auto writes NULL, so the column reads back empty
        s.set_review_action(p.id, &ReviewAction::Auto).unwrap();
        assert_eq!(s.project(p.id).unwrap().review_action, ReviewAction::Auto);
        let raw: Option<String> = s
            .conn
            .query_row(
                "SELECT review_action FROM projects WHERE id = ?1",
                [p.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(raw, None);

        assert!(matches!(
            s.set_review_action(999, &ReviewAction::Pr),
            Err(Error::ProjectNotFound(999))
        ));
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
                human: false,
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
    fn set_priority_updates_leaves_state_and_logs() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "reprioritise me".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();

        let raised = s.set_priority(t.id, Priority::P0).unwrap();
        assert_eq!(raised.priority, Priority::P0);
        // priority is changed in isolation; state is untouched
        assert_eq!(raised.state, TaskState::Ready);

        let events = s.events_for(t.id).unwrap();
        let last = events.last().unwrap();
        assert_eq!(last.kind, "priority");
        assert_eq!(last.detail.as_deref(), Some("P0"));

        assert!(matches!(
            s.set_priority(999, Priority::P1),
            Err(Error::TaskNotFound(999))
        ));
    }

    #[test]
    fn set_branch_records_clears_and_logs() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "branch me".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        assert!(s.task(t.id).unwrap().branch.is_none());

        let named = s.set_branch(t.id, Some("feat/parser")).unwrap();
        assert_eq!(named.branch.as_deref(), Some("feat/parser"));
        // recording a branch never touches task state
        assert_eq!(named.state, TaskState::Ready);

        // reporting a different branch overwrites the intended one
        let renamed = s.set_branch(t.id, Some("feat/parser-v2")).unwrap();
        assert_eq!(renamed.branch.as_deref(), Some("feat/parser-v2"));

        let cleared = s.set_branch(t.id, None).unwrap();
        assert!(cleared.branch.is_none());

        let events = s.events_for(t.id).unwrap();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["created", "branch", "branch", "branch"]);
        assert!(matches!(
            s.set_branch(999, None),
            Err(Error::TaskNotFound(999))
        ));
    }

    // --- the human flag and the agent override are mutually exclusive (§3/§6) ---

    /// A store, a project, and a `NewTask` builder for the human-flag tests.
    fn human_fixture() -> (Store, i64) {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        (s, p.id)
    }

    fn new_with(project_id: i64, agent: Option<&str>, human: bool) -> NewTask {
        NewTask {
            project_id,
            title: "hands-on".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: agent.map(str::to_string),
            human,
        }
    }

    fn edit_of(task: &Task, agent: Option<&str>, human: bool) -> TaskEdit {
        TaskEdit {
            title: task.title.clone(),
            body: task.body.clone(),
            priority: task.priority,
            agent: agent.map(str::to_string),
            human,
        }
    }

    #[test]
    fn create_task_refuses_a_human_task_with_an_agent_override() {
        let (mut s, p) = human_fixture();
        let err = s.create_task(new_with(p, Some("codex"), true)).unwrap_err();
        assert!(err.to_string().contains("agent override"), "{err}");
        assert!(s.tasks().unwrap().is_empty());

        // either alone is fine
        assert!(s.create_task(new_with(p, Some("codex"), false)).is_ok());
        let human = s.create_task(new_with(p, None, true)).unwrap();
        assert!(human.human);
    }

    #[test]
    fn update_task_guards_the_agent_human_exclusivity_both_ways() {
        let (mut s, p) = human_fixture();

        // an agent override cannot land on a human task
        let human = s.create_task(new_with(p, None, true)).unwrap();
        let err = s
            .update_task(human.id, edit_of(&human, Some("codex"), true))
            .unwrap_err();
        assert!(matches!(err, Error::HumanTask { id, .. } if id == human.id));

        // ...and the flag cannot land while an override is kept
        let agented = s.create_task(new_with(p, Some("codex"), false)).unwrap();
        let err = s
            .update_task(agented.id, edit_of(&agented, Some("codex"), true))
            .unwrap_err();
        assert!(matches!(err, Error::HumanTask { id, .. } if id == agented.id));

        // clearing the override in the same edit is the designed way through
        let flipped = s
            .update_task(agented.id, edit_of(&agented, None, true))
            .unwrap();
        assert!(flipped.human);
        assert!(flipped.agent.is_none());
    }

    #[test]
    fn update_task_refuses_flagging_human_in_agent_executed_states() {
        use crate::transition::Action;

        // needs-input, review, and stalled are unreachable for human tasks
        // (§6), so a task already sitting there cannot be flagged as one.
        for walk in [TaskState::NeedsInput, TaskState::Review, TaskState::Stalled] {
            let (mut s, p) = human_fixture();
            let t = s.create_task(new_with(p, None, false)).unwrap();
            match walk {
                TaskState::NeedsInput => {
                    s.apply(t.id, Action::Start).unwrap();
                    s.apply(t.id, Action::Ask("A or B?".into())).unwrap();
                }
                TaskState::Stalled => {
                    let (_, session) = s.record_dispatch(t.id, "claude", Some(1), None).unwrap();
                    s.reconcile_session(session.id, false, false).unwrap();
                }
                _ => {
                    s.apply(t.id, Action::Start).unwrap();
                    s.apply(t.id, Action::Complete(None)).unwrap();
                }
            }
            assert_eq!(s.task(t.id).unwrap().state, walk);
            let err = s.update_task(t.id, edit_of(&t, None, true)).unwrap_err();
            assert!(
                matches!(err, Error::HumanTask { id, .. } if id == t.id),
                "{walk}: {err}"
            );
            assert!(!s.task(t.id).unwrap().human);
        }
    }

    #[test]
    fn update_task_refuses_flagging_human_while_a_session_is_open() {
        use crate::transition::Action;

        let (mut s, p) = human_fixture();
        let t = s.create_task(new_with(p, None, false)).unwrap();
        s.record_dispatch(t.id, "claude", Some(1), None).unwrap();

        let err = s.update_task(t.id, edit_of(&t, None, true)).unwrap_err();
        assert!(matches!(err, Error::HumanTask { id, .. } if id == t.id));

        // once the session is torn down the flip is allowed again
        s.apply(t.id, Action::Abort).unwrap();
        let flipped = s.update_task(t.id, edit_of(&t, None, true)).unwrap();
        assert!(flipped.human);

        // a hand-started running task has no session and can flip freely
        let by_hand = s.create_task(new_with(p, None, false)).unwrap();
        s.apply(by_hand.id, Action::Start).unwrap();
        assert!(
            s.update_task(by_hand.id, edit_of(&by_hand, None, true))
                .unwrap()
                .human
        );
    }

    /// A database from before migration 0007 must open with every existing
    /// task dispatchable (`human = 0`), and the CHECK must reject junk.
    #[test]
    fn migration_0007_defaults_existing_tasks_to_dispatchable() {
        let conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..6] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 6).unwrap();
        conn.execute("INSERT INTO projects (name, path) VALUES ('p', '/tmp')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, state, state_since, created_at)
             VALUES (1, 'pre-flag', 'ready', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        let store = Store::from_connection(conn).unwrap();
        assert!(!store.task(1).unwrap().human);

        let junk = store
            .conn
            .execute("UPDATE tasks SET human = 2 WHERE id = 1", []);
        assert!(junk.is_err(), "the CHECK must reject values outside 0/1");
    }

    #[test]
    fn set_summary_appends_a_superseding_summary_event() {
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "summarise me".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();

        // a running task may record its account before `done`
        s.apply(t.id, Action::Start).unwrap();
        let updated = s.set_summary(t.id, "  early account  ").unwrap();
        assert_eq!(updated.state, TaskState::Running);
        assert_eq!(
            s.latest_summary(t.id).unwrap().as_deref(),
            Some("early account")
        );

        // in review, a new summary supersedes the done-time one
        s.apply(t.id, Action::Complete(Some("done-time".into())))
            .unwrap();
        let updated = s.set_summary(t.id, "amended for the PR body").unwrap();
        assert_eq!(updated.state, TaskState::Review);
        assert_eq!(
            s.latest_summary(t.id).unwrap().as_deref(),
            Some("amended for the PR body")
        );

        // every account stays on the append-only log
        let events = s.events_for(t.id).unwrap();
        let summaries = events.iter().filter(|e| e.kind == "summary").count();
        assert_eq!(summaries, 3);
    }

    #[test]
    fn set_summary_clears_the_incomplete_report_flag() {
        use crate::transition::Action;

        // The SessionEnd-fallback shape: review with a branch and no summary.
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "half a report".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        s.apply(t.id, Action::Start).unwrap();
        s.apply(t.id, Action::Complete(None)).unwrap();
        s.set_branch(t.id, Some("feat/x")).unwrap();
        assert!(s.incomplete_report_flag(t.id).unwrap());

        s.set_summary(t.id, "the missing half").unwrap();
        assert!(!s.incomplete_report_flag(t.id).unwrap());
    }

    #[test]
    fn set_summary_is_refused_outside_running_and_review() {
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "not yet".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        let err = s.set_summary(t.id, "too early").unwrap_err();
        assert!(err.to_string().contains("ready"), "{err}");

        s.apply(t.id, Action::Start).unwrap();
        s.apply(t.id, Action::Complete(None)).unwrap();
        s.apply(t.id, Action::Accept).unwrap();
        let err = s.set_summary(t.id, "too late").unwrap_err();
        assert!(err.to_string().contains("done"), "{err}");

        // and never with nothing to record, nor for a missing task
        assert!(s.set_summary(t.id, "   ").is_err());
        assert!(matches!(
            s.set_summary(999, "x"),
            Err(Error::TaskNotFound(999))
        ));
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
                human: false,
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
                s.apply(id, Action::Complete(None)).unwrap();
                id
            }
            Stalled => {
                let id = create(s, Ready);
                let (_, session) = s.record_dispatch(id, "claude", Some(1), None).unwrap();
                s.reconcile_session(session.id, false, false).unwrap();
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

    /// Migration 0006 must dedupe a task that already carries several open
    /// sessions (the duplicate-rows bug) — keeping the newest open and closing
    /// the rest — before it can create the one-open-session index, and the
    /// index must then reject any further second open row.
    #[test]
    fn migration_0006_dedupes_open_sessions_and_enforces_the_index() {
        let conn = Connection::open_in_memory().unwrap();
        // apply 0001..=0005, i.e. everything before the invariant migration
        for sql in &MIGRATIONS[..5] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 5).unwrap();
        conn.execute("INSERT INTO projects (name, path) VALUES ('p', '/tmp')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (project_id, title, state, state_since, created_at)
             VALUES (1, 'run me', 'running', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        // three open sessions on the one task — the exact duplicate state
        for _ in 0..3 {
            conn.execute(
                "INSERT INTO sessions (task_id, agent, started_at) VALUES (1, 'a', datetime('now'))",
                [],
            )
            .unwrap();
        }

        let store = Store::from_connection(conn).unwrap();
        // only the newest open session survives; the rest are closed `aborted`
        let open: Vec<i64> = store
            .sessions_for(1)
            .unwrap()
            .into_iter()
            .filter(|s| s.ended_at.is_none())
            .map(|s| s.id)
            .collect();
        assert_eq!(open, vec![3]);
        assert_eq!(
            store.session(1).unwrap().outcome,
            Some(SessionOutcome::Aborted)
        );
        // and the index now forbids a second open row
        let second = store.conn.execute(
            "INSERT INTO sessions (task_id, agent, started_at) VALUES (1, 'b', datetime('now'))",
            [],
        );
        assert!(second.is_err());
    }

    /// Migration 0008 must backfill exactly the tasks the derived redispatch
    /// flag used to mark — `ready` with a most recent session ended
    /// `failed`/`capped` — into `stalled`, leaving every other shape alone,
    /// and must carry 0007's `human` column through the table rebuild.
    #[test]
    fn migration_0008_backfills_flagged_ready_tasks_to_stalled() {
        let conn = Connection::open_in_memory().unwrap();
        for sql in &MIGRATIONS[..7] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", 7).unwrap();
        conn.execute("INSERT INTO projects (name, path) VALUES ('p', '/tmp')", [])
            .unwrap();
        // 1: ready, last session failed          -> stalled
        // 2: ready, last session capped          -> stalled
        // 3: ready, last session aborted         -> stays ready
        // 4: ready, failed session then aborted  -> stays ready (latest wins)
        // 5: ready, no sessions                  -> stays ready
        // 6: running, last session failed        -> stays running
        conn.execute(
            "INSERT INTO tasks (project_id, title, state, state_since, created_at)
             VALUES (1, 't1', 'ready', datetime('now'), datetime('now')),
                    (1, 't2', 'ready', datetime('now'), datetime('now')),
                    (1, 't3', 'ready', datetime('now'), datetime('now')),
                    (1, 't4', 'ready', datetime('now'), datetime('now')),
                    (1, 't5', 'ready', datetime('now'), datetime('now')),
                    (1, 't6', 'running', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (task_id, agent, started_at, ended_at, outcome)
             VALUES (1, 'a', datetime('now'), datetime('now'), 'failed'),
                    (2, 'a', datetime('now'), datetime('now'), 'capped'),
                    (3, 'a', datetime('now'), datetime('now'), 'aborted'),
                    (4, 'a', datetime('now'), datetime('now'), 'failed'),
                    (4, 'a', datetime('now'), datetime('now'), 'aborted'),
                    (6, 'a', datetime('now'), datetime('now'), 'failed')",
            [],
        )
        .unwrap();
        conn.execute("UPDATE tasks SET human = 1 WHERE id = 5", [])
            .unwrap();

        let store = Store::from_connection(conn).unwrap();
        assert_eq!(store.task(1).unwrap().state, TaskState::Stalled);
        assert_eq!(store.task(2).unwrap().state, TaskState::Stalled);
        assert_eq!(store.task(3).unwrap().state, TaskState::Ready);
        assert_eq!(store.task(4).unwrap().state, TaskState::Ready);
        assert_eq!(store.task(5).unwrap().state, TaskState::Ready);
        assert_eq!(store.task(6).unwrap().state, TaskState::Running);
        // the rebuild carries the human flag and its CHECK across
        assert!(store.task(5).unwrap().human);
        assert!(!store.task(1).unwrap().human);
        let junk = store
            .conn
            .execute("UPDATE tasks SET human = 2 WHERE id = 5", []);
        assert!(junk.is_err(), "the CHECK must reject values outside 0/1");
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
                human: false,
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
    fn latest_summary_returns_the_newest_summary_event() {
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "summary me".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        // no summary yet
        assert_eq!(s.latest_summary(t.id).unwrap(), None);

        s.apply(t.id, Action::Start).unwrap();
        s.apply(t.id, Action::Complete(Some("first pass".into())))
            .unwrap();
        assert_eq!(
            s.latest_summary(t.id).unwrap().as_deref(),
            Some("first pass")
        );

        // a reject-then-redo records a second summary; the newest wins
        s.apply(t.id, Action::RejectWork("redo".into())).unwrap();
        s.apply(t.id, Action::Complete(Some("second pass".into())))
            .unwrap();
        assert_eq!(
            s.latest_summary(t.id).unwrap().as_deref(),
            Some("second pass")
        );
    }

    #[test]
    fn incomplete_report_flag_marks_a_review_task_missing_one_half() {
        use crate::transition::Action;

        // Helper: a fresh task carried to `review` with the given branch/summary.
        fn reviewed(branch: Option<&str>, summary: Option<&str>) -> (Store, i64) {
            let mut s = Store::open_in_memory().unwrap();
            let p = s.create_project("voro", "/tmp/voro").unwrap();
            let t = s
                .create_task(NewTask {
                    project_id: p.id,
                    title: "report me".into(),
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                })
                .unwrap();
            s.apply(t.id, Action::Start).unwrap();
            s.apply(t.id, Action::Complete(summary.map(str::to_string)))
                .unwrap();
            if let Some(name) = branch {
                s.set_branch(t.id, Some(name)).unwrap();
            }
            (s, t.id)
        }

        // Branch but no summary — the classic forgotten-summary flake and the
        // shape the SessionEnd fallback leaves behind.
        let (s, id) = reviewed(Some("feat/x"), None);
        assert!(s.incomplete_report_flag(id).unwrap());

        // Summary but no branch — the reverse flake.
        let (s, id) = reviewed(None, Some("did the thing"));
        assert!(s.incomplete_report_flag(id).unwrap());

        // Both present — a complete report, not an anomaly.
        let (s, id) = reviewed(Some("feat/x"), Some("did the thing"));
        assert!(!s.incomplete_report_flag(id).unwrap());

        // Neither present — a legitimate no-artifact (e.g. planning) task.
        let (s, id) = reviewed(None, None);
        assert!(!s.incomplete_report_flag(id).unwrap());
    }

    #[test]
    fn incomplete_report_flag_is_gated_on_review() {
        use crate::transition::Action;

        // A partial report only counts once the task is in `review`: a running
        // task with an intended branch and no summary yet is mid-flight, not a
        // finished-but-incomplete report.
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "in flight".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        s.set_branch(t.id, Some("feat/x")).unwrap();
        assert!(!s.incomplete_report_flag(t.id).unwrap(), "ready");

        s.apply(t.id, Action::Start).unwrap();
        assert!(!s.incomplete_report_flag(t.id).unwrap(), "running");

        // Only on reaching review does the missing summary become an anomaly.
        s.apply(t.id, Action::Complete(None)).unwrap();
        assert!(s.incomplete_report_flag(t.id).unwrap(), "review");

        // Accepting past review clears it — no PR is opened from `done`.
        s.apply(t.id, Action::Accept).unwrap();
        assert!(!s.incomplete_report_flag(t.id).unwrap(), "done");
    }

    #[test]
    fn incomplete_report_flag_is_false_for_a_missing_task() {
        let s = Store::open_in_memory().unwrap();
        assert!(!s.incomplete_report_flag(999).unwrap());
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

    /// `latest_sessions` maps each task to its newest session only, and tasks
    /// with no session history stay absent.
    #[test]
    fn latest_sessions_keeps_only_the_newest_per_task() {
        let mut s = Store::open_in_memory().unwrap();
        let with_history = task_fixture(&mut s);
        let sessionless = task_fixture(&mut s);

        let first = s
            .create_session(with_history, "claude", None, Some("/var/log/first.log"))
            .unwrap();
        s.end_session(first.id, SessionOutcome::Failed).unwrap();
        let second = s
            .create_session(with_history, "codex", None, Some("/var/log/second.log"))
            .unwrap();

        let latest = s.latest_sessions().unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[&with_history].id, second.id);
        assert_eq!(
            latest[&with_history].log_path.as_deref(),
            Some("/var/log/second.log")
        );
        assert!(!latest.contains_key(&sessionless));
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

    /// A task can be `running` with no live session — started by hand, so no
    /// session was ever opened. The running strip must still surface it
    /// (DESIGN.md §9), with no session id or agent and elapsed measured from
    /// when it entered `running`.
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

    /// The strip filters on task state: a task that has left `running` —
    /// review, done, rejected — never renders, even a `review` task whose
    /// session is deliberately still open (DESIGN.md §8/§9).
    #[test]
    fn running_rows_exclude_tasks_that_left_running() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
        };

        // review keeps its session open, yet must not appear in the strip
        let review = s.create_task(new("review")).unwrap().id;
        s.record_dispatch(review, "claude", Some(1), None).unwrap();
        s.apply(review, Action::Complete(None)).unwrap();
        assert!(s.sessions_for(review).unwrap()[0].ended_at.is_none());

        // done and rejected have their sessions closed by the transition
        let done = s.create_task(new("done")).unwrap().id;
        s.record_dispatch(done, "claude", Some(2), None).unwrap();
        s.apply(done, Action::Complete(None)).unwrap();
        s.apply(done, Action::Accept).unwrap();

        let rejected = s.create_task(new("rejected")).unwrap().id;
        s.record_dispatch(rejected, "claude", Some(3), None)
            .unwrap();
        s.apply(rejected, Action::Abort).unwrap();
        s.apply(rejected, Action::Abandon).unwrap();

        // one genuinely running task remains
        let running = s.create_task(new("running")).unwrap().id;
        s.record_dispatch(running, "claude", Some(4), None).unwrap();

        let rows = s.running_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].task_id, running);
    }

    /// The direct session-25/#79 reproduction: a `done` task left carrying an
    /// open session (a lingering listing entry could otherwise keep it "alive")
    /// must stay out of the strip purely on its state.
    #[test]
    fn running_rows_ignore_a_stale_open_session_on_a_closed_task() {
        let mut s = Store::open_in_memory().unwrap();
        let task_id = task_fixture(&mut s);
        s.create_session(task_id, "claude", Some(1), None).unwrap();
        s.conn
            .execute("UPDATE tasks SET state = 'done' WHERE id = ?1", [task_id])
            .unwrap();
        assert!(s.running_rows().unwrap().is_empty());
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
    fn dep_maps_resolve_both_directions_with_title_state_and_kind() {
        use crate::model::{DepKind, DepRef, Priority};
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str| NewTask {
            project_id: p.id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
        };
        let blocker = s.create_task(new("blocker")).unwrap();
        s.apply(blocker.id, Action::Start).unwrap();
        s.apply(blocker.id, Action::Complete(None)).unwrap();
        s.apply(blocker.id, Action::Accept).unwrap();
        let source = s.create_task(new("source")).unwrap();
        let task = s.create_task(new("task")).unwrap();
        s.add_dep(task.id, blocker.id, DepKind::Blocks).unwrap();
        s.add_dep(task.id, source.id, DepKind::DiscoveredFrom)
            .unwrap();

        // Forward: the task's own deps, every kind, resolved to the
        // dependency's title and state.
        let deps = s.deps_by_task().unwrap();
        assert_eq!(
            deps[&task.id],
            vec![
                DepRef {
                    id: blocker.id,
                    title: "blocker".into(),
                    state: TaskState::Done,
                    kind: DepKind::Blocks,
                },
                DepRef {
                    id: source.id,
                    title: "source".into(),
                    state: TaskState::Ready,
                    kind: DepKind::DiscoveredFrom,
                },
            ]
        );
        assert!(!deps[&task.id][0].is_open());
        assert!(!deps.contains_key(&blocker.id));

        // Reverse: keyed by the task depended on, resolving the dependant.
        let dependents = s.dependents_by_task().unwrap();
        assert_eq!(
            dependents[&blocker.id],
            vec![DepRef {
                id: task.id,
                title: "task".into(),
                state: TaskState::Ready,
                kind: DepKind::Blocks,
            }]
        );
        assert_eq!(dependents[&source.id].len(), 1);
        assert_eq!(dependents[&source.id][0].kind, DepKind::DiscoveredFrom);
        assert!(!dependents.contains_key(&task.id));
    }
}
