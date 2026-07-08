//! The scheduler (DESIGN.md §7): pure scoring and the ordering of the two
//! views. The store supplies candidates (with `age_days` already computed);
//! everything in this module is deterministic arithmetic on those rows, so it
//! is trivially testable and identical beneath every interface.

use crate::error::Result;
use crate::model::{Priority, Task, TaskState};
use crate::store::{Store, task_from_row};

/// The score decomposition — every term visible so the number can be
/// distrusted productively (§7, §12).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreBreakdown {
    pub weight: i64,
    pub priority: Priority,
    pub priority_value: f64,
    /// weight × priority_value
    pub base: f64,
    pub age_days: f64,
    /// 0.1 × age_days, capped at 2
    pub age_bonus: f64,
    pub total: f64,
}

pub fn score(weight: i64, priority: Priority, age_days: f64) -> ScoreBreakdown {
    let priority_value = priority.value();
    let base = weight as f64 * priority_value;
    let age_bonus = (0.1 * age_days).min(2.0);
    ScoreBreakdown {
        weight,
        priority,
        priority_value,
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

/// All `needs-input` and `review` tasks, sorted by score. The "triage N
/// proposed tasks" meta-item is a count, not a task; interfaces render it
/// from `Store::proposed_count`.
pub fn inbox(candidates: &[Candidate]) -> Vec<&Candidate> {
    let mut items: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| matches!(c.task.state, TaskState::NeedsInput | TaskState::Review))
        .collect();
    items.sort_by(|a, b| rank(a, b));
    items
}

/// The single highest-scoring `ready` task.
pub fn focus(candidates: &[Candidate]) -> Option<&Candidate> {
    candidates
        .iter()
        .filter(|c| c.task.state == TaskState::Ready)
        .min_by(|a, b| rank(a, b))
}

/// Total order for views: score desc; then an unanswered question outranks a
/// finished diff (§6); then priority, older `state_since`, id — the tail
/// exists only to make the ordering deterministic.
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
        _ => 2,
    }
}

impl Store {
    /// Scheduler input: every task in a scored state, joined with its
    /// project, excluding weight-0 (parked) projects entirely (§7).
    pub fn candidates(&self) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.project_id, t.title, t.body, t.priority, t.state, t.agent,
                    t.question, t.state_since, t.created_at, t.closed_at,
                    p.name, p.weight,
                    julianday('now') - julianday(t.state_since)
             FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE p.weight > 0 AND t.state IN ('ready','needs-input','review')",
        )?;
        let rows = stmt.query_map([], |row| {
            let task = task_from_row(row)?;
            let project_name: String = row.get(11)?;
            let weight: i64 = row.get(12)?;
            let age_days: f64 = row.get(13)?;
            let score = score(weight, task.priority, age_days);
            Ok(Candidate {
                task,
                project_name,
                score,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Size of the standing "triage N proposed tasks" inbox entry. Parked
    /// (weight-0) projects are hidden here too.
    pub fn proposed_count(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE t.state = 'proposed' AND p.weight > 0",
            [],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskState;
    use crate::store::NewTask;

    #[test]
    fn worked_example_from_design_doc() {
        // P0 in a weight-2 project (16) beats P2 in a weight-5 project (10).
        let p0_low_weight = score(2, Priority::P0, 0.0);
        let p2_high_weight = score(5, Priority::P2, 0.0);
        assert_eq!(p0_low_weight.total, 16.0);
        assert_eq!(p2_high_weight.total, 10.0);
        assert!(p0_low_weight.total > p2_high_weight.total);
    }

    #[test]
    fn priority_values_are_geometric() {
        assert_eq!(score(1, Priority::P0, 0.0).total, 8.0);
        assert_eq!(score(1, Priority::P1, 0.0).total, 4.0);
        assert_eq!(score(1, Priority::P2, 0.0).total, 2.0);
        assert_eq!(score(1, Priority::P3, 0.0).total, 1.0);
    }

    #[test]
    fn age_bonus_grows_then_caps_at_two() {
        assert_eq!(score(3, Priority::P2, 0.0).age_bonus, 0.0);
        assert_eq!(score(3, Priority::P2, 5.0).age_bonus, 0.5);
        assert_eq!(score(3, Priority::P2, 20.0).age_bonus, 2.0);
        assert_eq!(score(3, Priority::P2, 365.0).age_bonus, 2.0);
        assert_eq!(score(3, Priority::P2, 365.0).total, 8.0);
    }

    #[test]
    fn decomposition_terms_sum_to_total() {
        let s = score(4, Priority::P1, 7.3);
        assert_eq!(s.base, 16.0);
        assert_eq!(s.total, s.base + s.age_bonus);
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
        s.apply(id, crate::Action::Complete).unwrap();
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
    fn inbox_contains_exactly_needs_input_and_review_sorted_by_score() {
        let mut s = setup();
        let a = add_project(&mut s, "a", 3);
        let b = add_project(&mut s, "b", 1);

        let question = add_task(&mut s, a, "question", Priority::P2); // 3×2 = 6
        to_needs_input(&mut s, question);
        let diff = add_task(&mut s, a, "diff", Priority::P0); // 3×8 = 24
        to_review(&mut s, diff);
        let small = add_task(&mut s, b, "small question", Priority::P3); // 1×1 = 1
        to_needs_input(&mut s, small);
        add_task(&mut s, a, "ready task", Priority::P0); // not inbox material

        let candidates = s.candidates().unwrap();
        let inbox = inbox(&candidates);
        let ids: Vec<i64> = inbox.iter().map(|c| c.task.id).collect();
        assert_eq!(ids, vec![diff, question, small]);
    }

    #[test]
    fn question_outranks_review_at_equal_score() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let diff = add_task(&mut s, p, "diff", Priority::P1);
        to_review(&mut s, diff);
        let question = add_task(&mut s, p, "question", Priority::P1);
        to_needs_input(&mut s, question);

        let candidates = s.candidates().unwrap();
        let ids: Vec<i64> = inbox(&candidates).iter().map(|c| c.task.id).collect();
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
        })
        .unwrap();
        let visible = add_task(&mut s, active, "visible", Priority::P3);

        let candidates = s.candidates().unwrap();
        assert!(inbox(&candidates).is_empty());
        assert_eq!(focus(&candidates).unwrap().task.id, visible);
        assert_eq!(s.proposed_count().unwrap(), 0);
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
