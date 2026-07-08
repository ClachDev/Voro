//! Core logic for Focus: the SQLite store, the task state machine, and the
//! scheduler/scoring. Pure of terminal I/O; every interface (TUI, CLI verbs)
//! is a thin consumer of this crate. Concepts and invariants are specified in
//! `docs/DESIGN.md`.

mod error;
mod model;
mod store;
mod transition;

pub use error::{Error, Result};
pub use model::{Dep, DepKind, Event, Priority, Project, Task, TaskState};
pub use store::{NewTask, Store, TaskEdit};
pub use transition::{Action, Triage};

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn new_task(project_id: i64, title: &str, state: TaskState) -> NewTask {
        NewTask {
            project_id,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
        }
    }

    #[test]
    fn project_crud_and_weight_bounds() {
        let mut s = store();
        let p = s.create_project("focus", "/tmp/focus").unwrap();
        assert_eq!(p.weight, 3);
        s.set_weight(p.id, 5).unwrap();
        assert_eq!(s.project(p.id).unwrap().weight, 5);
        assert!(s.set_weight(p.id, 6).is_err());
        assert!(s.set_weight(999, 2).is_err());
    }

    #[test]
    fn task_create_defaults_and_event() {
        let mut s = store();
        let p = s.create_project("focus", "/tmp/focus").unwrap();
        let t = s
            .create_task(new_task(p.id, "First", TaskState::Ready))
            .unwrap();
        assert_eq!(t.state, TaskState::Ready);
        assert!(t.question.is_none());
        assert!(t.closed_at.is_none());
        assert!(!t.state_since.is_empty());
        let events = s.events_for(t.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "created");
        assert_eq!(events[0].detail.as_deref(), Some("ready"));
    }

    #[test]
    fn task_cannot_be_created_in_active_or_closed_states() {
        let mut s = store();
        let p = s.create_project("focus", "/tmp/focus").unwrap();
        for state in [
            TaskState::Running,
            TaskState::NeedsInput,
            TaskState::Review,
            TaskState::Done,
            TaskState::Rejected,
        ] {
            assert!(s.create_task(new_task(p.id, "bad", state)).is_err());
        }
    }

    #[test]
    fn dep_rejects_self_reference() {
        let mut s = store();
        let p = s.create_project("focus", "/tmp/focus").unwrap();
        let t = s
            .create_task(new_task(p.id, "t", TaskState::Backlog))
            .unwrap();
        assert!(s.add_dep(t.id, t.id, DepKind::Blocks).is_err());
    }

    #[test]
    fn dep_add_and_remove() {
        let mut s = store();
        let p = s.create_project("focus", "/tmp/focus").unwrap();
        let a = s
            .create_task(new_task(p.id, "a", TaskState::Ready))
            .unwrap();
        let b = s
            .create_task(new_task(p.id, "b", TaskState::Backlog))
            .unwrap();
        s.add_dep(b.id, a.id, DepKind::Blocks).unwrap();
        let deps = s.deps_of(b.id).unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on, a.id);
        assert_eq!(deps[0].kind, DepKind::Blocks);
        s.remove_dep(b.id, a.id).unwrap();
        assert!(s.deps_of(b.id).unwrap().is_empty());
    }
}
