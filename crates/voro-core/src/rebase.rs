//! The conflict-resolution continuation for a stale review branch (DESIGN.md
//! §6/§8): a `review` task whose branch no longer merges cleanly with the
//! moved base is sent back to its agent session through the same
//! reject-with-feedback path, with a generated feedback body naming the case.
//! The precondition check and the feedback text live here, pure of I/O — the
//! git step that updates the base branch, and the continuation launch, are the
//! `voro` crate's.

use crate::error::{Error, Result};
use crate::model::{Task, TaskState};

/// Everything the conflict-resolution action needs once the task is proven
/// eligible (DESIGN.md §8): the conflicting branch, and the feedback body fed
/// through `Action::RejectWork` into the continued session. Assembled by
/// [`plan_rebase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebasePlan {
    pub branch: String,
    pub feedback: String,
}

/// Validate that a task can take the conflict-resolution continuation, and if
/// so assemble the [`RebasePlan`] (DESIGN.md §6/§8): an agent-executed `review`
/// task carrying a branch. Each gap fails naming what is missing — a human
/// task has no agent session to continue, a non-review task has no finished
/// work going stale, and without a branch there is nothing to rebase. `base`
/// is the checkout's default branch, named in the feedback so the agent knows
/// what to rebase onto. Pure of I/O.
pub fn plan_rebase(task: &Task, base: &str) -> Result<RebasePlan> {
    if task.human {
        return Err(Error::HumanTask {
            id: task.id,
            reason: "it has no agent session to continue — resolve the conflicts by hand".into(),
        });
    }
    if task.state != TaskState::Review {
        return Err(Error::Invalid(format!(
            "only a review task's branch can go stale under it; task {} is {}",
            task.id, task.state
        )));
    }
    let branch = task
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "task {} has no branch to rebase — record one with `voro set --branch`",
                task.id
            ))
        })?;
    let feedback = format!(
        "The work itself was not rejected: `{base}` has moved since this task reached \
         review, and your branch `{branch}` now has merge conflicts with it. The updated \
         `{base}` has been pulled into the project checkout. In your worktree, rebase \
         `{branch}` onto the updated `{base}` (or merge `{base}` in), resolve the \
         conflicts, verify the build and tests still pass, and re-report with `voro done` \
         as before."
    );
    Ok(RebasePlan {
        branch: branch.to_string(),
        feedback,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Priority;

    fn task(state: TaskState, branch: Option<&str>, human: bool) -> Task {
        Task {
            id: 111,
            project_id: 1,
            title: "Resolve review-task merge conflicts".into(),
            body: String::new(),
            priority: Priority::P1,
            state,
            agent: None,
            human,
            question: None,
            pr_url: None,
            branch: branch.map(str::to_string),
            state_since: "2026-07-10 00:00:00".into(),
            created_at: "2026-07-10 00:00:00".into(),
            closed_at: None,
        }
    }

    #[test]
    fn plans_a_rebase_from_a_review_task_with_a_branch() {
        let plan = plan_rebase(&task(TaskState::Review, Some("feat/x"), false), "main").unwrap();
        assert_eq!(plan.branch, "feat/x");
        // the feedback tells the agent the branch, the base, what happened,
        // and how to report back
        assert!(plan.feedback.contains("`feat/x`"), "{}", plan.feedback);
        assert!(plan.feedback.contains("`main`"), "{}", plan.feedback);
        assert!(
            plan.feedback.contains("merge conflicts"),
            "{}",
            plan.feedback
        );
        assert!(plan.feedback.contains("worktree"), "{}", plan.feedback);
        assert!(plan.feedback.contains("voro done"), "{}", plan.feedback);
        assert!(
            plan.feedback.contains("not rejected"),
            "the feedback must distinguish a stale branch from rejected work: {}",
            plan.feedback
        );
    }

    #[test]
    fn plan_names_a_non_main_base() {
        let plan = plan_rebase(&task(TaskState::Review, Some("feat/x"), false), "trunk").unwrap();
        assert!(plan.feedback.contains("`trunk`"), "{}", plan.feedback);
    }

    #[test]
    fn plan_requires_the_review_state() {
        for state in [
            TaskState::Proposed,
            TaskState::Parked,
            TaskState::Ready,
            TaskState::Running,
            TaskState::NeedsInput,
            TaskState::Waiting,
            TaskState::Stalled,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            let err = plan_rebase(&task(state, Some("feat/x"), false), "main")
                .unwrap_err()
                .to_string();
            assert!(err.contains("review"), "{state}: {err}");
        }
    }

    #[test]
    fn plan_names_a_missing_or_blank_branch() {
        for branch in [None, Some("   ")] {
            let err = plan_rebase(&task(TaskState::Review, branch, false), "main")
                .unwrap_err()
                .to_string();
            assert!(err.contains("no branch"), "{err}");
            assert!(err.contains("voro set --branch"), "{err}");
        }
    }

    /// The full core path the action rides (DESIGN.md §6/§8): the plan's
    /// feedback goes through `Action::RejectWork`, so `review → running` lands
    /// with the canned text in the body and event log while the session stays
    /// open for the continuation to reuse.
    #[test]
    fn plan_feedback_through_rejectwork_requeues_and_keeps_the_session_open() {
        use crate::store::{NewTask, Store};
        use crate::transition::Action;

        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/proj").unwrap();
        let id = s
            .create_task(NewTask {
                project_id: p.id,
                title: "T".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap()
            .id;
        let (_, session) = s.record_dispatch(id, "claude", Some(1), None).unwrap();
        s.apply(id, Action::Complete(Some("done".into()))).unwrap();
        s.set_branch(id, Some("feat/x")).unwrap();

        let plan = plan_rebase(&s.task(id).unwrap(), "main").unwrap();
        let task = s.apply(id, Action::RejectWork(plan.feedback)).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("merge conflicts"), "{}", task.body);
        assert!(
            s.session(session.id).unwrap().ended_at.is_none(),
            "the session must stay open for the continuation to reuse"
        );
        assert!(
            s.events_for(id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "feedback"
                    && e.detail
                        .as_deref()
                        .is_some_and(|d| d.contains("merge conflicts"))),
            "the event log must carry the canned feedback"
        );
    }

    #[test]
    fn plan_refuses_a_human_task() {
        // Unreachable by construction — a human task never enters `review`
        // (§6) — but refused explicitly, the same belt-and-braces shape as
        // dispatch's own check.
        let err = plan_rebase(&task(TaskState::Review, Some("feat/x"), true), "main").unwrap_err();
        assert!(matches!(err, Error::HumanTask { id: 111, .. }), "{err}");
    }
}
