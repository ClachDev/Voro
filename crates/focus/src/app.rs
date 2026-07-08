use focus_core::{Candidate, Project, Store, Task, TaskState, scheduler};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Cockpit,
    Tasks,
}

/// One selectable row on the cockpit; indices point into the App caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitRow {
    Inbox(usize),
    Triage,
    Focus,
    Running(usize),
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: Task,
    pub project: String,
    pub weight: i64,
}

pub struct App {
    pub store: Store,
    pub screen: Screen,
    pub should_quit: bool,
    pub status: Option<String>,

    pub projects: Vec<Project>,
    pub inbox: Vec<Candidate>,
    pub focus: Option<Candidate>,
    pub running: Vec<TaskRow>,
    pub all: Vec<TaskRow>,
    pub proposed: i64,

    pub cockpit_rows: Vec<CockpitRow>,
    pub cockpit_sel: usize,
    pub tasks_sel: usize,
}

/// Browser grouping: attention states first, closed last.
fn browse_order(state: TaskState) -> u8 {
    match state {
        TaskState::Proposed => 0,
        TaskState::NeedsInput => 1,
        TaskState::Review => 2,
        TaskState::Ready => 3,
        TaskState::Running => 4,
        TaskState::Backlog => 5,
        TaskState::Done => 6,
        TaskState::Rejected => 7,
    }
}

impl App {
    pub fn new(store: Store) -> focus_core::Result<App> {
        let mut app = App {
            store,
            screen: Screen::Cockpit,
            should_quit: false,
            status: None,
            projects: Vec::new(),
            inbox: Vec::new(),
            focus: None,
            running: Vec::new(),
            all: Vec::new(),
            proposed: 0,
            cockpit_rows: Vec::new(),
            cockpit_sel: 0,
            tasks_sel: 0,
        };
        app.refresh()?;
        Ok(app)
    }

    /// Reload every view from the store. Called after any mutation; the data
    /// volumes are trivial, so correctness beats cleverness.
    pub fn refresh(&mut self) -> focus_core::Result<()> {
        self.projects = self.store.projects()?;
        let candidates = self.store.candidates()?;
        self.inbox = scheduler::inbox(&candidates).into_iter().cloned().collect();
        self.focus = scheduler::focus(&candidates).cloned();
        self.proposed = self.store.proposed_count()?;

        let mut all: Vec<TaskRow> = self
            .store
            .tasks()?
            .into_iter()
            .map(|task| {
                let (project, weight) = self
                    .projects
                    .iter()
                    .find(|p| p.id == task.project_id)
                    .map(|p| (p.name.clone(), p.weight))
                    .unwrap_or_default();
                TaskRow {
                    task,
                    project,
                    weight,
                }
            })
            .collect();
        all.sort_by_key(|r| (browse_order(r.task.state), r.task.id));
        self.running = all
            .iter()
            .filter(|r| r.task.state == TaskState::Running)
            .cloned()
            .collect();
        self.all = all;

        self.cockpit_rows = (0..self.inbox.len()).map(CockpitRow::Inbox).collect();
        if self.proposed > 0 {
            self.cockpit_rows.push(CockpitRow::Triage);
        }
        if self.focus.is_some() {
            self.cockpit_rows.push(CockpitRow::Focus);
        }
        self.cockpit_rows
            .extend((0..self.running.len()).map(CockpitRow::Running));

        self.cockpit_sel = self
            .cockpit_sel
            .min(self.cockpit_rows.len().saturating_sub(1));
        self.tasks_sel = self.tasks_sel.min(self.all.len().saturating_sub(1));
        Ok(())
    }

    pub fn selected_task_id(&self) -> Option<i64> {
        match self.screen {
            Screen::Cockpit => match self.cockpit_rows.get(self.cockpit_sel)? {
                CockpitRow::Inbox(i) => Some(self.inbox.get(*i)?.task.id),
                CockpitRow::Focus => Some(self.focus.as_ref()?.task.id),
                CockpitRow::Running(i) => Some(self.running.get(*i)?.task.id),
                CockpitRow::Triage => None,
            },
            Screen::Tasks => Some(self.all.get(self.tasks_sel)?.task.id),
        }
    }

    pub fn move_selection(&mut self, delta: i64) {
        let (sel, len) = match self.screen {
            Screen::Cockpit => (&mut self.cockpit_sel, self.cockpit_rows.len()),
            Screen::Tasks => (&mut self.tasks_sel, self.all.len()),
        };
        if len == 0 {
            return;
        }
        *sel = (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
    }

    pub fn toggle_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Cockpit => Screen::Tasks,
            Screen::Tasks => Screen::Cockpit,
        };
    }

    /// Jump to the first proposed task in the browser (the triage row's
    /// Enter action).
    pub fn jump_to_proposed(&mut self) {
        self.screen = Screen::Tasks;
        if let Some(i) = self
            .all
            .iter()
            .position(|r| r.task.state == TaskState::Proposed)
        {
            self.tasks_sel = i;
        }
    }

    pub fn report<T>(&mut self, result: focus_core::Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                self.status = Some(e.to_string());
                None
            }
        }
    }
}
