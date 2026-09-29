//! The cockpit's rows (DESIGN.md §9): the queue, proposal and running strips
//! in one selection space, the digests that expand in place, and the focus
//! card's scroll.

use voro_core::{ActionRow, DigestRow, QueueRow};

use super::{App, CockpitRow};

impl App {
    /// Flatten the queue into selectable rows: every queue row, with an
    /// expanded digest's proposals listed beneath it, then the running strip.
    pub(super) fn build_cockpit_rows(&self) -> Vec<CockpitRow> {
        let mut rows = Vec::new();
        for (i, row) in self.queue.rows.iter().enumerate() {
            rows.push(CockpitRow::Queue(i));
            if let QueueRow::Digest(digest) = row
                && self.expanded_digests.contains(&digest.project_name)
            {
                rows.extend((0..digest.tasks.len()).map(|j| CockpitRow::Proposal(i, j)));
            }
        }
        rows.extend((0..self.running.len()).map(CockpitRow::Running));
        rows
    }

    /// The queue's task rows in order, by id — digests contribute nothing,
    /// since they name a backlog rather than a task.
    #[cfg(test)]
    pub fn queue_task_ids(&self) -> Vec<i64> {
        self.queue
            .rows
            .iter()
            .filter_map(|row| match row {
                QueueRow::Action(row) => Some(row.candidate.task.id),
                QueueRow::Digest(_) => None,
            })
            .collect()
    }

    /// The digest a queue row holds, if it is one.
    pub fn digest(&self, queue_index: usize) -> Option<&DigestRow> {
        match self.queue.rows.get(queue_index)? {
            QueueRow::Digest(digest) => Some(digest),
            QueueRow::Action(_) => None,
        }
    }

    /// One proposal inside a digest row.
    pub fn digest_child(&self, queue_index: usize, child: usize) -> Option<&ActionRow> {
        self.digest(queue_index)?.tasks.get(child)
    }

    /// Fold a digest row open or shut, so its proposals become selectable for
    /// triage (DESIGN.md §7). Rebuilds the row list in place; the selection
    /// stays on the digest, which is where the operator pressed Enter.
    pub(super) fn toggle_digest(&mut self, queue_index: usize) {
        let Some(digest) = self.digest(queue_index) else {
            return;
        };
        let project = digest.project_name.clone();
        if !self.expanded_digests.remove(&project) {
            self.expanded_digests.insert(project);
        }
        self.cockpit_rows = self.build_cockpit_rows();
        self.cockpit_sel = self
            .cockpit_sel
            .min(self.cockpit_rows.len().saturating_sub(1));
    }

    /// Scroll the cockpit focus card, clamped to the overflow `draw_detail`
    /// last measured so `K` past the top or `J` past the bottom simply stops.
    pub(super) fn scroll_detail(&mut self, delta: i64) {
        let max = self.detail_max_scroll.get() as i64;
        self.detail_scroll = (self.detail_scroll as i64 + delta).clamp(0, max) as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::action_label;
    use crate::app::modes::Mode;
    use crate::app::tests::{app_with, key};
    use ratatui::crossterm::event::KeyCode;
    use voro_core::{Action, Store, TaskState};

    /// Proposals ride the queue as one digest row per project (DESIGN.md §7),
    /// so triage is two keystrokes: Enter folds the digest open, Enter on the
    /// proposal beneath it opens the triage menu as before.
    #[test]
    fn enter_expands_the_digest_then_opens_the_triage_menu() {
        let mut app = app_with(&[TaskState::Proposed]);
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Queue(_)
        ));
        // The digest names no task of its own — the row stands for the backlog.
        assert_eq!(app.selected_task_id(), None);
        assert_eq!(app.enter_hint(), Some("⏎ expand"));

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.enter_hint(), Some("⏎ collapse"));
        assert!(matches!(app.cockpit_rows[1], CockpitRow::Proposal(0, 0)));

        app.move_selection(1);
        assert_eq!(app.enter_hint(), Some("⏎ triage"));
        key(&mut app, KeyCode::Enter);
        let task_id = match &app.mode {
            Mode::Transition {
                actions, task_id, ..
            } => {
                assert_eq!(*actions, Store::legal_actions(TaskState::Proposed, false));
                *task_id
            }
            _ => panic!("enter on a proposed row should open the triage menu"),
        };

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Ready);
        // the triaged task re-enters the queue as startable work
        assert_eq!(app.queue.rows.len(), 1);
        assert_eq!(app.enter_hint(), Some("⏎ act"));
    }

    /// A refine in flight is out of the triage queue and on the running strip
    /// instead (DESIGN.md §6/§9) — in *this* window whether or not it launched
    /// the round, because the state is in the store rather than in a flag the
    /// launching process holds.
    #[test]
    fn a_refining_proposal_leaves_the_queue_for_the_running_strip() {
        let mut app = app_with(&[TaskState::Refining, TaskState::Ready]);
        let refining = app.all[0].task.id;

        assert!(
            !app.queue_task_ids().contains(&refining),
            "{:?}",
            app.queue_task_ids()
        );
        assert!(
            app.queue
                .rows
                .iter()
                .all(|row| !matches!(row, QueueRow::Digest(_))),
            "a refining proposal must not ride the triage digest either"
        );
        assert_eq!(app.running.len(), 1);
        assert_eq!(app.running[0].task_state, TaskState::Refining);
        assert_eq!(app.counts.refining, 1);
        assert_eq!(app.counts.proposed, 0);

        // and the strip row is selectable, so the detail card and `C` reach it
        let strip = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .expect("the refine rides the strip");
        app.cockpit_sel = strip;
        assert!(app.selected_is_refining());
    }

    /// A hand-off is work in flight someone else owns, which is the same fact
    /// the strip carries about a dispatch (DESIGN.md §9). It rides the strip
    /// while staying out of the queue and out of `next` — `waiting` earns no
    /// score (§7), and putting it on the strip does not change that.
    #[test]
    fn a_waiting_task_rides_the_strip_and_not_the_queue() {
        let mut app = app_with(&[TaskState::Waiting, TaskState::Ready]);
        let waiting = app
            .all
            .iter()
            .find(|r| r.task.state == TaskState::Waiting)
            .unwrap()
            .task
            .id;

        assert!(
            !app.queue_task_ids().contains(&waiting),
            "{:?}",
            app.queue_task_ids()
        );
        assert_eq!(app.running.len(), 1);
        assert_eq!(app.running[0].task_id, waiting);
        assert_eq!(app.running[0].task_state, TaskState::Waiting);
        assert_eq!(app.counts.waiting, 1);

        let strip = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .expect("the hand-off rides the strip");
        app.cockpit_sel = strip;
        assert_eq!(app.selected_task_id(), Some(waiting));
    }

    /// The strip's newest row kind reaches the verdicts `waiting` offers
    /// (DESIGN.md §6) through the same transition menu every other row uses —
    /// a merged PR is accepted without leaving the cockpit.
    #[test]
    fn a_verdict_applies_from_a_waiting_strip_row() {
        let mut app = app_with(&[TaskState::Waiting]);
        let task_id = app.running[0].task_id;
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        key(&mut app, KeyCode::Char('s'));
        match &app.mode {
            Mode::Transition { actions, .. } => assert_eq!(
                actions.iter().map(action_label).collect::<Vec<_>>(),
                vec![
                    "accept → done",
                    "reject with feedback → running",
                    "reclaim → review",
                    "abandon → rejected",
                ]
            ),
            _ => panic!("s on a waiting strip row should open the transition menu"),
        }
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Done);
        assert!(app.running.is_empty(), "{:?}", app.running);
    }

    /// The reclaim verdict returns a hand-off to the operator's own queue, so
    /// the row leaves the strip for a queue row rather than closing.
    #[test]
    fn reclaiming_from_the_strip_returns_the_task_to_the_queue() {
        let mut app = app_with(&[TaskState::Waiting]);
        let task_id = app.running[0].task_id;

        app.apply_and_refresh(task_id, Action::Reclaim);

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Review);
        assert!(app.running.is_empty(), "{:?}", app.running);
        assert!(app.queue_task_ids().contains(&task_id));
    }
}
