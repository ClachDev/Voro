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
    Store, get_session, get_task, insert_session, latest_session_id, log_event, set_session_outcome,
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
    /// ready → running (dispatch, or the human starting by hand)
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
    /// ready → parked (deliberate parking)
    Park,
    /// parked → ready (manual unpark)
    Unpark,
    /// parked | ready | needs-input | review → rejected
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
    pub fn legal_actions(state: TaskState) -> Vec<Action> {
        use TaskState::*;
        match state {
            Proposed => vec![
                Action::Triage(Triage::Ready),
                Action::Triage(Triage::Parked),
                Action::Triage(Triage::Reject),
            ],
            Parked => vec![Action::Unpark, Action::Abandon],
            Ready => vec![Action::Start, Action::Park, Action::Abandon],
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

    /// Close out a session whose backing process is no longer running
    /// (DESIGN.md §8, the observation half of dispatch). `pid_alive` and
    /// `likely_capped` are supplied by the caller — voro-core stays free of
    /// process and log I/O — so this is a pure decision over the session and
    /// its task's current state:
    ///
    /// - `pid_alive`: nothing to do, the session is left untouched (`Ok(None)`).
    /// - the session already ended: also a no-op, so a caller looping over
    ///   `live_sessions` repeatedly can't double-finalise one.
    /// - the task is still `running` and this is its most recent session: the
    ///   agent's process died without calling `done` or `ask`. The session
    ///   outcome is `capped` if `likely_capped`, else `failed`, and the task
    ///   drops `running → ready` flagged for redispatch (via the same
    ///   transition `Abort` uses, tagged with a distinct `reconcile` event so
    ///   the log tells an automatic reconciliation apart from a human abort).
    /// - the task is `running` but an *older* session is the one that exited —
    ///   it was aborted and redispatched (#41), and a newer session now owns
    ///   the running state. Its outcome is backfilled (`failed`/`capped`) but
    ///   the task is left in `running`, so a dead predecessor can't demote a
    ///   legitimately-running task.
    /// - the task already left `running` on its own — `done`/`ask` landed it
    ///   in `review`/`needs-input` before the process exited, or a human
    ///   otherwise moved it — the session outcome reflects that instead
    ///   (`completed`/`asked`/`aborted`) and the task is left alone.
    pub fn reconcile_session(
        &mut self,
        session_id: i64,
        pid_alive: bool,
        likely_capped: bool,
    ) -> Result<Option<(Session, Task)>> {
        if pid_alive {
            return Ok(None);
        }
        let tx = self.conn.transaction()?;
        let session = get_session(&tx, session_id)?.ok_or(Error::SessionNotFound(session_id))?;
        if session.ended_at.is_some() {
            return Ok(None);
        }
        let task = get_task(&tx, session.task_id)?.ok_or(Error::TaskNotFound(session.task_id))?;

        let outcome = match task.state {
            TaskState::Running => {
                if likely_capped {
                    SessionOutcome::Capped
                } else {
                    SessionOutcome::Failed
                }
            }
            TaskState::NeedsInput => SessionOutcome::Asked,
            TaskState::Review => SessionOutcome::Completed,
            _ => SessionOutcome::Aborted,
        };
        set_session_outcome(&tx, session_id, outcome)?;

        let is_current_session = latest_session_id(&tx, task.id)? == Some(session_id);
        if task.state == TaskState::Running && is_current_session {
            apply_action(&tx, task.id, Action::Abort)?;
            log_event(
                &tx,
                task.id,
                "reconcile",
                Some(&format!(
                    "session {session_id} exited ({outcome}); flagged for redispatch"
                )),
            )?;
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
        (Ready, Action::Start) => Running,
        (Running, Action::Ask(_)) => NeedsInput,
        (NeedsInput, Action::Answer(_)) => Running,
        (Running, Action::Complete(_)) => Review,
        (Review, Action::Accept) => Done,
        (Review, Action::RejectWork(_)) => Running,
        (Running, Action::Abort) => Ready,
        (Ready, Action::Park) => Parked,
        (Parked, Action::Unpark) => Ready,
        (Parked | Ready | NeedsInput | Review, Action::Abandon) => Rejected,
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
/// - `ready` with an open blocker → demote to `parked`.
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
        TaskState::Ready if open > 0 => TaskState::Parked,
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
            (Ready, Action::Start) => Some(Running),
            (Ready, Action::Park) => Some(Parked),
            (Parked, Action::Unpark) => Some(Ready),
            (Running, Action::Ask(_)) => Some(NeedsInput),
            (Running, Action::Complete(_)) => Some(Review),
            (Running, Action::Abort) => Some(Ready),
            (NeedsInput, Action::Answer(_)) => Some(Running),
            (Review, Action::Accept) => Some(Done),
            (Review, Action::RejectWork(_)) => Some(Running),
            (Parked | Ready | NeedsInput | Review, Action::Abandon) => Some(Rejected),
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
            let legal = Store::legal_actions(state);
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
        fn dead_pid_on_a_running_task_fails_and_drops_to_ready() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);

            let (session, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Failed));
            assert!(session.ended_at.is_some());
            assert_eq!(task.state, TaskState::Ready);
            assert!(s.redispatch_flag(task_id).unwrap());

            let events = s.events_for(task_id).unwrap();
            assert_eq!(events.last().unwrap().kind, "reconcile");
            assert!(
                events
                    .last()
                    .unwrap()
                    .detail
                    .as_deref()
                    .unwrap()
                    .contains("failed")
            );
            // the transition itself reads exactly like Abort's
            let transition = events.iter().rev().nth(1).unwrap();
            assert_eq!(transition.kind, "transition");
            assert_eq!(transition.detail.as_deref(), Some("running -> ready"));
        }

        #[test]
        fn dead_pid_reports_capped_when_the_caller_says_so() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);

            let (session, task) = s
                .reconcile_session(session_id, false, true)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Capped));
            assert_eq!(task.state, TaskState::Ready);
            assert!(s.redispatch_flag(task_id).unwrap());
        }

        #[test]
        fn a_task_the_agent_already_asked_is_left_alone() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.apply(task_id, Action::Ask("A or B?".into())).unwrap();

            let (session, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Asked));
            assert_eq!(task.state, TaskState::NeedsInput);
            assert!(!s.redispatch_flag(task_id).unwrap());
        }

        #[test]
        fn a_task_the_agent_already_completed_is_left_alone() {
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.apply(task_id, Action::Complete(None)).unwrap();

            let (session, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Completed));
            assert_eq!(task.state, TaskState::Review);
            assert!(!s.redispatch_flag(task_id).unwrap());
        }

        #[test]
        fn a_task_already_moved_off_running_some_other_way_is_marked_aborted() {
            // e.g. a human aborted by hand before the process was noticed dead.
            let (mut s, p) = store_with_project();
            let (task_id, session_id) = dispatch(&mut s, p);
            s.apply(task_id, Action::Abort).unwrap();

            let (session, task) = s
                .reconcile_session(session_id, false, false)
                .unwrap()
                .unwrap();
            assert_eq!(session.outcome, Some(SessionOutcome::Aborted));
            assert_eq!(task.state, TaskState::Ready);
            // a manual abort must not itself read as a redispatch flag
            assert!(!s.redispatch_flag(task_id).unwrap());
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
        fn a_dead_older_session_does_not_demote_a_redispatched_task() {
            // dispatch, abort (session stays open, #41), redispatch: the task
            // is running behind a fresh, live session when the first session's
            // process is finally noticed dead. Reconciling that older session
            // must backfill only its own outcome, not knock the task back.
            let (mut s, p) = store_with_project();
            let (task_id, older_session) = dispatch(&mut s, p);
            s.apply(task_id, Action::Abort).unwrap();
            let newer_session = s
                .record_dispatch(task_id, "claude", Some(4343), Some("/var/log/s2.log"))
                .unwrap()
                .1
                .id;

            let (session, task) = s
                .reconcile_session(older_session, false, false)
                .unwrap()
                .unwrap();

            // the older session is finalised as failed...
            assert_eq!(session.id, older_session);
            assert_eq!(session.outcome, Some(SessionOutcome::Failed));
            // ...but the task stays running behind the still-live newer session
            assert_eq!(task.state, TaskState::Running);
            assert!(!s.redispatch_flag(task_id).unwrap());
            assert!(s.session(newer_session).unwrap().ended_at.is_none());

            // and no reconcile/transition event was logged against the task
            let events = s.events_for(task_id).unwrap();
            assert!(events.iter().all(|e| e.kind != "reconcile"));
        }

        #[test]
        fn unknown_session_is_an_error() {
            let (mut s, _p) = store_with_project();
            assert!(matches!(
                s.reconcile_session(9999, false, false),
                Err(Error::SessionNotFound(9999))
            ));
        }

        #[test]
        fn redispatch_flag_is_false_with_no_sessions() {
            let (mut s, p) = store_with_project();
            let id = create(&mut s, p, TaskState::Ready);
            assert!(!s.redispatch_flag(id).unwrap());
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
