//! The task state machine (DESIGN.md §6). `Store::apply` is the only path
//! that changes `tasks.state`; it validates the transition, restamps
//! `state_since`, maintains the `question`/`closed_at` invariants, appends to
//! the event log, and cascades readiness of dependant tasks — all in one
//! transaction.

use std::collections::{HashMap, VecDeque};

use rusqlite::{Connection, params};

use crate::error::{Error, Result};
use crate::model::{Session, SessionOutcome, Task, TaskState};
use crate::store::{
    Store, close_open_session, get_session, get_task, insert_session, log_event,
    set_session_outcome,
};

/// Where a `proposed` task goes at triage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Triage {
    Parked,
    Ready,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// proposed → parked | ready | rejected
    Triage(Triage),
    /// ready | stalled → running (dispatch or redispatch, or the human
    /// starting by hand)
    Start,
    /// running → needs-input; the string is the question
    Ask(String),
    /// needs-input → running; the answer is appended to the body and logged
    Answer(String),
    /// running → review; the optional string is the agent's completion summary,
    /// logged as a `summary` event so review starts from it
    Complete(Option<String>),
    /// review → done
    Accept,
    /// review → running; the string is the feedback, appended to the body
    RejectWork(String),
    /// running → ready
    Abort,
    /// ready | stalled → parked (deliberate parking)
    Park,
    /// parked → ready (manual unpark)
    Unpark,
    /// parked | ready | needs-input | review | stalled → rejected
    Abandon,
}

impl Action {
    fn name(&self) -> &'static str {
        match self {
            Action::Triage(_) => "triage",
            Action::Start => "start",
            Action::Ask(_) => "ask",
            Action::Answer(_) => "answer",
            Action::Complete(_) => "complete",
            Action::Accept => "accept",
            Action::RejectWork(_) => "reject work on",
            Action::Abort => "abort",
            Action::Park => "park",
            Action::Unpark => "unpark",
            Action::Abandon => "abandon",
        }
    }
}

impl Store {
    /// Legal target of `action` from `state`, if any. Exposed so interfaces
    /// can offer exactly the legal actions without duplicating the machine.
    /// Order matters: interfaces render these as menus with the first entry
    /// selected, so the most common action leads — for triage that is
    /// `ready`, since `parked` means blocked or deliberately parked.
    /// `human` shortens the running path (DESIGN.md §6): a human task cannot
    /// `ask` — its executor cannot be blocked on their own decision — and its
    /// completion leads, since it goes straight to `done` with no review.
    pub fn legal_actions(state: TaskState, human: bool) -> Vec<Action> {
        use TaskState::*;
        match state {
            Proposed => vec![
                Action::Triage(Triage::Ready),
                Action::Triage(Triage::Parked),
                Action::Triage(Triage::Reject),
            ],
            Parked => vec![Action::Unpark, Action::Abandon],
            Ready => vec![Action::Start, Action::Park, Action::Abandon],
            Running if human => vec![Action::Complete(None), Action::Abort],
            Running => vec![
                Action::Ask(String::new()),
                Action::Complete(None),
                Action::Abort,
            ],
            NeedsInput => vec![Action::Answer(String::new()), Action::Abandon],
            Review => vec![
                Action::Accept,
                Action::RejectWork(String::new()),
                Action::Abandon,
            ],
            Stalled => vec![Action::Start, Action::Park, Action::Abandon],
            Done | Rejected => vec![],
        }
    }

    pub fn apply(&mut self, task_id: i64, action: Action) -> Result<Task> {
        let tx = self.conn.transaction()?;
        apply_action(&tx, task_id, action)?;
        tx.commit()?;
        self.task(task_id)
    }

    /// Dispatch's atomic write (DESIGN.md §8): move the task `ready → running`
    /// (or `stalled → running` — redispatch is the whole point of that state)
    /// and open its session in one transaction, so a running task always has a
    /// session and a session always has a running task. The state change goes
    /// through the same machine as [`apply`](Store::apply); spawning the process
    /// is the caller's job and happens before this commits.
    pub fn record_dispatch(
        &mut self,
        task_id: i64,
        agent: &str,
        pid: Option<i64>,
        log_path: Option<&str>,
    ) -> Result<(Task, Session)> {
        let tx = self.conn.transaction()?;
        reject_human_dispatch(&tx, task_id)?;
        apply_action(&tx, task_id, Action::Start)?;
        let session_id = insert_session(&tx, task_id, agent, pid, log_path)?;
        tx.commit()?;
        Ok((self.task(task_id)?, self.session(session_id)?))
    }

    /// Record a continuation session (DESIGN.md §6/§8): the answer to a
    /// `needs-input` question is fed back to the work not by writing to a live
    /// pipe — the asking session has typically already exited — but by
    /// dispatching a fresh session whose prompt is the task body, now carrying
    /// the `## Answers` section `apply`'s `Answer` action just appended.
    /// Unlike [`record_dispatch`](Store::record_dispatch), which performs the
    /// `ready → running` transition itself, continuation asserts the task is
    /// *already* `running` — `answer` puts it there — so this never weakens
    /// the ready-only rule normal dispatch enforces; it only ever adds a
    /// session to a task that reached `running` through the transition API.
    pub fn record_continuation(
        &mut self,
        task_id: i64,
        agent: &str,
        pid: Option<i64>,
        log_path: Option<&str>,
    ) -> Result<(Task, Session)> {
        let tx = self.conn.transaction()?;
        let task = get_task(&tx, task_id)?.ok_or(Error::TaskNotFound(task_id))?;
        reject_human_dispatch(&tx, task_id)?;
        if task.state != TaskState::Running {
            return Err(Error::InvalidTransition {
                from: task.state,
                action: "continue".to_string(),
            });
        }
        let session_id = insert_session(&tx, task_id, agent, pid, log_path)?;
        tx.commit()?;
        Ok((self.task(task_id)?, self.session(session_id)?))
    }

    /// Reconcile an open session against its task's state (DESIGN.md §8, the
    /// observation half of dispatch). The session's life now follows the task,
    /// not the agent's process listing, so reconciliation is no longer what
    /// closes healthy sessions — the terminal transitions do that. It keeps
    /// only the one job it is good at (detecting a crash or cap mid-`running`)
    /// plus finalising a session left stranded on a task that has already
    /// closed. `pid_alive` and `likely_capped` are supplied by the caller —
    /// voro-core stays free of process and log I/O — and only matter for a
    /// `running` task:
    ///
    /// - the session already ended: no-op (`Ok(None)`), so a caller looping
    ///   over `live_sessions` repeatedly can't double-finalise one.
    /// - task `running`, `pid_alive`: still working, left untouched.
    /// - task `running`, process gone: it ended without calling `done`/`ask`.
    ///   The outcome is recorded (`capped` if `likely_capped`, else `failed`)
    ///   and the task lands `running → stalled` (DESIGN.md §6/§8) — the
    ///   attention state for a dispatch that died, which puts redispatch in
    ///   the queue with no human abort step in between. A vanished session is
    ///   still indistinguishable from a completion whose return-path call has
    ///   not landed, but `stalled` makes that misfire safe where the old
    ///   automatic `running → ready` bounce was not: a stalled task is an
    ///   attention row, never handed out by `voro next`, and a late `done`
    ///   is refused loudly rather than racing a fresh dispatch. A stalled
    ///   task with an open blocker is demoted straight to `parked`, exactly
    ///   as if it had landed `ready`.
    /// - task `needs-input`/`review`: the session stays open on purpose — it is
    ///   reused when the answer/feedback continues the work — so reconciliation
    ///   leaves it alone (`Ok(None)`) regardless of process liveness. It is
    ///   closed later by the next terminal transition or superseded by a
    ///   continuation.
    /// - task already closed (`done`/`rejected`) or otherwise off the active
    ///   path: the session is stale — the terminal transition should have
    ///   closed it — so it is finalised now (`completed` for `done`, else
    ///   `aborted`) with no event. This is what heals a legacy stranded row
    ///   (e.g. a `done` task still carrying an open session) on the next pass.
    pub fn reconcile_session(
        &mut self,
        session_id: i64,
        pid_alive: bool,
        likely_capped: bool,
    ) -> Result<Option<(Session, Task)>> {
        let tx = self.conn.transaction()?;
        let session = get_session(&tx, session_id)?.ok_or(Error::SessionNotFound(session_id))?;
        if session.ended_at.is_some() {
            return Ok(None);
        }
        let task = get_task(&tx, session.task_id)?.ok_or(Error::TaskNotFound(session.task_id))?;

        let outcome = match task.state {
            TaskState::Running => {
                if pid_alive {
                    return Ok(None);
                }
                if likely_capped {
                    SessionOutcome::Capped
                } else {
                    SessionOutcome::Failed
                }
            }
            // The session is meant to stay open here; nothing to reconcile.
            TaskState::NeedsInput | TaskState::Review => return Ok(None),
            // Stale: a session still open on a task that has left the active
            // path. Close it with the outcome that fits where the task landed.
            TaskState::Done => SessionOutcome::Completed,
            TaskState::Rejected
            | TaskState::Ready
            | TaskState::Parked
            | TaskState::Proposed
            | TaskState::Stalled => SessionOutcome::Aborted,
        };
        set_session_outcome(&tx, session_id, outcome)?;

        if task.state == TaskState::Running {
            log_event(
                &tx,
                task.id,
                "reconcile",
                Some(&format!(
                    "session {session_id} ended without reporting ({outcome})"
                )),
            )?;
            tx.execute(
                "UPDATE tasks SET state = ?1, state_since = datetime('now') WHERE id = ?2",
                params![TaskState::Stalled, task.id],
            )?;
            log_event(
                &tx,
                task.id,
                "transition",
                Some(&format!("{} -> {}", task.state, TaskState::Stalled)),
            )?;
            reconcile_readiness(&tx, task.id)?;
        }
        tx.commit()?;
        Ok(Some((
            self.session(session_id)?,
            self.task(session.task_id)?,
        )))
    }

    /// Replace the `blocks` dependencies of a task with the given set, then
    /// reconcile its readiness. This is the dep-editing entry point for
    /// interfaces; `add_dep`/`remove_dep` reconcile too.
    pub fn set_blocks_deps(&mut self, task_id: i64, depends_on: &[i64]) -> Result<Task> {
        let tx = self.conn.transaction()?;
        if get_task(&tx, task_id)?.is_none() {
            return Err(Error::TaskNotFound(task_id));
        }
        tx.execute(
            "DELETE FROM deps WHERE task_id = ?1 AND kind = 'blocks'",
            [task_id],
        )?;
        for dep in depends_on {
            if get_task(&tx, *dep)?.is_none() {
                return Err(Error::TaskNotFound(*dep));
            }
            reject_blocks_cycle(&tx, task_id, *dep)?;
            tx.execute(
                "INSERT OR IGNORE INTO deps (task_id, depends_on, kind) VALUES (?1, ?2, 'blocks')",
                params![task_id, dep],
            )?;
        }
        reconcile_readiness(&tx, task_id)?;
        tx.commit()?;
        self.task(task_id)
    }

    /// The reverse authoring direction of
    /// [`set_blocks_deps`](Store::set_blocks_deps): make `blocker_id` a
    /// blocker of each task in `dependents`. Additive and idempotent — the
    /// blocker set belongs to each dependent, so replacing it from here would
    /// silently detach edges other tasks authored. Each dependent's readiness
    /// is reconciled in the same write; the returned pairs carry the
    /// dependent's state before that reconciliation so callers can surface a
    /// demotion.
    pub fn block_tasks(
        &mut self,
        blocker_id: i64,
        dependents: &[i64],
    ) -> Result<Vec<(Task, TaskState)>> {
        let tx = self.conn.transaction()?;
        if get_task(&tx, blocker_id)?.is_none() {
            return Err(Error::TaskNotFound(blocker_id));
        }
        let mut affected = Vec::with_capacity(dependents.len());
        for dep in dependents {
            let before = get_task(&tx, *dep)?.ok_or(Error::TaskNotFound(*dep))?.state;
            reject_blocks_cycle(&tx, *dep, blocker_id)?;
            tx.execute(
                "INSERT OR IGNORE INTO deps (task_id, depends_on, kind) VALUES (?1, ?2, 'blocks')",
                params![dep, blocker_id],
            )?;
            reconcile_readiness(&tx, *dep)?;
            affected.push((*dep, before));
        }
        tx.commit()?;
        affected
            .into_iter()
            .map(|(id, before)| Ok((self.task(id)?, before)))
            .collect()
    }
}

/// The state machine proper: validate `action` against the task's current
/// state, restamp `state_since`, maintain the `question`/`closed_at`
/// invariants, append to the event log, and cascade dependant readiness — all
/// against an already-open transaction so callers can bundle further writes
/// (a session insert, for dispatch) into the same atomic unit.
fn apply_action(tx: &Connection, task_id: i64, action: Action) -> Result<TaskState> {
    let task = get_task(tx, task_id)?.ok_or(Error::TaskNotFound(task_id))?;

    use TaskState::*;
    let to = match (task.state, &action) {
        (Proposed, Action::Triage(Triage::Parked)) => Parked,
        (Proposed, Action::Triage(Triage::Ready)) => Ready,
        (Proposed, Action::Triage(Triage::Reject)) => Rejected,
        (Ready | Stalled, Action::Start) => Running,
        // A human task cannot be blocked on a decision — the executor *is* the
        // human (DESIGN.md §6). Verifying its outcome is a downstream
        // `blocks`-dependent task, not a sub-state of this one.
        (Running, Action::Ask(_)) if task.human => {
            return Err(Error::HumanTask {
                id: task_id,
                reason: "its executor is the human, who cannot be blocked on their own \
                         decision — file a follow-up task that blocks on this one instead"
                    .into(),
            });
        }
        (Running, Action::Ask(_)) => NeedsInput,
        (NeedsInput, Action::Answer(_)) => Running,
        // Completing a human task skips `review`: the human is both executor
        // and acceptor, so there is no one left to accept the work (§6).
        (Running, Action::Complete(_)) if task.human => Done,
        (Running, Action::Complete(_)) => Review,
        (Review, Action::Accept) => Done,
        (Review, Action::RejectWork(_)) => Running,
        (Running, Action::Abort) => Ready,
        (Ready | Stalled, Action::Park) => Parked,
        (Parked, Action::Unpark) => Ready,
        (Parked | Ready | NeedsInput | Review | Stalled, Action::Abandon) => Rejected,
        _ => {
            return Err(Error::InvalidTransition {
                from: task.state,
                action: action.name().to_string(),
            });
        }
    };

    match &action {
        Action::Ask(q) if q.trim().is_empty() => {
            return Err(Error::Invalid("a question is required".into()));
        }
        Action::Answer(a) if a.trim().is_empty() => {
            return Err(Error::Invalid("an answer is required".into()));
        }
        Action::RejectWork(f) if f.trim().is_empty() => {
            return Err(Error::Invalid("rejection feedback is required".into()));
        }
        _ => {}
    }
    let question = match &action {
        Action::Ask(q) => Some(q.trim().to_string()),
        _ => None,
    };

    tx.execute(
        "UPDATE tasks SET state = ?1, state_since = datetime('now'), question = ?2,
                closed_at = CASE WHEN ?3 THEN datetime('now') ELSE closed_at END
         WHERE id = ?4",
        params![to, question, to.is_terminal(), task_id],
    )?;
    log_event(
        tx,
        task_id,
        "transition",
        Some(&format!("{} -> {}", task.state, to)),
    )?;

    match &action {
        Action::Answer(a) => {
            append_section(tx, task_id, "Answers", a.trim())?;
            log_event(tx, task_id, "answer", Some(a.trim()))?;
        }
        Action::RejectWork(f) => {
            append_section(tx, task_id, "Feedback", f.trim())?;
            log_event(tx, task_id, "feedback", Some(f.trim()))?;
        }
        Action::Complete(Some(s)) if !s.trim().is_empty() => {
            log_event(tx, task_id, "summary", Some(s.trim()))?;
        }
        _ => {}
    }

    // The session's life follows the task (DESIGN.md §8): the terminal
    // transitions tear down the running work, so they close the task's open
    // session in the same transaction. `Accept` completed the work; `Abort`
    // and `Abandon` threw it away. `Ask`/`Complete`/`Answer`/`RejectWork`
    // deliberately leave the session open — it is reused across
    // needs-input/review and superseded by the next continuation.
    match &action {
        Action::Accept => {
            close_open_session(tx, task_id, SessionOutcome::Completed)?;
        }
        // A human task's completion is itself terminal (running → done), so it
        // owns the teardown `Accept` would otherwise perform. No agent session
        // should exist on a human task, but the belt-and-braces close keeps
        // the sessions-follow-the-task rule total.
        Action::Complete(_) if to == TaskState::Done => {
            close_open_session(tx, task_id, SessionOutcome::Completed)?;
        }
        Action::Abort | Action::Abandon => {
            close_open_session(tx, task_id, SessionOutcome::Aborted)?;
        }
        _ => {}
    }

    if to.is_terminal() {
        reconcile_dependants(tx, task_id)?;
    } else if to == TaskState::Ready {
        // `ready` must mean genuinely actionable: a transition landing here
        // while a blocker is still open (triage, abort, unpark) reconciles
        // straight back to `parked` so the scheduler never surfaces it.
        reconcile_readiness(tx, task_id)?;
    }

    Ok(to)
}

/// Refuse to open an agent session on a human-only task (DESIGN.md §6/§8):
/// dispatch, redispatch, and continuation all route through here, before any
/// state change or session insert, so the refusal writes nothing.
fn reject_human_dispatch(tx: &Connection, task_id: i64) -> Result<()> {
    let task = get_task(tx, task_id)?.ok_or(Error::TaskNotFound(task_id))?;
    if task.human {
        return Err(Error::HumanTask {
            id: task_id,
            reason: "no agent can execute it — start it by hand instead".into(),
        });
    }
    Ok(())
}

/// Reject `from` acquiring a `blocks` dependency on `to` if doing so would
/// close a cycle in the `blocks` graph (a task blocking itself, directly or
/// transitively). Called by every write path that adds a `blocks` edge.
pub(crate) fn reject_blocks_cycle(conn: &Connection, from: i64, to: i64) -> Result<()> {
    if let Some(cycle) = find_blocks_cycle(conn, from, to)? {
        let path = cycle
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(" -> ");
        return Err(Error::DependencyCycle(path));
    }
    Ok(())
}

/// Would adding the edge `from` --blocks--> `to` close a cycle? Equivalent to
/// asking whether `to` can already reach `from` by following existing
/// `blocks` edges (`task_id -> depends_on`) — if so, the new edge would let
/// `from` walk out to `to` and back to itself. Self-deps are the degenerate
/// case where `from == to`, a zero-hop cycle. Returns the cycle in task-id
/// order starting and ending at `from`, e.g. `[3, 7, 3]`.
fn find_blocks_cycle(conn: &Connection, from: i64, to: i64) -> Result<Option<Vec<i64>>> {
    if from == to {
        return Ok(Some(vec![from, from]));
    }

    // BFS outward from `to`, following `blocks` edges, recording each node's
    // predecessor so a path back to `to` can be rebuilt if `from` turns up.
    let mut predecessor: HashMap<i64, i64> = HashMap::new();
    let mut queue = VecDeque::new();
    queue.push_back(to);

    while let Some(node) = queue.pop_front() {
        if node == from {
            let mut path = vec![node];
            let mut cur = node;
            while cur != to {
                cur = predecessor[&cur];
                path.push(cur);
            }
            path.reverse(); // now to -> ... -> from
            let mut cycle = vec![from];
            cycle.extend(path);
            return Ok(Some(cycle));
        }
        let mut stmt = conn
            .prepare_cached("SELECT depends_on FROM deps WHERE task_id = ?1 AND kind = 'blocks'")?;
        let children: Vec<i64> = stmt
            .query_map([node], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for child in children {
            if child != to && !predecessor.contains_key(&child) {
                predecessor.insert(child, node);
                queue.push_back(child);
            }
        }
    }
    Ok(None)
}

/// Append `text` under a `## {heading}` section at the end of the body,
/// creating the section on first use.
fn append_section(conn: &Connection, task_id: i64, heading: &str, text: &str) -> Result<()> {
    let body: String = conn.query_row("SELECT body FROM tasks WHERE id = ?1", [task_id], |r| {
        r.get(0)
    })?;
    let marker = format!("## {heading}");
    let mut body = body.trim_end().to_string();
    if !body.ends_with(&marker) && !body.contains(&format!("{marker}\n")) {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&marker);
        body.push('\n');
    }
    body.push_str(&format!("\n- {text}\n"));
    conn.execute(
        "UPDATE tasks SET body = ?1 WHERE id = ?2",
        params![body, task_id],
    )?;
    Ok(())
}

/// After `closed_id` reaches a terminal state, re-check every task that
/// `blocks`-depends on it (DESIGN.md §5: promotion happens the moment the
/// last blocker closes).
fn reconcile_dependants(conn: &Connection, closed_id: i64) -> Result<()> {
    let mut stmt =
        conn.prepare("SELECT task_id FROM deps WHERE depends_on = ?1 AND kind = 'blocks'")?;
    let dependants: Vec<i64> = stmt
        .query_map([closed_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in dependants {
        reconcile_readiness(conn, id)?;
    }
    Ok(())
}

/// Enforce readiness against `blocks` dependencies:
/// - `parked` with at least one blocker, all closed → promote to `ready`.
///   A parked task with *no* blockers is deliberately parked and stays put.
/// - `ready` or `stalled` with an open blocker → demote to `parked`. A
///   stalled task re-promotes to `ready`, not `stalled`, when the blocker
///   closes — by then the stall context is stale (DESIGN.md §6).
pub(crate) fn reconcile_readiness(conn: &Connection, task_id: i64) -> Result<()> {
    let Some(task) = get_task(conn, task_id)? else {
        return Ok(());
    };
    let (total, open): (i64, i64) = conn.query_row(
        "SELECT COUNT(*),
                COUNT(*) FILTER (WHERE b.state NOT IN ('done','rejected'))
         FROM deps d JOIN tasks b ON b.id = d.depends_on
         WHERE d.task_id = ?1 AND d.kind = 'blocks'",
        [task_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let to = match task.state {
        TaskState::Parked if total > 0 && open == 0 => TaskState::Ready,
        TaskState::Ready | TaskState::Stalled if open > 0 => TaskState::Parked,
        _ => return Ok(()),
    };
    conn.execute(
        "UPDATE tasks SET state = ?1, state_since = datetime('now') WHERE id = ?2",
        params![to, task_id],
    )?;
    let reason = if to == TaskState::Ready {
        "unblocked"
    } else {
        "blocked"
    };
    log_event(
        conn,
        task_id,
        "transition",
        Some(&format!("{} -> {} ({reason})", task.state, to)),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DepKind, Priority};
    use crate::store::NewTask;

    fn store_with_project() -> (Store, i64) {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/proj").unwrap();
        (s, p.id)
    }

    fn create(s: &mut Store, project_id: i64, state: TaskState) -> i64 {
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
    }

    /// Walk a fresh task into `state` through the transition API itself.
    fn task_in_state(s: &mut Store, project_id: i64, state: TaskState) -> i64 {
        use TaskState::*;
        match state {
            Proposed | Parked | Ready => create(s, project_id, state),
            Running => {
                let id = create(s, project_id, Ready);
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
                let id = create(s, project_id, Ready);
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
                let id = create(s, project_id, Proposed);
                s.apply(id, Action::Triage(Triage::Reject)).unwrap();
                id
            }
        }
    }

    fn all_actions() -> Vec<Action> {
        vec![
            Action::Triage(Triage::Parked),
            Action::Triage(Triage::Ready),
            Action::Triage(Triage::Reject),
            Action::Start,
            Action::Ask("q?".into()),
            Action::Answer("a.".into()),
            Action::Complete(None),
            Action::Accept,
            Action::RejectWork("redo".into()),
            Action::Abort,
            Action::Park,
            Action::Unpark,
            Action::Abandon,
        ]
    }

    /// The full §6 matrix: expected target state for every (state, action)
    /// pair, `None` meaning the transition is illegal.
    fn expected(state: TaskState, action: &Action) -> Option<TaskState> {
        use TaskState::*;
        match (state, action) {
            (Proposed, Action::Triage(Triage::Parked)) => Some(Parked),
            (Proposed, Action::Triage(Triage::Ready)) => Some(Ready),
            (Proposed, Action::Triage(Triage::Reject)) => Some(Rejected),
            (Ready | Stalled, Action::Start) => Some(Running),
            (Ready | Stalled, Action::Park) => Some(Parked),
            (Parked, Action::Unpark) => Some(Ready),
            (Running, Action::Ask(_)) => Some(NeedsInput),
            (Running, Action::Complete(_)) => Some(Review),
            (Running, Action::Abort) => Some(Ready),
            (NeedsInput, Action::Answer(_)) => Some(Running),
            (Review, Action::Accept) => Some(Done),
            (Review, Action::RejectWork(_)) => Some(Running),
            (Parked | Ready | NeedsInput | Review | Stalled, Action::Abandon) => Some(Rejected),
            _ => None,
        }
    }

    #[test]
    fn full_transition_matrix() {
        for state in TaskState::ALL {
            for action in all_actions() {
                let (mut s, p) = store_with_project();
                let id = task_in_state(&mut s, p, state);
                let result = s.apply(id, action.clone());
                match expected(state, &action) {
                    Some(to) => {
                        let task = result.unwrap_or_else(|e| {
                            panic!("{state} + {action:?} should reach {to}: {e}")
                        });
                        assert_eq!(task.state, to, "{state} + {action:?}");
                    }
                    None => {
                        assert!(
                            matches!(result, Err(Error::InvalidTransition { .. })),
                            "{state} + {action:?} should be rejected"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn legal_actions_agrees_with_apply() {
        for state in TaskState::ALL {
            let legal = Store::legal_actions(state, false);
            for action in all_actions() {
                let in_legal = legal
                    .iter()
                    .any(|l| std::mem::discriminant(l) == std::mem::discriminant(&action))
                    && match (&action, state) {
                        // Triage variants share a discriminant; all are legal
                        // exactly when the state is proposed.
                        (Action::Triage(_), s) => s == TaskState::Proposed,
                        _ => true,
                    };
                assert_eq!(
                    expected(state, &action).is_some(),
                    in_legal,
                    "legal_actions disagrees for {state} + {action:?}"
                );
            }
        }
    }

    // --- human-only tasks (DESIGN.md §3/§6): the shortened path ---

    mod human {
        use super::*;

        fn create_human(s: &mut Store, project_id: i64, state: TaskState) -> i64 {
            s.create_task(NewTask {
                project_id,
                title: format!("human task in {state}"),
                body: String::new(),
                priority: Priority::P1,
                state,
                agent: None,
                human: true,
            })
            .unwrap()
            .id
        }

        #[test]
        fn completion_goes_straight_to_done() {
            let (mut s, p) = store_with_project();
            let id = create_human(&mut s, p, TaskState::Ready);
            s.apply(id, Action::Start).unwrap();

            let task = s
                .apply(id, Action::Complete(Some("bag captured".into())))
                .unwrap();
            assert_eq!(task.state, TaskState::Done);
            assert!(task.closed_at.is_some());

            let events = s.events_for(id).unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| e.detail.as_deref() == Some("running -> done")),
                "{events:?}"
            );
            // the summary still rides the completion, as on the agent path
            assert!(
                events
                    .iter()
                    .any(|e| e.kind == "summary" && e.detail.as_deref() == Some("bag captured"))
            );
        }

        #[test]
        fn ask_is_refused_and_writes_nothing() {
            let (mut s, p) = store_with_project();
            let id = create_human(&mut s, p, TaskState::Ready);
            s.apply(id, Action::Start).unwrap();

            let err = s.apply(id, Action::Ask("which bag?".into())).unwrap_err();
            assert!(
                matches!(err, Error::HumanTask { id: e, .. } if e == id),
                "expected a human-only refusal, got {err}"
            );
            let task = s.task(id).unwrap();
            assert_eq!(task.state, TaskState::Running);
            assert!(task.question.is_none());
        }

        #[test]
        fn record_dispatch_is_refused_and_writes_nothing() {
            let (mut s, p) = store_with_project();
            let id = create_human(&mut s, p, TaskState::Ready);

            let err = s.record_dispatch(id, "claude", Some(1), None).unwrap_err();
            assert!(
                matches!(err, Error::HumanTask { id: e, .. } if e == id),
                "expected a human-only refusal, got {err}"
            );
            assert_eq!(s.task(id).unwrap().state, TaskState::Ready);
            assert!(s.sessions_for(id).unwrap().is_empty());
        }

        #[test]
        fn record_continuation_is_refused() {
            let (mut s, p) = store_with_project();
            let id = create_human(&mut s, p, TaskState::Ready);
            s.apply(id, Action::Start).unwrap();

            let err = s.record_continuation(id, "claude", None, None).unwrap_err();
            assert!(
                matches!(err, Error::HumanTask { id: e, .. } if e == id),
                "expected a human-only refusal, got {err}"
            );
            assert!(s.sessions_for(id).unwrap().is_empty());
        }

        #[test]
        fn completion_unblocks_dependants() {
            // running → done is terminal, so it must cascade readiness exactly
            // as an accept does.
            let (mut s, p) = store_with_project();
            let blocker = create_human(&mut s, p, TaskState::Ready);
            let dependant = create(&mut s, p, TaskState::Parked);
            s.add_dep(dependant, blocker, DepKind::Blocks).unwrap();

            s.apply(blocker, Action::Start).unwrap();
            s.apply(blocker, Action::Complete(None)).unwrap();
            assert_eq!(s.task(dependant).unwrap().state, TaskState::Ready);
        }

        #[test]
        fn completion_closes_a_stray_open_session() {
            // No agent session should ever exist on a human task, but the
            // terminal completion still tears one down (sessions follow the
            // task, §8) if a legacy or hand-made row is lying around.
            let (mut s, p) = store_with_project();
            let id = create_human(&mut s, p, TaskState::Ready);
            s.apply(id, Action::Start).unwrap();
            let stray = s.create_session(id, "claude", Some(1), None).unwrap();

            s.apply(id, Action::Complete(None)).unwrap();
            let closed = s.session(stray.id).unwrap();
            assert!(closed.ended_at.is_some());
            assert_eq!(closed.outcome, Some(SessionOutcome::Completed));
        }

        #[test]
        fn legal_actions_omit_ask_and_agree_with_apply() {
            let legal = Store::legal_actions(TaskState::Running, true);
            assert_eq!(legal, vec![Action::Complete(None), Action::Abort]);
            // every other state offers the same menu regardless of the flag
            for state in TaskState::ALL {
                if state != TaskState::Running {
                    assert_eq!(
                        Store::legal_actions(state, true),
                        Store::legal_actions(state, false),
                        "{state}"
                    );
                }
            }
        }

        /// The matrix over the states a human task can actually reach —
        /// `needs-input` and `review` are unreachable by construction (§6).
        #[test]
        fn full_transition_matrix_for_human_tasks() {
            use TaskState::*;
            for state in [Proposed, Parked, Ready, Running] {
                for action in all_actions() {
                    let (mut s, p) = store_with_project();
                    let id = create_human(&mut s, p, if state == Running { Ready } else { state });
                    if state == Running {
                        s.apply(id, Action::Start).unwrap();
                    }
                    let result = s.apply(id, action.clone());
                    let expected = match (state, &action) {
                        // the two divergences from the agent path
                        (Running, Action::Ask(_)) => None,
                        (Running, Action::Complete(_)) => Some(Done),
                        _ => expected(state, &action),
                    };
                    match expected {
                        Some(to) => {
                            let task = result.unwrap_or_else(|e| {
                                panic!("human {state} + {action:?} should reach {to}: {e}")
                            });
                            assert_eq!(task.state, to, "human {state} + {action:?}");
                        }
                        None => {
                            assert!(
                                matches!(
                                    result,
                                    Err(Error::InvalidTransition { .. } | Error::HumanTask { .. })
                                ),
                                "human {state} + {action:?} should be rejected"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn transitions_restamp_state_since_and_log_events() {
        let (mut s, p) = store_with_project();
        let id = create(&mut s, p, TaskState::Ready);
        s.conn
            .execute(
                "UPDATE tasks SET state_since = '2000-01-01 00:00:00' WHERE id = ?1",
                [id],
            )
            .unwrap();
        let task = s.apply(id, Action::Start).unwrap();
        assert_ne!(task.state_since, "2000-01-01 00:00:00");
        let events = s.events_for(id).unwrap();
        assert_eq!(events.last().unwrap().kind, "transition");
        assert_eq!(
            events.last().unwrap().detail.as_deref(),
            Some("ready -> running")
        );
    }

    #[test]
    fn question_is_set_iff_needs_input() {
        let (mut s, p) = store_with_project();
        let id = task_in_state(&mut s, p, TaskState::Running);
        let task = s.apply(id, Action::Ask("  A or B?  ".into())).unwrap();
        assert_eq!(task.question.as_deref(), Some("A or B?"));
        let task = s.apply(id, Action::Answer("B".into())).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.question.is_none());

        let id = task_in_state(&mut s, p, TaskState::NeedsInput);
        let task = s.apply(id, Action::Abandon).unwrap();
        assert!(task.question.is_none());
    }

    #[test]
    fn empty_question_answer_feedback_are_rejected() {
        let (mut s, p) = store_with_project();
        let running = task_in_state(&mut s, p, TaskState::Running);
        assert!(s.apply(running, Action::Ask("  ".into())).is_err());
        let waiting = task_in_state(&mut s, p, TaskState::NeedsInput);
        assert!(s.apply(waiting, Action::Answer("".into())).is_err());
        let review = task_in_state(&mut s, p, TaskState::Review);
        assert!(s.apply(review, Action::RejectWork(" ".into())).is_err());
        // failed applies must not have changed anything
        assert_eq!(s.task(waiting).unwrap().state, TaskState::NeedsInput);
    }

    #[test]
    fn answers_and_feedback_accumulate_in_body() {
        let (mut s, p) = store_with_project();
        let id = task_in_state(&mut s, p, TaskState::NeedsInput);
        let task = s.apply(id, Action::Answer("Schema B".into())).unwrap();
        assert!(task.body.contains("## Answers"));
        assert!(task.body.contains("- Schema B"));

        s.apply(id, Action::Ask("and the index?".into())).unwrap();
        let task = s.apply(id, Action::Answer("covering".into())).unwrap();
        assert_eq!(task.body.matches("## Answers").count(), 1);
        assert!(task.body.contains("- covering"));

        s.apply(id, Action::Complete(None)).unwrap();
        let task = s
            .apply(id, Action::RejectWork("tests missing".into()))
            .unwrap();
        assert!(task.body.contains("## Feedback"));
        assert!(task.body.contains("- tests missing"));
        assert_eq!(
            s.events_for(id)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "answer")
                .count(),
            2
        );
    }

    #[test]
    fn complete_summary_is_logged_as_an_event() {
        let (mut s, p) = store_with_project();
        let id = task_in_state(&mut s, p, TaskState::Running);
        s.apply(
            id,
            Action::Complete(Some("Implemented X, tests pass".into())),
        )
        .unwrap();
        let summaries: Vec<_> = s
            .events_for(id)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "summary")
            .collect();
        assert_eq!(summaries.len(), 1);
        assert_eq!(
            summaries[0].detail.as_deref(),
            Some("Implemented X, tests pass")
        );
    }

    #[test]
    fn complete_without_summary_logs_no_summary_event() {
        let (mut s, p) = store_with_project();
        let id = task_in_state(&mut s, p, TaskState::Running);
        s.apply(id, Action::Complete(None)).unwrap();
        let blank = task_in_state(&mut s, p, TaskState::Running);
        s.apply(blank, Action::Complete(Some("   ".into())))
            .unwrap();
        assert!(
            s.events_for(id)
                .unwrap()
                .iter()
                .all(|e| e.kind != "summary")
        );
        assert!(
            s.events_for(blank)
                .unwrap()
                .iter()
                .all(|e| e.kind != "summary")
        );
    }

    #[test]
    fn closing_stamps_closed_at() {
        let (mut s, p) = store_with_project();
        let id = task_in_state(&mut s, p, TaskState::Done);
        assert!(s.task(id).unwrap().closed_at.is_some());
        let id = task_in_state(&mut s, p, TaskState::Rejected);
        assert!(s.task(id).unwrap().closed_at.is_some());
        let id = task_in_state(&mut s, p, TaskState::Review);
        assert!(s.task(id).unwrap().closed_at.is_none());
    }

    #[test]
    fn accepting_last_blocker_promotes_dependant() {
        let (mut s, p) = store_with_project();
        let blocker = task_in_state(&mut s, p, TaskState::Review);
        let dependant = create(&mut s, p, TaskState::Parked);
        s.add_dep(dependant, blocker, DepKind::Blocks).unwrap();

        s.apply(blocker, Action::Accept).unwrap();
        let task = s.task(dependant).unwrap();
        assert_eq!(task.state, TaskState::Ready);
        let events = s.events_for(dependant).unwrap();
        assert_eq!(
            events.last().unwrap().detail.as_deref(),
            Some("parked -> ready (unblocked)")
        );
    }

    #[test]
    fn promotion_waits_for_the_last_blocker() {
        let (mut s, p) = store_with_project();
        let b1 = task_in_state(&mut s, p, TaskState::Review);
        let b2 = create(&mut s, p, TaskState::Ready);
        let dependant = create(&mut s, p, TaskState::Parked);
        s.add_dep(dependant, b1, DepKind::Blocks).unwrap();
        s.add_dep(dependant, b2, DepKind::Blocks).unwrap();

        s.apply(b1, Action::Accept).unwrap();
        assert_eq!(s.task(dependant).unwrap().state, TaskState::Parked);

        // a rejected blocker no longer blocks either
        s.apply(b2, Action::Abandon).unwrap();
        assert_eq!(s.task(dependant).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn parked_task_without_blockers_is_never_auto_promoted() {
        let (mut s, p) = store_with_project();
        let parked = create(&mut s, p, TaskState::Parked);
        let unrelated = task_in_state(&mut s, p, TaskState::Review);
        s.apply(unrelated, Action::Accept).unwrap();
        assert_eq!(s.task(parked).unwrap().state, TaskState::Parked);
    }

    #[test]
    fn only_blocks_deps_gate_readiness() {
        let (mut s, p) = store_with_project();
        let other = create(&mut s, p, TaskState::Ready);
        let task = create(&mut s, p, TaskState::Ready);
        for kind in [DepKind::DiscoveredFrom, DepKind::Parent, DepKind::Related] {
            s.add_dep(task, other, kind).unwrap_or_default();
        }
        assert_eq!(s.task(task).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn adding_open_blocker_demotes_ready_task() {
        let (mut s, p) = store_with_project();
        let blocker = create(&mut s, p, TaskState::Ready);
        let task = create(&mut s, p, TaskState::Ready);
        s.add_dep(task, blocker, DepKind::Blocks).unwrap();
        let demoted = s.task(task).unwrap();
        assert_eq!(demoted.state, TaskState::Parked);

        // ...and closing that blocker brings it straight back.
        s.apply(blocker, Action::Start).unwrap();
        s.apply(blocker, Action::Complete(None)).unwrap();
        s.apply(blocker, Action::Accept).unwrap();
        assert_eq!(s.task(task).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn triaging_to_ready_parks_a_blocked_task() {
        let (mut s, p) = store_with_project();
        let blocker = create(&mut s, p, TaskState::Ready);
        let task = create(&mut s, p, TaskState::Proposed);
        s.add_dep(task, blocker, DepKind::Blocks).unwrap();

        // The human triages it "ready", but an open blocker overrides that: it
        // lands in parked and never reaches the scheduler.
        let triaged = s.apply(task, Action::Triage(Triage::Ready)).unwrap();
        assert_eq!(triaged.state, TaskState::Parked);

        // closing the blocker auto-promotes it exactly like any parked task
        s.apply(blocker, Action::Start).unwrap();
        s.apply(blocker, Action::Complete(None)).unwrap();
        s.apply(blocker, Action::Accept).unwrap();
        assert_eq!(s.task(task).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn triaging_to_ready_stays_ready_when_unblocked() {
        let (mut s, p) = store_with_project();
        let closed = task_in_state(&mut s, p, TaskState::Done);
        let task = create(&mut s, p, TaskState::Proposed);
        s.add_dep(task, closed, DepKind::Blocks).unwrap();
        // a closed blocker does not gate readiness
        let triaged = s.apply(task, Action::Triage(Triage::Ready)).unwrap();
        assert_eq!(triaged.state, TaskState::Ready);
    }

    #[test]
    fn aborting_parks_a_task_blocked_while_running() {
        let (mut s, p) = store_with_project();
        let task = task_in_state(&mut s, p, TaskState::Running);
        let blocker = create(&mut s, p, TaskState::Ready);
        // adding a blocker to a running task leaves it running...
        s.add_dep(task, blocker, DepKind::Blocks).unwrap();
        assert_eq!(s.task(task).unwrap().state, TaskState::Running);
        // ...but aborting must not expose it as ready while still blocked
        let aborted = s.apply(task, Action::Abort).unwrap();
        assert_eq!(aborted.state, TaskState::Parked);
    }

    #[test]
    fn unparking_a_blocked_task_reparks_it() {
        let (mut s, p) = store_with_project();
        let blocker = create(&mut s, p, TaskState::Ready);
        let task = create(&mut s, p, TaskState::Ready);
        s.add_dep(task, blocker, DepKind::Blocks).unwrap();
        assert_eq!(s.task(task).unwrap().state, TaskState::Parked);
        // a manual unpark cannot override an open blocker
        let unparked = s.apply(task, Action::Unpark).unwrap();
        assert_eq!(unparked.state, TaskState::Parked);
    }

    #[test]
    fn set_blocks_deps_replaces_and_reconciles() {
        let (mut s, p) = store_with_project();
        let open = create(&mut s, p, TaskState::Ready);
        let closed = task_in_state(&mut s, p, TaskState::Done);
        let task = create(&mut s, p, TaskState::Ready);

        let t = s.set_blocks_deps(task, &[open, closed]).unwrap();
        assert_eq!(t.state, TaskState::Parked);

        // dropping the open blocker (one closed dep remains) promotes
        let t = s.set_blocks_deps(task, &[closed]).unwrap();
        assert_eq!(t.state, TaskState::Ready);

        assert!(s.set_blocks_deps(task, &[task]).is_err());
        assert!(s.set_blocks_deps(task, &[9999]).is_err());
    }

    #[test]
    fn block_tasks_demotes_ready_dependents_in_the_same_write() {
        let (mut s, p) = store_with_project();
        let blocker = create(&mut s, p, TaskState::Ready);
        let ready = create(&mut s, p, TaskState::Ready);
        let parked = create(&mut s, p, TaskState::Parked);

        let affected = s.block_tasks(blocker, &[ready, parked]).unwrap();
        let states: Vec<_> = affected
            .iter()
            .map(|(t, before)| (t.id, *before, t.state))
            .collect();
        assert_eq!(
            states,
            vec![
                (ready, TaskState::Ready, TaskState::Parked),
                (parked, TaskState::Parked, TaskState::Parked),
            ]
        );

        // closing the blocker promotes both dependents
        s.apply(blocker, Action::Start).unwrap();
        s.apply(blocker, Action::Complete(None)).unwrap();
        s.apply(blocker, Action::Accept).unwrap();
        assert_eq!(s.task(ready).unwrap().state, TaskState::Ready);
        assert_eq!(s.task(parked).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn block_tasks_is_additive_and_idempotent() {
        let (mut s, p) = store_with_project();
        let existing = create(&mut s, p, TaskState::Ready);
        let blocker = create(&mut s, p, TaskState::Ready);
        let task = create(&mut s, p, TaskState::Ready);
        s.set_blocks_deps(task, &[existing]).unwrap();

        s.block_tasks(blocker, &[task]).unwrap();
        s.block_tasks(blocker, &[task]).unwrap();
        let deps = s.deps_of(task).unwrap();
        assert_eq!(deps.len(), 2, "{deps:?}");
    }

    #[test]
    fn block_tasks_rejects_cycles_and_unknown_tasks() {
        let (mut s, p) = store_with_project();
        let a = create(&mut s, p, TaskState::Ready);
        let b = create(&mut s, p, TaskState::Ready);
        let c = create(&mut s, p, TaskState::Ready);
        // b waits on a, c waits on b; making c block a would close the loop
        s.set_blocks_deps(b, &[a]).unwrap();
        s.set_blocks_deps(c, &[b]).unwrap();
        let err = s.block_tasks(c, &[a]).unwrap_err();
        assert!(matches!(err, Error::DependencyCycle(_)), "{err:?}");

        // self-block is the zero-hop cycle
        assert!(s.block_tasks(a, &[a]).is_err());
        assert!(s.block_tasks(a, &[9999]).is_err());
        assert!(s.block_tasks(9999, &[a]).is_err());
        assert!(s.deps_of(a).unwrap().is_empty());
    }

    #[test]
    fn record_dispatch_starts_the_task_and_opens_a_session() {
        let (mut s, p) = store_with_project();
        let id = create(&mut s, p, TaskState::Ready);
        let (task, session) = s
            .record_dispatch(id, "claude", Some(4321), Some("/var/log/26.log"))
            .unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(session.task_id, id);
        assert_eq!(session.agent, "claude");
        assert_eq!(session.pid, Some(4321));
        assert_eq!(session.log_path.as_deref(), Some("/var/log/26.log"));
        assert!(session.ended_at.is_none());
        assert_eq!(s.sessions_for(id).unwrap().len(), 1);
    }

    #[test]
    fn record_dispatch_on_a_non_ready_task_writes_nothing() {
        let (mut s, p) = store_with_project();
        let id = create(&mut s, p, TaskState::Proposed);
        assert!(matches!(
            s.record_dispatch(id, "claude", None, None),
            Err(Error::InvalidTransition { .. })
        ));
        // the failed transaction must leave neither state change nor session
        assert_eq!(s.task(id).unwrap().state, TaskState::Proposed);
        assert!(s.sessions_for(id).unwrap().is_empty());
    }

    // --- record_continuation (DESIGN.md §6/§8: answer → running → continue) ---

    #[test]
    fn record_continuation_adds_a_session_without_touching_state() {
        let (mut s, p) = store_with_project();
        let id = task_in_state(&mut s, p, TaskState::NeedsInput);
        s.apply(id, Action::Answer("B, with a covering index".into()))
            .unwrap();
        assert_eq!(s.task(id).unwrap().state, TaskState::Running);

        let (task, session) = s
            .record_continuation(id, "claude", Some(777), Some("/var/log/31.log"))
            .unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert_eq!(session.task_id, id);
        assert_eq!(session.agent, "claude");
        assert_eq!(session.pid, Some(777));
        assert_eq!(session.log_path.as_deref(), Some("/var/log/31.log"));
        assert!(session.ended_at.is_none());
        assert_eq!(s.sessions_for(id).unwrap().len(), 1);
    }

    #[test]
    fn record_continuation_refuses_a_task_that_is_not_running() {
        let (mut s, p) = store_with_project();
        for state in [
            TaskState::Proposed,
            TaskState::Parked,
            TaskState::Ready,
            TaskState::NeedsInput,
            TaskState::Review,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            let id = task_in_state(&mut s, p, state);
            assert!(
                matches!(
                    s.record_continuation(id, "claude", None, None),
                    Err(Error::InvalidTransition { .. })
                ),
                "continuation should be refused from {state}"
            );
            // refusal must not create a session
            assert!(s.sessions_for(id).unwrap().is_empty());
        }
    }

    // --- session lifecycle: one open session, closed by terminal transitions ---

    #[test]
    fn terminal_transitions_close_the_open_session_with_the_right_outcome() {
        let (mut s, p) = store_with_project();

        // Accept: the session is kept open through review, then closed
        // `completed` when the review is accepted.
        let accepted = create(&mut s, p, TaskState::Ready);
        let sess = s
            .record_dispatch(accepted, "claude", Some(1), None)
            .unwrap()
            .1;
        s.apply(accepted, Action::Complete(None)).unwrap();
        assert!(
            s.session(sess.id).unwrap().ended_at.is_none(),
            "review keeps it open"
        );
        s.apply(accepted, Action::Accept).unwrap();
        let closed = s.session(sess.id).unwrap();
        assert_eq!(closed.outcome, Some(SessionOutcome::Completed));
        assert!(closed.ended_at.is_some());

        // Abort: running -> ready closes the session `aborted`.
        let aborted = create(&mut s, p, TaskState::Ready);
        let sess = s
            .record_dispatch(aborted, "claude", Some(1), None)
            .unwrap()
            .1;
        s.apply(aborted, Action::Abort).unwrap();
        assert_eq!(
            s.session(sess.id).unwrap().outcome,
            Some(SessionOutcome::Aborted)
        );

        // Abandon (from review) closes the session `aborted` too.
        let abandoned = create(&mut s, p, TaskState::Ready);
        let sess = s
            .record_dispatch(abandoned, "claude", Some(1), None)
            .unwrap()
            .1;
        s.apply(abandoned, Action::Complete(None)).unwrap();
        s.apply(abandoned, Action::Abandon).unwrap();
        assert_eq!(
            s.session(sess.id).unwrap().outcome,
            Some(SessionOutcome::Aborted)
        );
    }

    #[test]
    fn a_continuation_ends_the_predecessor_open_session() {
        let (mut s, p) = store_with_project();
        let id = create(&mut s, p, TaskState::Ready);
        let first = s.record_dispatch(id, "claude", Some(1), None).unwrap().1;

        // ask/answer leave the session open across needs-input -> running...
        s.apply(id, Action::Ask("A or B?".into())).unwrap();
        s.apply(id, Action::Answer("B".into())).unwrap();
        assert!(s.session(first.id).unwrap().ended_at.is_none());

        // ...then a continuation supersedes it: the predecessor is closed and
        // the new session is the only one open (the invariant).
        let second = s
            .record_continuation(id, "claude", Some(2), None)
            .unwrap()
            .1;
        assert!(s.session(first.id).unwrap().ended_at.is_some());
        let open: Vec<i64> = s
            .sessions_for(id)
            .unwrap()
            .into_iter()
            .filter(|x| x.ended_at.is_none())
            .map(|x| x.id)
            .collect();
        assert_eq!(open, vec![second.id]);
    }

    #[test]
    fn rejecting_a_review_keeps_the_same_session_open_with_its_ref() {
        // Rejecting with feedback returns the task to running with its session
        // still open, so a continuation reuses the same agent session — the
        // ref survives until the task actually closes (DESIGN.md §8).
        let (mut s, p) = store_with_project();
        let id = create(&mut s, p, TaskState::Ready);
        let sess = s.record_dispatch(id, "claude", Some(1), None).unwrap().1;
        s.set_session_ref(sess.id, "ref-1").unwrap();
        s.apply(id, Action::Complete(None)).unwrap();
        s.apply(id, Action::RejectWork("redo the tests".into()))
            .unwrap();

        let live = s.session(sess.id).unwrap();
        assert!(live.ended_at.is_none(), "reject leaves the session open");
        assert_eq!(live.session_ref.as_deref(), Some("ref-1"));
        assert_eq!(s.task(id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn a_second_open_session_violates_the_unique_index() {
        // The schema backstop: even a raw insert bypassing insert_session's
        // supersede cannot leave two open rows on one task.
        let (mut s, p) = store_with_project();
        let id = create(&mut s, p, TaskState::Ready);
        s.record_dispatch(id, "claude", Some(1), None).unwrap();
        let second = s.conn.execute(
            "INSERT INTO sessions (task_id, agent, started_at) VALUES (?1, 'x', datetime('now'))",
            [id],
        );
        assert!(second.is_err(), "a second open session must be rejected");
    }

    #[test]
    fn terminal_states_are_final() {
        let (mut s, p) = store_with_project();
        for state in [TaskState::Done, TaskState::Rejected] {
            let id = task_in_state(&mut s, p, state);
            for action in all_actions() {
                assert!(s.apply(id, action).is_err(), "{state} must be terminal");
            }
        }
    }

    // --- reconcile_session (DESIGN.md §8, the observation half of dispatch) ---

    mod reconcile {
        use super::*;
        use crate::model::SessionOutcome;

        fn dispatch(s: &mut Store, p: i64) -> (i64, i64) {
            let task_id = create(s, p, TaskState::Ready);
            let (_, session) = s
                .record_dispatch(task_id, "claude", Some(4242), Some("/var/log/s.log"))
                .unwrap();
            (task_id, session.id)
        }

        #[test]
        fn live_pid_is_left_untouched() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);

            let result = s.reconcile_session(session_id, true, false).unwrap();
            assert!(result.is_none());
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
            assert!(s.session(session_id).unwrap().ended_at.is_none());
        }

        #[test]
        fn dead_pid_on_a_running_task_stalls_it() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);

            let (session, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Failed));
            assert!(session.ended_at.is_some());
            // the dispatch died under the task: running -> stalled (DESIGN.md
            // §6/§8), the attention state that puts redispatch in the queue.
            assert_eq!(task.state, TaskState::Stalled);

            let events = s.events_for(task_id).unwrap();
            let transition = events.last().unwrap();
            assert_eq!(transition.kind, "transition");
            assert_eq!(
                transition.detail.as_deref(),
                Some("running -> stalled"),
                "{events:?}"
            );
            let reconcile = &events[events.len() - 2];
            assert_eq!(reconcile.kind, "reconcile");
            let detail = reconcile.detail.as_deref().unwrap();
            assert!(detail.contains("without reporting"), "{detail}");
            assert!(detail.contains("failed"), "{detail}");
        }

        #[test]
        fn dead_pid_reports_capped_when_the_caller_says_so() {
            let (mut s, p) = store_with_project();
            let (_, session_id) = dispatch(&mut s, p);

            let (session, task) = s
                .reconcile_session(session_id, false, true)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Capped));
            // a cap stalls the task the same way a failure does; redispatch
            // happens from `stalled` once quota resets (DESIGN.md §6/§8).
            assert_eq!(task.state, TaskState::Stalled);
        }

        #[test]
        fn a_stalled_task_can_be_redispatched() {
            // record_dispatch's precondition accepts stalled -> running:
            // redispatch is the whole point of the state (DESIGN.md §8).
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.reconcile_session(session_id, false, false).unwrap();
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Stalled);

            let (task, session) = s
                .record_dispatch(task_id, "codex", Some(4343), Some("/var/log/s2.log"))
                .unwrap();
            assert_eq!(task.state, TaskState::Running);
            assert_eq!(session.agent, "codex");
            assert!(session.ended_at.is_none());
        }

        #[test]
        fn a_stalled_task_with_an_open_blocker_is_parked() {
            // Readiness reconciliation treats stalled like ready (DESIGN.md
            // §6): a blocker opened mid-run means the stall lands in parked,
            // never surfacing unactionable work in the queue.
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            let blocker = create(&mut s, p, TaskState::Ready);
            s.add_dep(task_id, blocker, crate::model::DepKind::Blocks)
                .unwrap();

            let (_, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert_eq!(task.state, TaskState::Parked);
        }

        #[test]
        fn adding_an_open_blocker_demotes_a_stalled_task() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.reconcile_session(session_id, false, false).unwrap();
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Stalled);

            let blocker = create(&mut s, p, TaskState::Ready);
            s.add_dep(task_id, blocker, crate::model::DepKind::Blocks)
                .unwrap();
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Parked);

            // when the blocker closes it re-promotes to ready, not stalled —
            // the stall context is stale by then (DESIGN.md §6).
            s.apply(blocker, Action::Start).unwrap();
            s.apply(blocker, Action::Complete(None)).unwrap();
            s.apply(blocker, Action::Accept).unwrap();
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Ready);
        }

        #[test]
        fn a_needs_input_tasks_session_stays_open() {
            // The asking session is reused when the answer continues the work,
            // so it stays open across needs-input; reconcile leaves it alone
            // even with a dead process (DESIGN.md §8).
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.apply(task_id, Action::Ask("A or B?".into())).unwrap();

            assert!(
                s.reconcile_session(session_id, false, false)
                    .unwrap()
                    .is_none()
            );
            assert!(s.session(session_id).unwrap().ended_at.is_none());
            assert_eq!(s.task(task_id).unwrap().state, TaskState::NeedsInput);
        }

        #[test]
        fn a_review_tasks_session_stays_open() {
            // Review keeps the session open on purpose, so a reject-with-feedback
            // can continue the same agent session; reconcile must not close it.
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.apply(task_id, Action::Complete(None)).unwrap();

            assert!(
                s.reconcile_session(session_id, false, false)
                    .unwrap()
                    .is_none()
            );
            assert!(s.session(session_id).unwrap().ended_at.is_none());
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Review);
        }

        #[test]
        fn aborting_closes_the_session_so_reconcile_is_a_noop() {
            // Abort now closes the session itself, in the same transaction, so
            // by the time reconcile sees it there is nothing left to do.
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.apply(task_id, Action::Abort).unwrap();

            let session = s.session(session_id).unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Aborted));
            assert!(session.ended_at.is_some());
            assert!(
                s.reconcile_session(session_id, false, false)
                    .unwrap()
                    .is_none()
            );
            // a manual abort lands in plain ready, never stalled — the human
            // chose to stop the work; nothing about the dispatch died.
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Ready);
        }

        #[test]
        fn a_stale_open_session_on_a_closed_task_is_finalised() {
            // The session-25/#79 bug: a `done` task still carrying an open
            // session because the pre-fix reconcile never closed it. The new
            // logic finalises it on the next pass, without manual SQL. Forcing
            // the state directly reproduces the legacy stranded row that
            // `Accept` would otherwise have closed.
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.conn
                .execute(
                    "UPDATE tasks SET state = 'done', closed_at = datetime('now') WHERE id = ?1",
                    [task_id],
                )
                .unwrap();

            let (session, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert!(session.ended_at.is_some());
            assert_eq!(session.outcome, Some(SessionOutcome::Completed));
            assert_eq!(task.state, TaskState::Done);
        }

        #[test]
        fn an_already_ended_session_is_not_reprocessed() {
            let (mut s, p) = store_with_project();
            let (_, session_id) = dispatch(&mut s, p);
            s.reconcile_session(session_id, false, false).unwrap();
            let first_ended_at = s.session(session_id).unwrap().ended_at;

            let result = s.reconcile_session(session_id, false, false).unwrap();
            assert!(result.is_none());
            assert_eq!(s.session(session_id).unwrap().ended_at, first_ended_at);
        }

        #[test]
        fn redispatch_supersedes_the_prior_session_keeping_one_open() {
            // dispatch, abort, redispatch: abort now closes the first session
            // itself, so the redispatch opens the only remaining open row. The
            // one-open-session invariant holds, and reconciling the old
            // already-closed session is a no-op that can't knock the task back.
            let (mut s, p) = store_with_project();
            let (task_id, older_session) = dispatch(&mut s, p);
            s.apply(task_id, Action::Abort).unwrap();
            assert!(s.session(older_session).unwrap().ended_at.is_some());

            let newer_session = s
                .record_dispatch(task_id, "claude", Some(4343), Some("/var/log/s2.log"))
                .unwrap()
                .1
                .id;

            // exactly one open session — the newer one
            let open: Vec<i64> = s
                .sessions_for(task_id)
                .unwrap()
                .into_iter()
                .filter(|x| x.ended_at.is_none())
                .map(|x| x.id)
                .collect();
            assert_eq!(open, vec![newer_session]);

            // reconciling the old, already-closed session does nothing
            assert!(
                s.reconcile_session(older_session, false, false)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
            assert!(s.session(newer_session).unwrap().ended_at.is_none());
        }

        #[test]
        fn unknown_session_is_an_error() {
            let (mut s, _p) = store_with_project();
            assert!(matches!(
                s.reconcile_session(9999, false, false),
                Err(Error::SessionNotFound(9999))
            ));
        }
    }

    #[test]
    fn add_dep_rejects_self_blocks() {
        let (mut s, p) = store_with_project();
        let task = create(&mut s, p, TaskState::Ready);
        let err = s.add_dep(task, task, DepKind::Blocks).unwrap_err();
        assert!(
            matches!(&err, Error::DependencyCycle(path) if path == &format!("{task} -> {task}")),
            "expected a self-cycle error, got {err}"
        );
    }

    #[test]
    fn add_dep_rejects_direct_cycle() {
        let (mut s, p) = store_with_project();
        let a = create(&mut s, p, TaskState::Ready);
        let b = create(&mut s, p, TaskState::Ready);
        s.add_dep(b, a, DepKind::Blocks).unwrap();

        let err = s.add_dep(a, b, DepKind::Blocks).unwrap_err();
        assert!(
            matches!(&err, Error::DependencyCycle(path) if path == &format!("{a} -> {b} -> {a}")),
            "expected a direct cycle error, got {err}"
        );
        // the rejected write must not have landed
        assert!(s.deps_of(a).unwrap().is_empty());
    }

    #[test]
    fn add_dep_rejects_transitive_cycle() {
        let (mut s, p) = store_with_project();
        let a = create(&mut s, p, TaskState::Ready);
        let b = create(&mut s, p, TaskState::Ready);
        let c = create(&mut s, p, TaskState::Ready);
        s.add_dep(b, c, DepKind::Blocks).unwrap();
        s.add_dep(c, a, DepKind::Blocks).unwrap();

        let err = s.add_dep(a, b, DepKind::Blocks).unwrap_err();
        assert!(
            matches!(&err, Error::DependencyCycle(path)
                if path == &format!("{a} -> {b} -> {c} -> {a}")),
            "expected a transitive cycle error, got {err}"
        );
    }

    #[test]
    fn add_dep_allows_a_diamond() {
        // a depends on b and c; b and c both depend on d. Not a cycle.
        let (mut s, p) = store_with_project();
        let a = create(&mut s, p, TaskState::Ready);
        let b = create(&mut s, p, TaskState::Ready);
        let c = create(&mut s, p, TaskState::Ready);
        let d = create(&mut s, p, TaskState::Ready);
        s.add_dep(b, d, DepKind::Blocks).unwrap();
        s.add_dep(c, d, DepKind::Blocks).unwrap();
        s.add_dep(a, b, DepKind::Blocks).unwrap();
        s.add_dep(a, c, DepKind::Blocks).unwrap();

        let deps = s.deps_of(a).unwrap();
        assert_eq!(
            deps.iter().map(|d| d.depends_on).collect::<Vec<_>>(),
            vec![b, c]
        );
    }

    #[test]
    fn set_blocks_deps_rejects_self_and_transitive_cycles() {
        let (mut s, p) = store_with_project();
        let a = create(&mut s, p, TaskState::Ready);
        let b = create(&mut s, p, TaskState::Ready);
        let c = create(&mut s, p, TaskState::Ready);

        let err = s.set_blocks_deps(a, &[a]).unwrap_err();
        assert!(matches!(&err, Error::DependencyCycle(path) if path == &format!("{a} -> {a}")));

        // b -> c -> a, then a -> b would close the cycle
        s.set_blocks_deps(b, &[c]).unwrap();
        s.set_blocks_deps(c, &[a]).unwrap();
        let err = s.set_blocks_deps(a, &[b]).unwrap_err();
        assert!(matches!(&err, Error::DependencyCycle(path)
                if path == &format!("{a} -> {b} -> {c} -> {a}")));
        // the rejected write must leave the task's existing blocks deps alone
        assert!(s.deps_of(a).unwrap().is_empty());
    }

    #[test]
    fn set_blocks_deps_allows_a_diamond() {
        let (mut s, p) = store_with_project();
        let a = create(&mut s, p, TaskState::Ready);
        let b = create(&mut s, p, TaskState::Ready);
        let c = create(&mut s, p, TaskState::Ready);
        let d = create(&mut s, p, TaskState::Ready);
        s.set_blocks_deps(b, &[d]).unwrap();
        s.set_blocks_deps(c, &[d]).unwrap();

        let t = s.set_blocks_deps(a, &[b, c]).unwrap();
        assert_eq!(t.state, TaskState::Parked);
    }
}
