//! The scheduler (DESIGN.md §7): pure scoring and the ordering of the two
//! views. The store supplies candidates (with `age_days` already computed);
//! everything here is deterministic arithmetic on those rows.

use crate::error::Result;
use crate::model::{NextAction, Priority, Task, TaskState};
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
    /// Open tasks with a `blocks` dependency on this one (§7).
    pub open_dependents: i64,
    /// 1 × open_dependents, capped at 2
    pub unblock_bonus: f64,
    /// weight × (priority_value + state_bonus + unblock_bonus)
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

/// A nudge for a task other open work is parked behind (§7): one point per
/// direct open `blocks` dependent, capped at two so unblocking never
/// masquerades as a priority level.
pub fn unblock_bonus(open_dependents: i64) -> f64 {
    (open_dependents.max(0) as f64).min(2.0)
}

pub fn score(
    weight: i64,
    priority: Priority,
    state: TaskState,
    age_days: f64,
    open_dependents: i64,
) -> ScoreBreakdown {
    let priority_value = priority.value();
    let state_bonus = state_bonus(state);
    let unblock_bonus = unblock_bonus(open_dependents);
    let base = weight as f64 * (priority_value + state_bonus + unblock_bonus);
    let age_bonus = (0.1 * age_days).min(2.0);
    ScoreBreakdown {
        weight,
        priority,
        priority_value,
        state,
        state_bonus,
        open_dependents,
        unblock_bonus,
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

/// How many dispatches may be in flight before the queue stops offering more
/// (§7). Overridable as `max_running` in `voro.toml`.
pub const DEFAULT_MAX_RUNNING: i64 = 5;

/// What each next action costs the operator's attention (§7) — the divisor
/// that turns a raw score into the effective one the queue ranks by. The band
/// is deliberately narrow, so pricing nudges the order rather than overturning
/// it: priority still dominates within an action kind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AttentionCosts {
    /// Answering a question: a decision, not a work session.
    pub answer: f64,
    /// Triaging a proposal: a minute at most.
    pub triage: f64,
    /// Handing a task to an agent. Near-instant, so its real cost is the
    /// concurrency slot the WIP gate meters rather than this divisor.
    pub dispatch: f64,
    /// Reviewing a diff, locally or on a PR: the expensive one.
    pub review: f64,
    /// Doing a human-only task by hand: the most expensive of all.
    pub human_do: f64,
}

impl Default for AttentionCosts {
    fn default() -> AttentionCosts {
        AttentionCosts {
            answer: 0.8,
            triage: 0.8,
            dispatch: 1.0,
            review: 1.4,
            human_do: 1.8,
        }
    }
}

impl AttentionCosts {
    /// The divisor for one next action. `redispatch` prices as `dispatch`
    /// because it *is* one — the operator's move is the same keypress, and it
    /// opens the same session; `pr` and `review PR` are the same review either
    /// way, differing only in the medium the diff arrives on (§3).
    pub fn of(&self, action: NextAction) -> f64 {
        match action {
            NextAction::Answer => self.answer,
            NextAction::Triage => self.triage,
            NextAction::Dispatch | NextAction::Redispatch => self.dispatch,
            NextAction::Pr | NextAction::ReviewPr => self.review,
            NextAction::Do => self.human_do,
        }
    }
}

/// Whether an action starts an agent, and so spends a concurrency slot rather
/// than only the operator's attention (§7).
fn opens_a_session(action: NextAction) -> bool {
    matches!(action, NextAction::Dispatch | NextAction::Redispatch)
}

/// The dispatch work-in-progress gate (§7): how many tasks are running against
/// how many the operator will carry at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WipGate {
    pub running: i64,
    pub max_running: i64,
}

impl WipGate {
    pub fn at_capacity(&self) -> bool {
        self.running >= self.max_running
    }
}

/// One row of the queue, priced by what it asks of the operator.
// A task row carries a whole `Candidate` and a digest only a summary, so the
// variants differ in size; boxing would buy an indirection over a list capped
// at ten rows.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum QueueRow {
    /// A single task and its one next action.
    Action(ActionRow),
    /// A project's untriaged proposals, collapsed into one triage row (§7).
    Digest(DigestRow),
}

/// A task's row: the candidate, the verb it asks for, and what that verb costs.
#[derive(Debug, Clone)]
pub struct ActionRow {
    pub candidate: Candidate,
    pub action: NextAction,
    pub cost: f64,
    /// `score / cost` — what the queue ranks by.
    pub effective: f64,
}

/// One project's proposals, collapsed so a triage backlog cannot swamp the
/// queue with cheap rows (§7). Scored as its best child, so the digest survives
/// the cut exactly when that child would have.
#[derive(Debug, Clone)]
pub struct DigestRow {
    pub project_name: String,
    /// The constituent proposals, in the order they would have ranked.
    pub tasks: Vec<ActionRow>,
    pub effective: f64,
}

/// The queue as rendered: its rows, plus the dispatch gate's state when it is
/// suppressing rows (§7).
#[derive(Debug, Clone)]
pub struct Queue {
    pub rows: Vec<QueueRow>,
    /// `Some` while dispatch is at capacity, carrying the counts the capacity
    /// line names in place of the suppressed rows.
    pub at_capacity: Option<WipGate>,
}

impl QueueRow {
    pub fn effective(&self) -> f64 {
        match self {
            QueueRow::Action(row) => row.effective,
            QueueRow::Digest(row) => row.effective,
        }
    }

    /// The candidate a row's tie-break reads: its own for a task row, its best
    /// child's for a digest.
    fn ranking_candidate(&self) -> Option<&Candidate> {
        match self {
            QueueRow::Action(row) => Some(&row.candidate),
            QueueRow::Digest(row) => row.tasks.first().map(|row| &row.candidate),
        }
    }
}

/// The next-action queue (§1): the `QUEUE_MAX_ROWS` highest-*effective*-scoring
/// next actions, in one list. The cap is uniform — every state competes for the
/// same slots, so a low-scoring row of any kind can fall below the cut (§7) —
/// but what competes is the attention price `score / cost(action)`, so a cheap
/// decision outranks an expensive review of the same raw worth.
///
/// Two rows are not priced but shaped: dispatch is metered by the WIP gate
/// rather than a divisor, so at capacity its rows leave the queue entirely; and
/// proposals collapse into one digest row per project, since at a divisor below
/// one a large triage backlog would otherwise crowd out everything else.
pub fn queue(candidates: &[Candidate], costs: &AttentionCosts, gate: WipGate) -> Queue {
    let at_capacity = gate.at_capacity();
    let mut actions: Vec<ActionRow> = Vec::new();
    for candidate in candidates {
        let Some(action) = candidate.task.next_action() else {
            continue;
        };
        if at_capacity && opens_a_session(action) {
            continue;
        }
        let cost = costs.of(action);
        actions.push(ActionRow {
            effective: candidate.score.total / cost,
            candidate: candidate.clone(),
            action,
            cost,
        });
    }

    let mut rows = collapse_proposals(actions);
    rows.sort_by(rank_rows);
    rows.truncate(QUEUE_MAX_ROWS);
    Queue {
        rows,
        at_capacity: at_capacity.then_some(gate),
    }
}

/// Fold every triage row into one digest per project, leaving the rest as they
/// are. Each digest takes its best child's effective score, so it competes for
/// a slot exactly as that child would have.
fn collapse_proposals(actions: Vec<ActionRow>) -> Vec<QueueRow> {
    let mut by_project: Vec<(String, Vec<ActionRow>)> = Vec::new();
    let mut rows: Vec<QueueRow> = Vec::new();
    for row in actions {
        if row.action != NextAction::Triage {
            rows.push(QueueRow::Action(row));
            continue;
        }
        let project = row.candidate.project_name.clone();
        match by_project.iter_mut().find(|(name, _)| *name == project) {
            Some((_, tasks)) => tasks.push(row),
            None => by_project.push((project, vec![row])),
        }
    }
    rows.extend(by_project.into_iter().map(|(project_name, mut tasks)| {
        tasks.sort_by(|a, b| rank(&a.candidate, &b.candidate));
        let effective = tasks
            .iter()
            .map(|row| row.effective)
            .fold(f64::NEG_INFINITY, f64::max);
        QueueRow::Digest(DigestRow {
            project_name,
            tasks,
            effective,
        })
    }));
    rows
}

/// Total order for the queue: effective score desc, then the same tie-break
/// chain the raw score uses (§6/§7). A digest breaks ties on its best child, so
/// it sits exactly where that child would have.
fn rank_rows(a: &QueueRow, b: &QueueRow) -> std::cmp::Ordering {
    b.effective().total_cmp(&a.effective()).then_with(|| {
        match (a.ranking_candidate(), b.ranking_candidate()) {
            (Some(a), Some(b)) => rank(a, b),
            (a, b) => a.is_none().cmp(&b.is_none()),
        }
    })
}

/// What a task's raw score becomes once priced by its next action (§7) — the
/// division `explain` and the TUI decomposition show beside the total.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveScore {
    pub action: NextAction,
    pub cost: f64,
    pub effective: f64,
}

/// The attention price of one task, or `None` for a state that asks nothing of
/// the operator and so never renders a queue row (§3).
pub fn effective_score(task: &Task, total: f64, costs: &AttentionCosts) -> Option<EffectiveScore> {
    let action = task.next_action()?;
    let cost = costs.of(action);
    Some(EffectiveScore {
        action,
        cost,
        effective: total / cost,
    })
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
    /// project, excluding weight-0 (parked) and archived projects entirely
    /// (§5/§7). The open-dependent count arrives as one grouped join, not a
    /// lookup per row.
    pub fn candidates(&self) -> Result<Vec<Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.project_id, t.title, t.body, t.priority, t.state, t.agent,
                    t.question, t.pr_url, t.branch, t.state_since, t.created_at, t.closed_at,
                    t.human, t.repo_id, t.deep, p.name, p.weight,
                    julianday('now') - julianday(t.state_since),
                    COALESCE(b.open_dependents, 0)
             FROM tasks t JOIN projects p ON p.id = t.project_id
             LEFT JOIN (SELECT d.depends_on AS blocker_id, COUNT(*) AS open_dependents
                        FROM deps d JOIN tasks dt ON dt.id = d.task_id
                        WHERE d.kind = 'blocks' AND dt.state NOT IN ('done','rejected')
                        GROUP BY d.depends_on) b ON b.blocker_id = t.id
             WHERE p.weight > 0 AND p.archived = 0
               AND t.state IN ('ready','needs-input','review','stalled','proposed')",
        )?;
        let rows = stmt.query_map([], |row| {
            let task = task_from_row(row)?;
            let project_name: String = row.get(16)?;
            let weight: i64 = row.get(17)?;
            let age_days: f64 = row.get(18)?;
            let open_dependents: i64 = row.get(19)?;
            let score = score(weight, task.priority, task.state, age_days, open_dependents);
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
        let (weight, priority, state, age_days, open_dependents): (
            i64,
            Priority,
            TaskState,
            f64,
            i64,
        ) = self.conn.query_row(
            "SELECT p.weight, t.priority, t.state,
                    julianday('now') - julianday(t.state_since),
                    (SELECT COUNT(*) FROM deps d JOIN tasks dt ON dt.id = d.task_id
                     WHERE d.depends_on = t.id AND d.kind = 'blocks'
                       AND dt.state NOT IN ('done','rejected'))
             FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE t.id = ?1",
            [task_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        Ok(score(weight, priority, state, age_days, open_dependents))
    }

    /// Count of untriaged tasks. Parked (weight-0) and archived projects are
    /// hidden here too.
    pub fn proposed_count(&self) -> Result<i64> {
        Ok(self.state_counts()?.proposed)
    }

    /// Task counts by state for the header indicator (DESIGN.md §12), so a
    /// backlog stays felt even when a low-scoring row falls past the queue's
    /// cap (§7). Parked (weight-0) and archived projects are excluded.
    pub fn state_counts(&self) -> Result<StateCounts> {
        let mut stmt = self.conn.prepare(
            "SELECT t.state, COUNT(*) FROM tasks t JOIN projects p ON p.id = t.project_id
             WHERE p.weight > 0 AND p.archived = 0 GROUP BY t.state",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, TaskState>(0)?, r.get::<_, i64>(1)?)))?;
        let mut counts = StateCounts::default();
        for row in rows {
            let (state, n) = row?;
            match state {
                TaskState::Proposed => counts.proposed = n,
                TaskState::Refining => counts.refining = n,
                TaskState::Ready => counts.ready = n,
                TaskState::Running => counts.running = n,
                TaskState::NeedsInput => counts.needs_input = n,
                TaskState::Review => counts.review = n,
                TaskState::Waiting => counts.waiting = n,
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
    /// Proposals an agent is rewriting right now (DESIGN.md §6). Counted apart
    /// from `proposed`, which they have temporarily left, so the triage backlog
    /// stays felt while a round is in flight.
    pub refining: i64,
    pub ready: i64,
    pub running: i64,
    pub needs_input: i64,
    pub review: i64,
    pub waiting: i64,
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
        let p0_low_weight = score(2, Priority::P0, TaskState::Ready, 0.0, 0);
        let p2_high_weight = score(5, Priority::P2, TaskState::Ready, 0.0, 0);
        assert_eq!(p0_low_weight.total, 16.0);
        assert_eq!(p2_high_weight.total, 10.0);
        assert!(p0_low_weight.total > p2_high_weight.total);
    }

    #[test]
    fn priority_values_are_geometric() {
        assert_eq!(score(1, Priority::P0, TaskState::Ready, 0.0, 0).total, 8.0);
        assert_eq!(score(1, Priority::P1, TaskState::Ready, 0.0, 0).total, 4.0);
        assert_eq!(score(1, Priority::P2, TaskState::Ready, 0.0, 0).total, 2.0);
        assert_eq!(score(1, Priority::P3, TaskState::Ready, 0.0, 0).total, 1.0);
    }

    #[test]
    fn age_bonus_grows_then_caps_at_two() {
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 0.0, 0).age_bonus,
            0.0
        );
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 5.0, 0).age_bonus,
            0.5
        );
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 20.0, 0).age_bonus,
            2.0
        );
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 365.0, 0).age_bonus,
            2.0
        );
        assert_eq!(
            score(3, Priority::P2, TaskState::Ready, 365.0, 0).total,
            8.0
        );
    }

    #[test]
    fn decomposition_terms_sum_to_total() {
        let s = score(4, Priority::P1, TaskState::Ready, 7.3, 0);
        assert_eq!(s.base, 16.0);
        assert_eq!(s.total, s.base + s.age_bonus);
    }

    #[test]
    fn unblock_bonus_counts_dependents_and_caps_at_two() {
        assert_eq!(unblock_bonus(0), 0.0);
        assert_eq!(unblock_bonus(1), 1.0);
        assert_eq!(unblock_bonus(2), 2.0);
        assert_eq!(unblock_bonus(9), 2.0);

        // priced inside the weight multiply like the state bonus: a P2 in a
        // weight-3 project is 3×2 = 6 blocking nothing, 3×(2+1) = 9 blocking
        // one task, 3×(2+2) = 12 blocking three or more.
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 0.0, 0).base, 6.0);
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 0.0, 1).base, 9.0);
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 0.0, 3).base, 12.0);

        // and it stays a decomposition term the total accounts for
        let s = score(3, Priority::P2, TaskState::Ready, 10.0, 1);
        assert_eq!(s.open_dependents, 1);
        assert_eq!(s.unblock_bonus, 1.0);
        assert_eq!(s.total, s.base + s.age_bonus);
    }

    #[test]
    fn the_unblock_bonus_applies_to_every_scored_state() {
        // like the age bonus: a dependency edge is an operator/graph fact, so
        // even an untriaged proposal earns it (§7).
        for state in [
            TaskState::Ready,
            TaskState::NeedsInput,
            TaskState::Review,
            TaskState::Stalled,
            TaskState::Proposed,
        ] {
            let plain = score(2, Priority::P2, state, 0.0, 0);
            let blocking = score(2, Priority::P2, state, 0.0, 1);
            assert_eq!(blocking.base - plain.base, 2.0, "{state}");
        }
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
            score(3, Priority::P2, TaskState::NeedsInput, 0.0, 0).base,
            18.0
        );
        assert_eq!(score(3, Priority::P2, TaskState::Review, 0.0, 0).base, 12.0);
        assert_eq!(score(3, Priority::P2, TaskState::Ready, 0.0, 0).base, 6.0);
    }

    // --- ordering over a real store ---

    /// The gate with nothing in flight — the ordering tests are about pricing,
    /// not capacity, so they run with room to dispatch.
    fn open_gate() -> WipGate {
        WipGate {
            running: 0,
            max_running: DEFAULT_MAX_RUNNING,
        }
    }

    fn default_queue(s: &Store) -> Queue {
        queue(
            &s.candidates().unwrap(),
            &AttentionCosts::default(),
            open_gate(),
        )
    }

    /// Each row in order: a task row as its id, a digest as its project and
    /// count, so an assertion can name both kinds in one list.
    fn labels(q: &Queue) -> Vec<String> {
        q.rows
            .iter()
            .map(|row| match row {
                QueueRow::Action(row) => format!("#{}", row.candidate.task.id),
                QueueRow::Digest(d) => format!("▲{} {}", d.tasks.len(), d.project_name),
            })
            .collect()
    }

    /// Only the task rows, by id — for the many tests where no proposal is in
    /// play and ids read better than labels.
    fn task_ids(q: &Queue) -> Vec<i64> {
        q.rows
            .iter()
            .filter_map(|row| match row {
                QueueRow::Action(row) => Some(row.candidate.task.id),
                QueueRow::Digest(_) => None,
            })
            .collect()
    }

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
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
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

    fn to_waiting(s: &mut Store, id: i64) {
        s.apply(id, crate::Action::Start).unwrap();
        s.apply(id, crate::Action::Complete(None)).unwrap();
        s.apply(id, crate::Action::HandOff).unwrap();
    }

    fn add_proposed(s: &mut Store, project_id: i64, title: &str, priority: Priority) -> i64 {
        s.create_task(NewTask {
            project_id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Proposed,
            agent: None,
            human: false,
            deep: false,
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
    fn queue_interleaves_every_actionable_state_by_attention_price() {
        let mut s = setup();
        let a = add_project(&mut s, "a", 3);
        let b = add_project(&mut s, "b", 1);

        let question = add_task(&mut s, a, "question", Priority::P2); // 18 ÷0.8 = 22.5
        to_needs_input(&mut s, question);
        let diff = add_task(&mut s, a, "diff", Priority::P0); // 30 ÷1.4 = 21.4
        to_review(&mut s, diff);
        let small = add_task(&mut s, b, "small question", Priority::P3); // 5 ÷0.8 = 6.25
        to_needs_input(&mut s, small);
        let ready = add_task(&mut s, a, "ready task", Priority::P1); // 12 ÷1.0 = 12
        add_proposed(&mut s, a, "proposal", Priority::P2); // 6 ÷0.8 = 7.5, digested

        let rows = labels(&default_queue(&s));
        // The P0 review leads on raw score (30 against the question's 18) and
        // still loses the top row: 15–60 minutes of attention against a
        // decision. Priority keeps its grip inside each kind — the P2 question
        // is far above the P3 one — and the proposals ride as one digest row.
        assert_eq!(
            rows,
            vec![
                format!("#{question}"),
                format!("#{diff}"),
                format!("#{ready}"),
                "▲1 a".to_string(),
                format!("#{small}"),
            ]
        );
    }

    #[test]
    fn the_cost_band_stays_a_nudge_not_a_re_ranking() {
        // The worked example from DESIGN.md §7: within one project and one
        // weight, a P2 review (8.4 ÷1.4 = 6.0) ranks below a P2 triage digest
        // (8.4 ÷0.8 = 10.5) but above a P3 one (4.2 ÷0.8 = 5.25) — priority
        // still dominates the action kind.
        let mut s = setup();
        let p = add_project(&mut s, "p", 4);
        let other = add_project(&mut s, "other", 4);
        let diff = add_task(&mut s, p, "diff", Priority::P2); // 4×(2+2) = 16 ÷1.4 = 11.4
        to_review(&mut s, diff);
        add_proposed(&mut s, p, "close idea", Priority::P2); // 4×2 = 8 ÷0.8 = 10
        add_proposed(&mut s, other, "distant idea", Priority::P3); // 4×1 = 4 ÷0.8 = 5

        assert_eq!(
            labels(&default_queue(&s)),
            vec![
                format!("#{diff}"),
                "▲1 p".to_string(),
                "▲1 other".to_string()
            ]
        );

        // A P1 review (4×(4+2) = 24 ÷1.4 = 17.1) still beats the P2 digest —
        // one priority level is worth more than the whole cost band.
        let urgent = add_task(&mut s, p, "urgent diff", Priority::P1);
        to_review(&mut s, urgent);
        assert_eq!(labels(&default_queue(&s))[0], format!("#{urgent}"));
    }

    #[test]
    fn a_human_task_is_priced_above_a_dispatch_of_the_same_worth() {
        // `do` is the most expensive row there is: the operator executes it
        // personally, where a dispatch is a keypress (§7).
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let by_hand = add_task(&mut s, p, "solder the harness", Priority::P1);
        let existing = s.task(by_hand).unwrap();
        s.update_task(
            by_hand,
            crate::TaskEdit {
                title: existing.title.clone(),
                body: existing.body.clone(),
                priority: existing.priority,
                agent: None,
                human: true,
                deep: false,
            },
        )
        .unwrap();
        let dispatchable = add_task(&mut s, p, "write the driver", Priority::P1);

        // Identical raw score (3×4 = 12); the divisors split them 6.67 to 12.
        assert_eq!(
            labels(&default_queue(&s)),
            vec![format!("#{dispatchable}"), format!("#{by_hand}"),]
        );
    }

    // --- the dispatch WIP gate (§7) ---

    #[test]
    fn the_wip_gate_suppresses_dispatch_rows_only_at_the_cap() {
        // Dispatch is near-instant for the operator, so its true cost is the
        // concurrency slot, not attention: below the cap it is priced at 1.0
        // like anything else, and at the cap it leaves the queue entirely.
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let ready = add_task(&mut s, p, "startable", Priority::P0);
        let stalled = add_task(&mut s, p, "died mid-run", Priority::P0);
        to_stalled(&mut s, stalled);
        let question = add_task(&mut s, p, "question", Priority::P3);
        to_needs_input(&mut s, question);

        let at = |running, max_running| {
            queue(
                &s.candidates().unwrap(),
                &AttentionCosts::default(),
                WipGate {
                    running,
                    max_running,
                },
            )
        };

        // One below the cap: everything competes.
        let below = at(1, 2);
        assert_eq!(below.at_capacity, None);
        assert_eq!(task_ids(&below), vec![stalled, ready, question]);

        // At the cap: both rows that would open a session are gone —
        // redispatch is a dispatch, it just carries the dead session's context
        // — and the capacity line stands in for them.
        let at_cap = at(2, 2);
        assert_eq!(task_ids(&at_cap), vec![question]);
        assert_eq!(
            at_cap.at_capacity,
            Some(WipGate {
                running: 2,
                max_running: 2
            })
        );

        // Over the cap reads the same as at it; the gate is a floor, not an
        // equality (a hand-started task can put the fleet over).
        assert!(at(5, 2).at_capacity.is_some());
        assert_eq!(task_ids(&at(5, 2)), vec![question]);
    }

    #[test]
    fn the_wip_gate_leaves_a_human_task_alone() {
        // `do` spends the operator's hands, not a concurrency slot, so a
        // full fleet says nothing about whether it can be picked up.
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let by_hand = add_task(&mut s, p, "drive to the lab", Priority::P2);
        let existing = s.task(by_hand).unwrap();
        s.update_task(
            by_hand,
            crate::TaskEdit {
                title: existing.title.clone(),
                body: existing.body.clone(),
                priority: existing.priority,
                agent: None,
                human: true,
                deep: false,
            },
        )
        .unwrap();
        add_task(&mut s, p, "dispatchable", Priority::P0);

        let q = queue(
            &s.candidates().unwrap(),
            &AttentionCosts::default(),
            WipGate {
                running: 9,
                max_running: 5,
            },
        );
        assert_eq!(task_ids(&q), vec![by_hand]);
    }

    #[test]
    fn max_running_zero_stops_the_queue_offering_dispatches() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        add_task(&mut s, p, "startable", Priority::P0);

        let q = queue(
            &s.candidates().unwrap(),
            &AttentionCosts::default(),
            WipGate {
                running: 0,
                max_running: 0,
            },
        );
        assert!(q.rows.is_empty());
        assert!(q.at_capacity.is_some());
    }

    // --- the proposal digest (§7) ---

    #[test]
    fn proposals_collapse_into_one_digest_scored_as_its_best_child() {
        // Cheap rows must not swamp the queue: nine proposals at ÷0.8 would
        // otherwise fill it. One digest per project, scored as the best child,
        // so it survives the cut exactly when that child would have.
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let other = add_project(&mut s, "other", 3);
        let best = add_proposed(&mut s, p, "the good idea", Priority::P0); // 24 ÷0.8 = 30
        for i in 0..8 {
            add_proposed(&mut s, p, &format!("idea {i}"), Priority::P3);
        }
        add_proposed(&mut s, other, "elsewhere", Priority::P2);

        let q = default_queue(&s);
        assert_eq!(labels(&q), vec!["▲9 p", "▲1 other"]);
        let QueueRow::Digest(digest) = &q.rows[0] else {
            panic!("expected a digest, got {:?}", q.rows[0]);
        };
        // 24 ÷0.8, give or take the age bonus these tasks accrue as the test runs
        assert!(
            (digest.effective - 30.0).abs() < 0.1,
            "{}",
            digest.effective
        );
        // Its children are ordered as they would have ranked, best first, so
        // folding it open opens on the proposal worth triaging.
        assert_eq!(digest.tasks[0].candidate.task.id, best);
        // and no proposal renders as a row of its own
        assert!(task_ids(&q).is_empty());
    }

    #[test]
    fn a_digest_falls_below_the_cap_exactly_as_its_best_child_would() {
        let mut s = setup();
        let heavy = add_project(&mut s, "heavy", 5);
        let light = add_project(&mut s, "light", 1);
        for i in 0..QUEUE_MAX_ROWS {
            add_task(&mut s, heavy, &format!("loud {i}"), Priority::P3); // 5×1 = 5 ÷1.0
        }
        // 1×1 = 1 ÷0.8 = 1.25 against ten rows at 5: below the cut.
        add_proposed(&mut s, light, "quiet idea", Priority::P3);

        let q = default_queue(&s);
        assert_eq!(q.rows.len(), QUEUE_MAX_ROWS);
        assert!(!labels(&q).iter().any(|l| l.starts_with('▲')));

        // Raise the same proposal's priority (1×8 = 8 ÷0.8 = 10) and its
        // digest earns a row, displacing one of the ten.
        let loud_idea = add_proposed(&mut s, light, "loud idea", Priority::P0);
        let q = default_queue(&s);
        assert!(labels(&q).contains(&"▲2 light".to_string()));
        let _ = loud_idea;
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

        let ids = task_ids(&default_queue(&s));
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

        let ids = task_ids(&default_queue(&s));
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

        let ids = task_ids(&default_queue(&s));
        assert_eq!(ids, vec![question, diff]);
    }

    #[test]
    fn a_stalled_task_scores_the_review_bonus_and_competes_in_the_queue() {
        // stalled earns +2, the same as review (§7).
        assert_eq!(state_bonus(TaskState::Stalled), 2.0);
        assert_eq!(
            score(3, Priority::P2, TaskState::Stalled, 0.0, 0).base,
            12.0
        );

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

        let ids = task_ids(&default_queue(&s));
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

        assert_eq!(focus(&s.candidates().unwrap()).unwrap().task.id, ready);
        assert_eq!(task_ids(&default_queue(&s)), vec![stalled, ready]);
    }

    #[test]
    fn waiting_is_excluded_from_the_queue_and_earns_no_bonus() {
        // `waiting` is out of the running like `parked` (DESIGN.md §6/§7): it
        // earns no state bonus and never surfaces in the queue or focus, only
        // in the state counts.
        assert_eq!(state_bonus(TaskState::Waiting), 0.0);

        let mut s = setup();
        let p = add_project(&mut s, "p", 5);
        let waiting = add_task(&mut s, p, "handed off", Priority::P0);
        to_waiting(&mut s, waiting);
        let ready = add_task(&mut s, p, "startable", Priority::P3);

        let candidates = s.candidates().unwrap();
        // even a P0 waiting task in a heavy project stays out of both views
        assert!(candidates.iter().all(|c| c.task.id != waiting));
        assert_eq!(task_ids(&default_queue(&s)), vec![ready]);
        assert_eq!(focus(&candidates).unwrap().task.id, ready);

        // but it is felt in the state counts
        assert_eq!(s.state_counts().unwrap().waiting, 1);
    }

    /// A proposal being refined is out of the triage queue for the duration
    /// (DESIGN.md §6): the operator cannot triage a body an agent is mid-way
    /// through rewriting, and the state — not a guard in the TUI — is what takes
    /// it out, so it is gone in every window at once.
    #[test]
    fn a_refining_proposal_leaves_the_queue_and_is_counted_apart() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 5);
        let refining = add_proposed(&mut s, p, "being rewritten", Priority::P0);
        s.record_refine_launch(refining, "thin body", "claude", Some(1), None)
            .unwrap();
        let ready = add_task(&mut s, p, "startable", Priority::P3);

        let candidates = s.candidates().unwrap();
        assert!(candidates.iter().all(|c| c.task.id != refining));
        assert_eq!(task_ids(&default_queue(&s)), vec![ready]);

        let counts = s.state_counts().unwrap();
        assert_eq!(counts.refining, 1);
        assert_eq!(counts.proposed, 0);

        // and it is back the moment the round concludes
        s.conclude_refine(refining, crate::RefineOutcome::Applied)
            .unwrap();
        assert_eq!(s.state_counts().unwrap().refining, 0);
        assert!(
            s.candidates()
                .unwrap()
                .iter()
                .any(|c| c.task.id == refining)
        );
    }

    #[test]
    fn equal_raw_totals_are_split_by_what_the_row_costs() {
        // Contrived so the folded scores collide: needs-input 3×(1+4) = 15,
        // review 5×(1+2) = 15. Before pricing this was a genuine tie broken by
        // the state precedence (§6); now the divisors decide it outright —
        // 18.75 for the question against 10.71 for the diff — and the
        // precedence is left to rows that tie on the effective score too.
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
        assert_eq!(task_ids(&default_queue(&s)), vec![question, diff]);
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
            repo_id: None,
            title: "hidden proposed".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
            deep: false,
        })
        .unwrap();
        let visible = add_task(&mut s, active, "visible", Priority::P3);

        let ids = task_ids(&default_queue(&s));
        assert_eq!(ids, vec![visible]);
        assert_eq!(focus(&s.candidates().unwrap()).unwrap().task.id, visible);
        assert_eq!(s.proposed_count().unwrap(), 0);
    }

    #[test]
    fn archived_projects_are_hidden_from_queue_focus_and_counts() {
        // An archived project leaves the cockpit with all its tasks, whatever
        // their state (DESIGN.md §5); unarchiving restores the exact
        // pre-archive view, since nothing about the tasks was touched.
        let mut s = setup();
        let retiring = add_project(&mut s, "retiring", 5);
        let active = add_project(&mut s, "active", 1);

        let question = add_task(&mut s, retiring, "question", Priority::P0);
        to_needs_input(&mut s, question);
        let ready = add_task(&mut s, retiring, "ready", Priority::P0);
        let idea = add_proposed(&mut s, retiring, "idea", Priority::P2);
        let done = add_task(&mut s, retiring, "done", Priority::P2);
        s.apply(done, crate::Action::Start).unwrap();
        s.apply(done, crate::Action::Complete(None)).unwrap();
        s.apply(done, crate::Action::Accept).unwrap();
        let visible = add_task(&mut s, active, "visible", Priority::P3);

        let before_labels = labels(&default_queue(&s));
        assert_eq!(
            before_labels,
            vec![
                format!("#{question}"),
                format!("#{ready}"),
                "▲1 retiring".to_string(),
                format!("#{visible}"),
            ]
        );
        let _ = idea;

        s.set_archived(retiring, true).unwrap();
        let ids = task_ids(&default_queue(&s));
        assert_eq!(ids, vec![visible]);
        assert_eq!(focus(&s.candidates().unwrap()).unwrap().task.id, visible);
        let counts = s.state_counts().unwrap();
        assert_eq!(counts.needs_input, 0);
        assert_eq!(counts.ready, 1);
        assert_eq!(counts.done, 0);
        assert_eq!(s.proposed_count().unwrap(), 0);

        // Unarchive: everything is back exactly as before.
        s.set_archived(retiring, false).unwrap();
        let restored_labels = labels(&default_queue(&s));
        assert_eq!(restored_labels, before_labels);
        assert_eq!(s.state_counts().unwrap().done, 1);
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
            repo_id: None,
            title: "idea".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
            deep: false,
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
            repo_id: None,
            title: "hidden idea".into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Proposed,
            agent: None,
            human: false,
            deep: false,
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
        let ids = task_ids(&default_queue(&s));
        assert_eq!(ids, vec![blocker]);
        assert_eq!(focus(&s.candidates().unwrap()).unwrap().task.id, blocker);

        // once the blocker closes it surfaces, and now outranks it
        s.apply(blocker, crate::Action::Start).unwrap();
        s.apply(blocker, crate::Action::Complete(None)).unwrap();
        s.apply(blocker, crate::Action::Accept).unwrap();
        let candidates = s.candidates().unwrap();
        assert_eq!(focus(&candidates).unwrap().task.id, blocked);
    }

    #[test]
    fn only_open_blocks_dependents_count_toward_the_unblock_bonus() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 2);
        let blocker = add_task(&mut s, p, "blocker", Priority::P2);

        // nothing waits on it yet
        let alone = s.explain(blocker).unwrap();
        assert_eq!(alone.open_dependents, 0);
        assert_eq!(alone.unblock_bonus, 0.0);

        // an open task parked behind it counts
        let blocked = add_task(&mut s, p, "blocked", Priority::P2);
        s.add_dep(blocked, blocker, crate::DepKind::Blocks).unwrap();
        assert_eq!(s.explain(blocker).unwrap().open_dependents, 1);

        // a closed dependent does not, whether accepted or rejected
        let finished = add_task(&mut s, p, "finished", Priority::P2);
        s.apply(finished, crate::Action::Start).unwrap();
        s.apply(finished, crate::Action::Complete(None)).unwrap();
        s.apply(finished, crate::Action::Accept).unwrap();
        s.add_dep(finished, blocker, crate::DepKind::Blocks)
            .unwrap();
        let idea = add_proposed(&mut s, p, "bad idea", Priority::P2);
        s.apply(idea, crate::Action::Triage(crate::Triage::Reject))
            .unwrap();
        s.add_dep(idea, blocker, crate::DepKind::Blocks).unwrap();
        assert_eq!(s.explain(blocker).unwrap().open_dependents, 1);

        // nor does an edge of any other kind
        for kind in [
            crate::DepKind::DiscoveredFrom,
            crate::DepKind::Parent,
            crate::DepKind::Related,
        ] {
            let other = add_task(&mut s, p, "adjacent work", Priority::P2);
            s.add_dep(other, blocker, kind).unwrap();
        }
        assert_eq!(s.explain(blocker).unwrap().open_dependents, 1);

        // the queue's own query agrees with the single-task decomposition
        let candidates = s.candidates().unwrap();
        let scored = candidates.iter().find(|c| c.task.id == blocker).unwrap();
        assert_eq!(scored.score.open_dependents, 1);
        assert_eq!(scored.score.base, 6.0); // 2×(2+1)
    }

    #[test]
    fn blocking_open_work_lifts_a_task_over_an_identical_one() {
        let mut s = setup();
        let p = add_project(&mut s, "p", 3);
        let plain = add_task(&mut s, p, "plain", Priority::P2); // 3×2 = 6
        let blocking = add_task(&mut s, p, "blocking", Priority::P2); // 3×(2+1) = 9
        let blocked = add_task(&mut s, p, "blocked", Priority::P2);
        s.add_dep(blocked, blocking, crate::DepKind::Blocks)
            .unwrap();
        // pin the ages equal so only the unblock bonus separates them
        s.conn
            .execute(
                "UPDATE tasks SET state_since = '2020-01-01 00:00:00' WHERE id IN (?1, ?2)",
                (plain, blocking),
            )
            .unwrap();

        let candidates = s.candidates().unwrap();
        // the blocked task is parked behind its blocker, so only the two
        // otherwise-identical tasks compete — and the one holding work up wins
        assert_eq!(task_ids(&default_queue(&s)), vec![blocking, plain]);
        assert_eq!(focus(&candidates).unwrap().task.id, blocking);

        // a second and third dependent grow the bonus to the cap, no further
        for i in 0..2 {
            let more = add_task(&mut s, p, &format!("also blocked {i}"), Priority::P2);
            s.add_dep(more, blocking, crate::DepKind::Blocks).unwrap();
        }
        let candidates = s.candidates().unwrap();
        let scored = candidates.iter().find(|c| c.task.id == blocking).unwrap();
        assert_eq!(scored.score.open_dependents, 3);
        assert_eq!(scored.score.unblock_bonus, 2.0);
        assert_eq!(scored.score.base, 12.0); // 3×(2+2)

        // and even at the cap it never fakes a priority level: a fresh P0
        // blocking nothing (3×8 = 24) still walks away with it
        let urgent = add_task(&mut s, p, "urgent", Priority::P0);
        let candidates = s.candidates().unwrap();
        assert_eq!(focus(&candidates).unwrap().task.id, urgent);
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
