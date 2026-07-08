//! The task state machine (DESIGN.md §6). `Store::apply` is the only path
//! that changes `tasks.state`; it validates the transition, restamps
//! `state_since`, maintains the `question`/`closed_at` invariants, appends to
//! the event log, and cascades readiness of dependant tasks — all in one
//! transaction.

use rusqlite::{Connection, params};

use crate::error::{Error, Result};
use crate::model::{Task, TaskState};
use crate::store::{Store, get_task, log_event};

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
    /// running → review
    Complete,
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
            Action::Complete => "complete",
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
            Running => vec![Action::Ask(String::new()), Action::Complete, Action::Abort],
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
        let task = get_task(&tx, task_id)?.ok_or(Error::TaskNotFound(task_id))?;

        use TaskState::*;
        let to = match (task.state, &action) {
            (Proposed, Action::Triage(Triage::Parked)) => Parked,
            (Proposed, Action::Triage(Triage::Ready)) => Ready,
            (Proposed, Action::Triage(Triage::Reject)) => Rejected,
            (Ready, Action::Start) => Running,
            (Running, Action::Ask(_)) => NeedsInput,
            (NeedsInput, Action::Answer(_)) => Running,
            (Running, Action::Complete) => Review,
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
            &tx,
            task_id,
            "transition",
            Some(&format!("{} -> {}", task.state, to)),
        )?;

        match &action {
            Action::Answer(a) => {
                append_section(&tx, task_id, "Answers", a.trim())?;
                log_event(&tx, task_id, "answer", Some(a.trim()))?;
            }
            Action::RejectWork(f) => {
                append_section(&tx, task_id, "Feedback", f.trim())?;
                log_event(&tx, task_id, "feedback", Some(f.trim()))?;
            }
            _ => {}
        }

        if to.is_terminal() {
            reconcile_dependants(&tx, task_id)?;
        }

        tx.commit()?;
        self.task(task_id)
    }

    /// Replace the `blocks` dependencies of a task with the given set, then
    /// reconcile its readiness. This is the dep-editing entry point for
    /// interfaces; `add_dep`/`remove_dep` reconcile too.
    pub fn set_blocks_deps(&mut self, task_id: i64, depends_on: &[i64]) -> Result<Task> {
        if depends_on.contains(&task_id) {
            return Err(Error::Invalid("a task cannot depend on itself".into()));
        }
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
                s.apply(id, Action::Complete).unwrap();
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
            Action::Complete,
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
            (Running, Action::Complete) => Some(Review),
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

        s.apply(id, Action::Complete).unwrap();
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
        s.apply(blocker, Action::Complete).unwrap();
        s.apply(blocker, Action::Accept).unwrap();
        assert_eq!(s.task(task).unwrap().state, TaskState::Ready);
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
    fn terminal_states_are_final() {
        let (mut s, p) = store_with_project();
        for state in [TaskState::Done, TaskState::Rejected] {
            let id = task_in_state(&mut s, p, state);
            for action in all_actions() {
                assert!(s.apply(id, action).is_err(), "{state} must be terminal");
            }
        }
    }
}
