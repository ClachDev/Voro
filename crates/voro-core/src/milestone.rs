//! Milestones (DESIGN.md §3): a flagged human task gated on everything that
//! blocks it. Membership and a task's own milestone are both walks over
//! `blocks` edges, computed here from one read of the graph and stored
//! nowhere.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use rusqlite::{Connection, params};

use crate::error::{Error, Result};
use crate::model::{Priority, Task, TaskState};
use crate::store::{Store, TASK_COLUMNS, get_task, log_event, task_from_row};
use crate::transition::Action;

/// A milestone with the tasks that count toward it.
#[derive(Debug, Clone)]
pub struct MilestoneMembers {
    pub milestone: Task,
    /// Every member in id order: the tasks that block the milestone at any
    /// distance, with a nearer milestone upstream counted once and its own
    /// members left to it.
    pub members: Vec<Task>,
}

impl MilestoneMembers {
    /// Members not yet closed.
    pub fn open(&self) -> usize {
        self.members
            .iter()
            .filter(|t| !matches!(t.state, TaskState::Done | TaskState::Rejected))
            .count()
    }

    pub fn done(&self) -> usize {
        self.members
            .iter()
            .filter(|t| t.state == TaskState::Done)
            .count()
    }
}

/// The `blocks` edges in both directions, plus which tasks are milestones.
struct Graph {
    /// task → the tasks blocking it
    blockers: HashMap<i64, Vec<i64>>,
    /// task → the tasks it blocks
    dependents: HashMap<i64, Vec<i64>>,
    milestones: HashSet<i64>,
}

impl Graph {
    fn load(conn: &Connection) -> Result<Graph> {
        let mut blockers: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut dependents: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut stmt = conn
            .prepare("SELECT task_id, depends_on FROM deps WHERE kind = 'blocks' ORDER BY 1, 2")?;
        let edges = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
        for edge in edges {
            let (task, blocker) = edge?;
            blockers.entry(task).or_default().push(blocker);
            dependents.entry(blocker).or_default().push(task);
        }
        let mut stmt = conn.prepare("SELECT id FROM tasks WHERE milestone = 1")?;
        let milestones = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Graph {
            blockers,
            dependents,
            milestones,
        })
    }

    /// Upstream of `milestone`, stopping at each milestone met on the way.
    fn members(&self, milestone: i64) -> BTreeSet<i64> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([milestone]);
        while let Some(id) = queue.pop_front() {
            for &blocker in self.blockers.get(&id).into_iter().flatten() {
                if blocker != milestone
                    && seen.insert(blocker)
                    && !self.milestones.contains(&blocker)
                {
                    queue.push_back(blocker);
                }
            }
        }
        seen
    }

    /// The milestones nearest downstream of `task`: every one at the first
    /// distance any is found, in id order.
    fn nearest(&self, task: i64) -> Vec<i64> {
        let mut seen = HashSet::from([task]);
        let mut level = vec![task];
        while !level.is_empty() {
            let mut next = Vec::new();
            for id in level {
                for &dependent in self.dependents.get(&id).into_iter().flatten() {
                    if seen.insert(dependent) {
                        next.push(dependent);
                    }
                }
            }
            let mut found: Vec<i64> = next
                .iter()
                .copied()
                .filter(|id| self.milestones.contains(id))
                .collect();
            if !found.is_empty() {
                found.sort_unstable();
                return found;
            }
            level = next;
        }
        Vec::new()
    }
}

impl Store {
    /// Create a milestone: a human task flagged `milestone`, parked. It stays
    /// parked while nothing blocks it, and the ordinary promotion readies it
    /// when its last blocker closes (DESIGN.md §5).
    pub fn create_milestone(
        &mut self,
        project_id: i64,
        title: &str,
        body: &str,
        priority: Priority,
    ) -> Result<Task> {
        let project = self.project(project_id)?;
        if project.archived {
            return Err(Error::ProjectArchived { name: project.name });
        }
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO tasks (project_id, title, body, priority, state, human, milestone,
                                state_since, created_at)
             VALUES (?1, ?2, ?3, ?4, 'parked', 1, 1, datetime('now'), datetime('now'))",
            params![project_id, title, body, priority],
        )?;
        let id = tx.last_insert_rowid();
        log_event(&tx, id, "created", Some("parked"))?;
        log_event(&tx, id, "milestone", Some("set"))?;
        tx.commit()?;
        self.task(id)
    }

    /// The milestones a task belongs to: the nearest downstream along `blocks`
    /// edges, all of them when several are equally near, in id order.
    pub fn milestones_of(&self, task_id: i64) -> Result<Vec<Task>> {
        get_task(&self.conn, task_id)?.ok_or(Error::TaskNotFound(task_id))?;
        let graph = Graph::load(&self.conn)?;
        graph
            .nearest(task_id)
            .into_iter()
            .map(|id| self.task(id))
            .collect()
    }

    /// [`milestones_of`](Store::milestones_of) for every task that has one,
    /// from a single read of the graph — the listing's per-row lookup.
    pub fn milestone_ids_by_task(&self) -> Result<HashMap<i64, Vec<i64>>> {
        let graph = Graph::load(&self.conn)?;
        let mut stmt = self.conn.prepare("SELECT id FROM tasks")?;
        let ids: Vec<i64> = stmt
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(ids
            .into_iter()
            .map(|id| (id, graph.nearest(id)))
            .filter(|(_, found)| !found.is_empty())
            .collect())
    }

    /// A milestone and its members. Refused on a task that is not one.
    pub fn milestone_members(&self, milestone_id: i64) -> Result<MilestoneMembers> {
        let milestone = self.task(milestone_id)?;
        if !milestone.milestone {
            return Err(Error::Invalid(format!(
                "task {milestone_id} is not a milestone"
            )));
        }
        let graph = Graph::load(&self.conn)?;
        let members = graph
            .members(milestone_id)
            .into_iter()
            .map(|id| self.task(id))
            .collect::<Result<_>>()?;
        Ok(MilestoneMembers { milestone, members })
    }

    /// Every milestone with its members, in id order. `all` includes closed
    /// ones; otherwise only those not yet done or rejected.
    pub fn milestones(&self, all: bool) -> Result<Vec<MilestoneMembers>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLUMNS} FROM tasks WHERE milestone = 1
               AND (?1 OR state NOT IN ('done', 'rejected'))
             ORDER BY id"
        ))?;
        let milestones: Vec<Task> = stmt
            .query_map([all], task_from_row)?
            .collect::<rusqlite::Result<_>>()?;
        let graph = Graph::load(&self.conn)?;
        milestones
            .into_iter()
            .map(|milestone| {
                let members = graph
                    .members(milestone.id)
                    .into_iter()
                    .map(|id| self.task(id))
                    .collect::<Result<_>>()?;
                Ok(MilestoneMembers { milestone, members })
            })
            .collect()
    }
}

impl Store {
    /// Close a ready milestone as done (DESIGN.md §6): the human path's
    /// `start` then `complete`, in one transaction, so a milestone is never
    /// left `running`. Refused on a task that is not a ready milestone.
    pub fn close_milestone(&mut self, milestone_id: i64) -> Result<Task> {
        let milestone = self.task(milestone_id)?;
        if !milestone.milestone {
            return Err(Error::Invalid(format!(
                "task {milestone_id} is not a milestone"
            )));
        }
        if milestone.state != TaskState::Ready {
            return Err(Error::Invalid(format!(
                "milestone {milestone_id} is {}; only a ready one closes as done",
                milestone.state
            )));
        }
        let tx = self.conn.transaction()?;
        crate::transition::apply_action(&tx, milestone_id, Action::Start)?;
        crate::transition::apply_action(&tx, milestone_id, Action::Complete(None))?;
        tx.commit()?;
        self.task(milestone_id)
    }
}

impl Task {
    /// Refuse a CLI transition on a milestone (DESIGN.md §6): only the
    /// operator opens or closes one, and only in the TUI.
    pub fn refuse_cli_transition(&self) -> Result<()> {
        if self.milestone {
            return Err(Error::MilestoneRefused { id: self.id });
        }
        Ok(())
    }

    /// The verdicts the TUI offers on a milestone (DESIGN.md §6): done once it
    /// is ready, through [`Store::close_milestone`], and abandon while it is
    /// open. `Complete` stands for done here, the one verb the menu offers
    /// that the machine reaches in two steps.
    pub fn milestone_actions(&self) -> Vec<Action> {
        match self.state {
            TaskState::Ready => vec![Action::Complete(None), Action::Abandon],
            TaskState::Done | TaskState::Rejected => vec![],
            _ => vec![Action::Abandon],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DepKind;
    use crate::store::NewTask;

    fn store() -> (Store, i64) {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("p", "/tmp").unwrap();
        (s, p.id)
    }

    fn task(s: &mut Store, project: i64, title: &str, state: TaskState) -> Task {
        s.create_task(NewTask {
            project_id: project,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
            deep: false,
        })
        .unwrap()
    }

    fn close(s: &mut Store, id: i64) {
        s.apply(id, Action::Start).unwrap();
        s.apply(id, Action::Complete(None)).unwrap();
        s.apply(id, Action::Accept).unwrap();
    }

    /// Milestone A behind the chain t3 → t2 → t1 → A; milestone B behind A and t4.
    struct Fixture {
        a: i64,
        b: i64,
        chain: [i64; 3],
        t4: i64,
    }

    fn fixture(s: &mut Store, p: i64) -> Fixture {
        let a = s.create_milestone(p, "A", "", Priority::P2).unwrap().id;
        let b = s.create_milestone(p, "B", "", Priority::P2).unwrap().id;
        let t1 = task(s, p, "t1", TaskState::Ready).id;
        let t2 = task(s, p, "t2", TaskState::Ready).id;
        let t3 = task(s, p, "t3", TaskState::Ready).id;
        let t4 = task(s, p, "t4", TaskState::Ready).id;
        s.add_dep(a, t1, DepKind::Blocks).unwrap();
        s.add_dep(t1, t2, DepKind::Blocks).unwrap();
        s.add_dep(t2, t3, DepKind::Blocks).unwrap();
        s.add_dep(b, a, DepKind::Blocks).unwrap();
        s.add_dep(b, t4, DepKind::Blocks).unwrap();
        Fixture {
            a,
            b,
            chain: [t1, t2, t3],
            t4,
        }
    }

    #[test]
    fn a_milestone_is_created_parked_human_and_flagged() {
        let (mut s, p) = store();
        let m = s
            .create_milestone(p, "Carpet crossing", "", Priority::P1)
            .unwrap();
        assert_eq!(m.state, TaskState::Parked);
        assert!(m.human && m.milestone);
    }

    #[test]
    fn membership_stops_at_the_next_milestone_upstream() {
        let (mut s, p) = store();
        let f = fixture(&mut s, p);
        let a = s.milestone_members(f.a).unwrap();
        let ids: Vec<i64> = a.members.iter().map(|t| t.id).collect();
        assert_eq!(ids, f.chain.to_vec());
        let b = s.milestone_members(f.b).unwrap();
        let ids: Vec<i64> = b.members.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![f.a, f.t4]);
    }

    #[test]
    fn a_task_belongs_to_its_nearest_milestone_downstream() {
        let (mut s, p) = store();
        let f = fixture(&mut s, p);
        for id in f.chain {
            let found: Vec<i64> = s.milestones_of(id).unwrap().iter().map(|t| t.id).collect();
            assert_eq!(found, vec![f.a], "task {id}");
        }
        let found: Vec<i64> = s
            .milestones_of(f.t4)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(found, vec![f.b]);
        let found: Vec<i64> = s.milestones_of(f.a).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(found, vec![f.b]);
        assert!(s.milestones_of(f.b).unwrap().is_empty());
        let map = s.milestone_ids_by_task().unwrap();
        assert_eq!(map.get(&f.chain[2]), Some(&vec![f.a]));
        assert!(!map.contains_key(&f.b));
    }

    #[test]
    fn equally_near_milestones_are_all_reported_in_id_order() {
        let (mut s, p) = store();
        let m2 = s
            .create_milestone(p, "second", "", Priority::P2)
            .unwrap()
            .id;
        let m1 = s.create_milestone(p, "first", "", Priority::P2).unwrap().id;
        let t = task(&mut s, p, "shared", TaskState::Ready).id;
        s.block_tasks(t, &[m1, m2]).unwrap();
        let found: Vec<i64> = s.milestones_of(t).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(found, vec![m2.min(m1), m2.max(m1)]);
    }

    #[test]
    fn closing_the_last_blocker_readies_the_milestone() {
        let (mut s, p) = store();
        let f = fixture(&mut s, p);
        for id in [f.chain[2], f.chain[1]] {
            close(&mut s, id);
        }
        assert_eq!(s.task(f.a).unwrap().state, TaskState::Parked);
        close(&mut s, f.chain[0]);
        assert_eq!(s.task(f.a).unwrap().state, TaskState::Ready);
        let a = s.milestone_members(f.a).unwrap();
        assert_eq!((a.open(), a.done()), (0, 3));
    }

    #[test]
    fn a_ready_milestone_closes_as_done_in_one_step() {
        let (mut s, p) = store();
        let f = fixture(&mut s, p);
        for id in f.chain.into_iter().rev() {
            close(&mut s, id);
        }
        assert_eq!(
            s.task(f.a).unwrap().milestone_actions(),
            vec![Action::Complete(None), Action::Abandon]
        );
        let a = s.close_milestone(f.a).unwrap();
        assert_eq!(a.state, TaskState::Done);
        assert!(a.milestone_actions().is_empty());
        // Closing A was B's last open blocker but one: t4 still holds it.
        assert_eq!(s.task(f.b).unwrap().state, TaskState::Parked);
    }

    #[test]
    fn only_a_ready_milestone_closes_and_a_parked_one_offers_abandon() {
        let (mut s, p) = store();
        let m = s.create_milestone(p, "m", "", Priority::P2).unwrap();
        assert_eq!(m.milestone_actions(), vec![Action::Abandon]);
        assert!(s.close_milestone(m.id).is_err());
        assert_eq!(s.task(m.id).unwrap().state, TaskState::Parked);
        let plain = task(&mut s, p, "plain", TaskState::Ready);
        assert!(s.close_milestone(plain.id).is_err());
    }

    #[test]
    fn the_open_listing_drops_closed_milestones_and_all_keeps_them() {
        let (mut s, p) = store();
        let m = s.create_milestone(p, "gone", "", Priority::P2).unwrap().id;
        s.apply(m, Action::Abandon).unwrap();
        assert!(s.milestones(false).unwrap().is_empty());
        assert_eq!(s.milestones(true).unwrap().len(), 1);
    }

    #[test]
    fn a_milestone_refuses_cli_transitions_and_cannot_lose_its_human_flag() {
        let (mut s, p) = store();
        let m = s.create_milestone(p, "m", "", Priority::P2).unwrap();
        assert!(matches!(
            m.refuse_cli_transition(),
            Err(Error::MilestoneRefused { .. })
        ));
        let plain = task(&mut s, p, "plain", TaskState::Ready);
        assert!(plain.refuse_cli_transition().is_ok());
        let err = s
            .update_task(
                m.id,
                crate::store::TaskEdit {
                    title: m.title.clone(),
                    body: m.body.clone(),
                    priority: m.priority,
                    agent: None,
                    human: false,
                    deep: false,
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("milestone"), "{err}");
    }
}
