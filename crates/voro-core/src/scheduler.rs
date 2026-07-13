//! The scheduler (DESIGN.md §7): pure scoring and the ordering of the two
//! views. The store supplies candidates (with `age_days` already computed);
//! everything here is deterministic arithmetic on those rows.

use crate::error::Result;
use crate::model::{Priority, Task, TaskState};
use crate::store::{Store, task_from_row};

/// The score decomposition — every term visible (§7, §12).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreBreakdown {
    pub weight: i64,
    pub priority: Priority,
    pub priority_value: f64,
    pub state: TaskState,
    /// Static per-state nudge folded into the priority term (§7).
    pub state_bonus: f64,
    /// weight × (priority_value + state_bonus)
    pub base: f64,
    pub age_days: f64,
    /// 0.1 × age_days, capped at 2
    pub age_bonus: f64,
    pub total: f64,
}

/// A static per-state weight folded into the priority term (§7), ranking
/// human-attention states above plain startable work: `needs-input` (blocks an
/// idle agent) outweighs `review` and `stalled`; `ready` and `proposed` earn
/// nothing.
pub fn state_bonus(state: TaskState) -> f64 {
    match state {
        TaskState::NeedsInput => 4.0,
        TaskState::Review | TaskState::Stalled => 2.0,
        _ => 0.0,
    }
}

pub fn score(weight: i64, priority: Priority, state: TaskState, age_days: f64) -> ScoreBreakdown {
    let priority_value = priority.value();
    let state_bonus = state_bonus(state);
    let base = weight as f64 * (priority_value + state_bonus);
    let age_bonus = (0.1 * age_days).min(2.0);
    ScoreBreakdown {
        weight,
        priority,
        priority_value,
        state,
        state_bonus,
        base,
        age_days,
        age_bonus,
        total: base + age_bonus,
    }
}

/// A task joined with what the scheduler needs to rank it.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub task: Task,
    pub project_name: String,
    pub score: ScoreBreakdown,
}

/// How many rows the queue offers: enough to pick around the top item, few
/// enough that the queue stays an answer rather than the whole backlog. A single
/// cap across every state, since each row is one next action on the same score
/// (§7).
pub const QUEUE_MAX_ROWS: usize = 10;

/// The next-action queue (§1): the `QUEUE_MAX_ROWS` highest-scoring tasks
/// across every actionable state, in one list ordered by score. The cap is
/// uniform — every state competes for the same slots on score alone, so a
/// low-scoring row of any state can fall below the cut (§7).
pub fn queue(candidates: &[Candidate]) -> Vec<&Candidate> {
    let mut items: Vec<&Candidate> = candidates.iter().collect();
    items.sort_by(|a, b| rank(a, b));
    items.truncate(QUEUE_MAX_ROWS);
    items
}

/// The single highest-scoring `ready` task — what `voro next` hands an agent
/// asking for work. Deliberately `ready`-only: a `stalled` task needs
/// redispatching with its prior session's context, not fresh work (§7).
pub fn focus(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.task.state == TaskState::Ready)
        .min_by(|a, b| rank(a, b))
}

/// Total order for views: score desc. Score already folds in the per-state
/// bonus (§7), so the `state_rank` tiebreak only decides genuinely equal totals,
/// where an unanswered question outranks a finished diff, startable work, then
/// an untriaged proposal (§6). Priority, older `state_since`, then id tail it.
fn rank(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    b.score
        .total
        .total_cmp(&a.score.total)
        .then_with(|| state_rank(a.task.state).cmp(&state_rank(b.task.state)))
        .then_with(|| a.task.priority.cmp(&b.task.priority))
        .then_with(|| a.task.state_since.cmp(&b.task.state_since))
        .then_with(|| a.task.id.cmp(&b.task.id))
}

fn state_rank(state: TaskState) -> u8 {
    match state {
        TaskState::NeedsInput => 0,
        TaskState::Review => 1,
        TaskState::Stalled => 2,
        TaskState::Ready => 3,
        _ => 4,
    }
}

impl Store {
    /// Scheduler input: every task in a scored state, joined with its
    /// project, excluding weight-0 (parked) projects entirely (§7).
    pub fn candidates(&self) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.project_id, t.title, t.body, t.priority, t.state, t.agent,
                    t.question, t.pr_url, t.branch, t.state_since, t.created_at, t.closed_at,
                    t.human, p.name, p.weight,
                    julianday('now') - julianday(t.state_since)
             FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE p.weight > 0
               AND t.state IN ('ready','needs-input','review','stalled','proposed')",
        )?;
        let rows = stmt.query_map([], |row| {
            let task = task_from_row(row)?;
            let project_name: String = row.get(14)?;
            let weight: i64 = row.get(15)?;
            let age_days: f64 = row.get(16)?;
            let score = score(weight, task.priority, task.state, age_days);
            Ok(Candidate {
                task,
                project_name,
                score,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Score decomposition for any single task, whatever its state — the
    /// TUI popup today, `voro explain <task>` later.
    pub fn explain(&self, task_id: i64) -> Result<ScoreBreakdown> {
        let (weight, priority, state, age_days): (i64, Priority, TaskState, f64) =
            self.conn.query_row(
                "SELECT p.weight, t.priority, t.state,
                        julianday('now') - julianday(t.state_since)
             FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE t.id = ?1",
                [task_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
        Ok(score(weight, priority, state, age_days))
    }

    /// Count of untriaged tasks. Parked (weight-0) projects are hidden
    /// here too.
    pub fn proposed_count(&self) -> Result<i64> {
        Ok(self.state_counts()?.proposed)
    }

    /// Task counts by state for the header indicator (DESIGN.md §12), so a
    /// backlog stays felt even when a low-scoring row falls past the queue's
    /// cap (§7). Parked (weight-0) projects are excluded.
    pub fn state_counts(&self) -> Result<StateCounts> {
        let mut stmt = self.conn.prepare(
            "SELECT t.state, COUNT(*) FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE p.weight > 0 GROUP BY t.state",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, TaskState>(0)?, r.get::<_, i64>(1)?)))?;
        let mut counts = StateCounts::default();
        for row in rows {
            let (state, n) = row?;
            match state {
                TaskState::Proposed => counts.proposed = n,
                TaskState::Ready => counts.ready = n,
                TaskState::Running => counts.running = n,
                TaskState::NeedsInput => counts.needs_input = n,
                TaskState::Review => counts.review = n,
                TaskState::Stalled => counts.stalled = n,
                TaskState::Done => counts.done = n,
                TaskState::Parked | TaskState::Rejected => {}
            }
        }
        Ok(counts)
    }
}

/// Task counts by state, for the persistent header indicator (DESIGN.md §12).
/// Parked and rejected tasks earn no field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateCounts {
    pub proposed: i64,
    pub ready: i64,
    pub running: i64,
    pub needs_input: i64,
    pub review: i64,
    pub stalled: i64,
    pub done: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskState;
    use crate::store::NewTask;

    #[test]
    fn worked_example_from_design_doc() {
        // P0 in a weight-2 project (16) beats P2 in a weight-5 project (10).
        let p0_low_weight = score(2, Priority::P0, TaskState::Ready, 0.0);
        let p2_high_weight = score(5, Priority::P2, TaskState::Ready, 0.0);
        assert_eq!(p0_low_weight.total, 16.0);
        assert_eq!(p2_high_weight.total, 10.0);
        assert!(p0_low_weight.total > p2_high_weight.total);
    }

    #[test]
    fn priority_values_are_geometric() {
        assert_eq!(score(1, Priority::P0, TaskState::Ready, 0.0).total, 8.0);
        assert_eq!(score(1, Priority::P1, TaskState::Ready, 0.0).total, 4.0);
        assert_eq!(score(1, Priority::P2, TaskState::Ready, 0.0).total, 2.0);
        assert_eq!(score(1, Priority::P3, TaskState::Ready, 0.0).total, 1.0);
    }

    #[test]
    fn age_bonus_grows_then_caps_at_two() {
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 0.0).age_bonus, 0.0);
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 5.0).age_bonus, 0.5);
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 20.0).age_bonus,
            2.0
        );
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 365.0).age_bonus,
            2.0
        );
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 365.0).total, 8.0);
    }

    #[test]
    fn decomposition_terms_sum_to_total() {
        let s = score(4, Priority::P1, TaskState::Ready, 7.3);
        assert_eq!(s.base, 16.0);
        assert_eq!(s.total, s.base + s.age_bonus);
    }

    #[test]
    fn state_bonus_folds_into_the_priority_term() {
        // needs-input +4, review +2, everything else nothing — multiplied by
        // project weight just like priority.
        assert_eq!(state_bonus(TaskState::NeedsInput), 4.0);
        assert_eq!(state_bonus(TaskState::Review), 2.0);
        assert_eq!(state_bonus(TaskState::Ready), 0.0);
        assert_eq!(state_bonus(TaskState::Proposed), 0.0);

        // P2 in a weight-3 project: 3×(2+4) = 18 as a question, 3×2 = 6 ready.
        assert_eq!(
            score(3, Priority::P2, TaskState::NeedsInput, 0.0).base,
            18.0
        );
        assert_eq!(score(3, Priority::P2, TaskState::Review, 0.0).base, 12.0);
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 0.0).base, 6.0);
    }

    // --- ordering over a real store ---

    fn setup() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn add_project(s: &mut Store, name: &str, weight: i64) -> i64 {
        let p = s.create_project(name, "/tmp").unwrap();
        s.set_weight(p.id, weight).unwrap();
        p.id
    }

    fn add_task(s: &mut Store, project_id: i64, title: &str, priority: Priority) -> i64 {
        s.create_task(NewTask {
            project_id,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Ready,
            agent: None,
            human: false,
        })
        .unwrap()
        .id
    }

    fn to_needs_input(s: &mut Store, id: i64) {
        s.apply(id, crate::Action::Start).unwrap();
        s.apply(id, crate::Action::Ask("?".into())).unwrap();
    }

    fn to_review(s: &mut Store, id: i64) {
        s.apply(id, crate::Action::Start).unwrap();
        s.apply(id, crate::Action::Complete(None)).unwrap();
    }

    fn to_stalled(s: &mut Store, id: i64) {
        let (_, session) = s.record_dispatch(id, "claude", Some(1), None).unwrap();
        s.reconcile_session(session.id, false, false).unwrap();
    }

    fn add_proposed(s: &mut Store, project_id: i64, title: &str, priority: Priority) -> i64 {
        s.create_task(NewTask {
            project_id,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Proposed,
            agent: None,
            human: false,
        })
        .unwrap()
        .id
    }

    fn set_age_days(s: &mut Store, id: i64, days: f64) {
        s.conn
            .execute(
                "UPDATE tasks SET state_since = datetime('now', ?1 || ' days') WHERE id = ?2",
                (format!("-{days}"), id),
            )
            .unwrap();
    }

    #[test]
    fn focus_picks_the_worked_example_winner() {
        let mut s = setup();
        let side = add_project(&mut s, "side-project", 2);
        let main = add_project(&mut s, "main-project", 5);
        let p0 = add_task(&mut s, side, "urgent fix", Priority::P0);
        add_task(&mut s, main, "nice to have", Priority::P2);

        let candidates = s.candidates().unwrap();
        let top = focus(&candidates).unwrap();
        assert_eq!(top.task.id, p0);
        assert_eq!(top.score.base, 16.0);
    }

    #[test]
    fn queue_interleaves_every_actionable_state_by_score() {
        let mut s = setup();
        let a = add_project(&mut s, "a", 3);
        let b = add_project(&mut s, "b", 1);

        let question = add_task(&mut s, a, "question", Priority::P2); // 3×(2+4) = 18
        to_needs_input(&mut s, question);
        let diff = add_task(&mut s, a, "diff", Priority::P0); // 3×(8+2) = 30
        to_review(&mut s, diff);
        let small = add_task(&mut s, b, "small question", Priority::P3); // 1×(1+4) = 5
        to_needs_input(&mut s, small);
        let ready = add_task(&mut s, a, "ready task", Priority::P1); // 3×4 = 12
        let proposed = add_proposed(&mut s, a, "proposal", Priority::P2); // 3×2 = 6

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        // the state bonus lifts the P2 question over the P1 ready task
        assert_eq!(ids, vec![diff, question, ready, proposed, small]);
    }

    #[test]
    fn queue_caps_at_the_highest_scoring_rows() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let tasks: Vec<i64> = (0..QUEUE_MAX_ROWS + 4)
            .map(|i| {
                let id = add_task(&mut s, p, &format!("t{i}"), Priority::P2);
                // older tasks score higher via the age bonus, so ordering is
                // deterministic: index 0 oldest, last youngest.
                set_age_days(&mut s, id, (QUEUE_MAX_ROWS + 4 - i) as f64);
                id
            })
            .collect();

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids.len(), QUEUE_MAX_ROWS);
        assert_eq!(ids, tasks[..QUEUE_MAX_ROWS]);
    }

    #[test]
    fn the_cap_drops_a_low_scoring_attention_item_regardless_of_state() {
        // The cap is uniform across state: a low-scoring question can fall
        // below it just like a ready task, once enough higher-scoring work
        // exists. Ten P0 ready tasks in a heavy project (score 40) fill the cap
        // and push a lone P3 question in a light project (1×(1+4) = 5) off.
        let mut s = setup();
        let heavy = add_project(&mut s, "heavy", 5);
        let light = add_project(&mut s, "light", 1);
        let loud: Vec<i64> = (0..QUEUE_MAX_ROWS)
            .map(|i| add_task(&mut s, heavy, &format!("loud {i}"), Priority::P0))
            .collect();
        let quiet_question = add_task(&mut s, light, "quiet question", Priority::P3);
        to_needs_input(&mut s, quiet_question);

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids.len(), QUEUE_MAX_ROWS);
        assert!(!ids.contains(&quiet_question));
        for id in &loud {
            assert!(ids.contains(id));
        }
    }

    #[test]
    fn state_bonus_lifts_a_question_over_an_equal_priority_review() {
        // Same weight and priority: the +4 needs-input bonus outscores the +2 review.
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let diff = add_task(&mut s, p, "diff", Priority::P1); // 3×(4+2) = 18
        to_review(&mut s, diff);
        let question = add_task(&mut s, p, "question", Priority::P1); // 3×(4+4) = 24
        to_needs_input(&mut s, question);

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![question, diff]);
    }

    #[test]
    fn a_stalled_task_scores_the_review_bonus_and_competes_in_the_queue() {
        // stalled earns +2, the same as review (§7).
        assert_eq!(state_bonus(TaskState::Stalled), 2.0);
        assert_eq!(score(3, Priority::P2, TaskState::Stalled, 0.0).base, 12.0);

        // A stalled P2 (3×(2+2) = 12) ties a ready P1 (3×4 = 12) exactly; the
        // state precedence slots stalled after review, before ready.
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let ready = add_task(&mut s, p, "ready", Priority::P1);
        let stalled = add_task(&mut s, p, "stalled", Priority::P2);
        to_stalled(&mut s, stalled);
        s.conn
            .execute(
                "UPDATE tasks SET state_since = '2020-01-01 00:00:00' WHERE id IN (?1, ?2)",
                (ready, stalled),
            )
            .unwrap();

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![stalled, ready]);
    }

    #[test]
    fn focus_never_hands_out_a_stalled_task() {
        // `voro next` answers with fresh startable work only, so even a stalled
        // task that outscores every ready task stays out of focus() while
        // still leading the queue.
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let stalled = add_task(&mut s, p, "stalled", Priority::P0);
        to_stalled(&mut s, stalled);
        let ready = add_task(&mut s, p, "ready", Priority::P3);

        let candidates = s.candidates().unwrap();
        assert_eq!(focus(&candidates).unwrap().task.id, ready);
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![stalled, ready]);
    }

    #[test]
    fn state_precedence_still_breaks_genuinely_equal_totals() {
        // Contrived so the folded scores collide: needs-input 3×(1+4) = 15,
        // review 5×(1+2) = 15. With ages pinned equal the totals tie exactly,
        // and the state precedence (§6) decides it.
        let mut s = setup();
        let a = add_project(&mut s, "a", 3);
        let b = add_project(&mut s, "b", 5);
        let diff = add_task(&mut s, b, "diff", Priority::P3);
        to_review(&mut s, diff);
        let question = add_task(&mut s, a, "question", Priority::P3);
        to_needs_input(&mut s, question);
        s.conn
            .execute(
                "UPDATE tasks SET state_since = '2020-01-01 00:00:00' WHERE id IN (?1, ?2)",
                (diff, question),
            )
            .unwrap();

        let candidates = s.candidates().unwrap();
        let total = |id| {
            candidates
                .iter()
                .find(|c| c.task.id == id)
                .unwrap()
                .score
                .total
        };
        assert_eq!(total(diff), total(question));
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![question, diff]);
    }

    #[test]
    fn age_bonus_breaks_priority_ties_and_starvation() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let fresh = add_task(&mut s, p, "fresh", Priority::P2);
        let stale = add_task(&mut s, p, "stale", Priority::P2);
        set_age_days(&mut s, stale, 10.0);

        let candidates = s.candidates().unwrap();
        let top = focus(&candidates).unwrap();
        assert_eq!(top.task.id, stale);
        assert!((top.score.age_bonus - 1.0).abs() < 0.01);

        // but capped age can never fake a priority level
        set_age_days(&mut s, stale, 300.0);
        let higher = add_task(&mut s, p, "actually urgent", Priority::P1);
        let candidates = s.candidates().unwrap();
        assert_eq!(focus(&candidates).unwrap().task.id, higher);
        let _ = fresh;
    }

    #[test]
    fn weight_zero_projects_are_hidden_everywhere() {
        let mut s = setup();
        let parked = add_project(&mut s, "parked", 0);
        let active = add_project(&mut s, "active", 1);

        let hidden_q = add_task(&mut s, parked, "hidden question", Priority::P0);
        to_needs_input(&mut s, hidden_q);
        add_task(&mut s, parked, "hidden ready", Priority::P0);
        s.create_task(NewTask {
            project_id: parked,
            title: "hidden proposed".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
        })
        .unwrap();
        let visible = add_task(&mut s, active, "visible", Priority::P3);

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![visible]);
        assert_eq!(focus(&candidates).unwrap().task.id, visible);
        assert_eq!(s.proposed_count().unwrap(), 0);
    }

    #[test]
    fn state_counts_group_by_state_and_hide_parked_projects() {
        let mut s = setup();
        let active = add_project(&mut s, "active", 3);
        let parked = add_project(&mut s, "parked", 0);

        add_task(&mut s, active, "r1", Priority::P2);
        add_task(&mut s, active, "r2", Priority::P2);
        s.create_task(NewTask {
            project_id: active,
            title: "idea".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
        })
        .unwrap();
        let question = add_task(&mut s, active, "blocked on me", Priority::P2);
        to_needs_input(&mut s, question);
        let reviewed = add_task(&mut s, active, "in review", Priority::P2);
        s.apply(reviewed, crate::Action::Start).unwrap();
        s.apply(reviewed, crate::Action::Complete(None)).unwrap();
        let stalled = add_task(&mut s, active, "died mid-run", Priority::P2);
        to_stalled(&mut s, stalled);

        // Everything in a parked (weight-0) project stays out of the tally.
        add_task(&mut s, parked, "hidden ready", Priority::P2);
        s.create_task(NewTask {
            project_id: parked,
            title: "hidden idea".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
        })
        .unwrap();

        let c = s.state_counts().unwrap();
        assert_eq!(c.ready, 2);
        assert_eq!(c.proposed, 1);
        assert_eq!(c.needs_input, 1);
        assert_eq!(c.review, 1);
        assert_eq!(c.stalled, 1);
        assert_eq!(c.running, 0);
        assert_eq!(c.done, 0);
        // proposed_count is the same guard-rail number the counts expose.
        assert_eq!(s.proposed_count().unwrap(), 1);
    }

    #[test]
    fn tasks_with_open_blockers_never_reach_the_queue() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let blocker = add_task(&mut s, p, "blocker", Priority::P2);
        let blocked = add_task(&mut s, p, "blocked", Priority::P0);
        s.add_dep(blocked, blocker, crate::DepKind::Blocks).unwrap();

        // the high-priority blocked task is out of the running until its
        // blocker closes — neither view offers it
        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = queue(&candidates).iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![blocker]);
        assert_eq!(focus(&candidates).unwrap().task.id, blocker);

        // once the blocker closes it surfaces, and now outranks it
        s.apply(blocker, crate::Action::Start).unwrap();
        s.apply(blocker, crate::Action::Complete(None)).unwrap();
        s.apply(blocker, crate::Action::Accept).unwrap();
        let candidates = s.candidates().unwrap();
        assert_eq!(focus(&candidates).unwrap().task.id, blocked);
    }

    #[test]
    fn deterministic_tail_ordering() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let first = add_task(&mut s, p, "first", Priority::P2);
        let second = add_task(&mut s, p, "second", Priority::P2);

        let candidates = s.candidates().unwrap();
        // identical score, state, priority, state_since → id ascending
        assert_eq!(focus(&candidates).unwrap().task.id, first);
        let _ = second;
    }
}
