use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::{
    Action, Candidate, Project, ScoreBreakdown, Store, Task, TaskState, Triage, scheduler,
};

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

/// What a text prompt is collecting, and the transition it feeds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Ask,
    Answer,
    RejectWork,
}

impl PromptKind {
    pub fn title(self) -> &'static str {
        match self {
            PromptKind::Ask => "Question",
            PromptKind::Answer => "Answer",
            PromptKind::RejectWork => "Rejection feedback",
        }
    }

    fn action(self, text: String) -> Action {
        match self {
            PromptKind::Ask => Action::Ask(text),
            PromptKind::Answer => Action::Answer(text),
            PromptKind::RejectWork => Action::RejectWork(text),
        }
    }
}

pub enum Mode {
    Normal,
    Weights {
        sel: usize,
    },
    AddProject {
        name: String,
        path: String,
        on_path: bool,
    },
    PickProject {
        sel: usize,
    },
    Transition {
        task_id: i64,
        actions: Vec<Action>,
        sel: usize,
    },
    Prompt {
        task_id: i64,
        kind: PromptKind,
        buffer: String,
    },
    Score {
        task_id: i64,
        state: TaskState,
        breakdown: ScoreBreakdown,
    },
}

/// A request for main() to suspend the terminal and run $EDITOR.
#[derive(Debug, Clone, Copy)]
pub enum EditorRequest {
    Create { project_id: i64 },
    Edit { task_id: i64 },
}

pub fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Triage(Triage::Backlog) => "triage → backlog",
        Action::Triage(Triage::Ready) => "triage → ready",
        Action::Triage(Triage::Reject) => "triage → rejected",
        Action::Start => "start → running",
        Action::Ask(_) => "ask a question → needs-input",
        Action::Answer(_) => "answer the question → running",
        Action::Complete => "complete → review",
        Action::Accept => "accept → done",
        Action::RejectWork(_) => "reject with feedback → running",
        Action::Abort => "abort → ready",
        Action::Park => "park → backlog",
        Action::Unpark => "unpark → ready",
        Action::Abandon => "abandon → rejected",
    }
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

    pub mode: Mode,
    pub pending_editor: Option<EditorRequest>,
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
    pub fn new(store: Store) -> voro_core::Result<App> {
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
            mode: Mode::Normal,
            pending_editor: None,
        };
        app.refresh()?;
        Ok(app)
    }

    /// Reload every view from the store. Called after any mutation; the data
    /// volumes are trivial, so correctness beats cleverness.
    pub fn refresh(&mut self) -> voro_core::Result<()> {
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

    pub fn report<T>(&mut self, result: voro_core::Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                self.status = Some(e.to_string());
                None
            }
        }
    }

    fn selected_task(&self) -> Option<&Task> {
        let id = self.selected_task_id()?;
        self.all.iter().map(|r| &r.task).find(|t| t.id == id)
    }

    fn apply_and_refresh(&mut self, task_id: i64, action: Action) {
        let result = self.store.apply(task_id, action);
        if self.report(result).is_some() {
            let result = self.refresh();
            self.report(result);
        }
    }

    // --- key handling ---

    pub fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
        match mode {
            Mode::Normal => self.key_normal(key),
            Mode::Weights { sel } => self.key_weights(key, sel),
            Mode::AddProject {
                name,
                path,
                on_path,
            } => self.key_add_project(key, name, path, on_path),
            Mode::PickProject { sel } => self.key_pick_project(key, sel),
            Mode::Transition {
                task_id,
                actions,
                sel,
            } => self.key_transition(key, task_id, actions, sel),
            Mode::Prompt {
                task_id,
                kind,
                buffer,
            } => self.key_prompt(key, task_id, kind, buffer),
            Mode::Score { .. } => {} // any key closes
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab => self.toggle_screen(),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('r') => {
                let result = self.refresh();
                self.report(result);
            }
            KeyCode::Enter => {
                if self.screen == Screen::Cockpit
                    && matches!(
                        self.cockpit_rows.get(self.cockpit_sel),
                        Some(CockpitRow::Triage)
                    )
                {
                    self.jump_to_proposed();
                }
            }
            KeyCode::Char('w') => {
                if self.projects.is_empty() {
                    self.status = Some("no projects yet — press P to add one".into());
                } else {
                    self.mode = Mode::Weights { sel: 0 };
                }
            }
            KeyCode::Char('P') => {
                self.mode = Mode::AddProject {
                    name: String::new(),
                    path: String::new(),
                    on_path: false,
                };
            }
            KeyCode::Char('n') => match self.projects.len() {
                0 => self.status = Some("no projects yet — press P to add one".into()),
                1 => {
                    self.pending_editor = Some(EditorRequest::Create {
                        project_id: self.projects[0].id,
                    })
                }
                _ => self.mode = Mode::PickProject { sel: 0 },
            },
            KeyCode::Char('e') => {
                if let Some(id) = self.selected_task_id() {
                    self.pending_editor = Some(EditorRequest::Edit { task_id: id });
                }
            }
            KeyCode::Char('s') => {
                if let Some(task) = self.selected_task() {
                    let actions = Store::legal_actions(task.state);
                    if actions.is_empty() {
                        self.status = Some(format!("task is {} — nowhere to go", task.state));
                    } else {
                        self.mode = Mode::Transition {
                            task_id: task.id,
                            actions,
                            sel: 0,
                        };
                    }
                }
            }
            KeyCode::Char('x') => {
                if let Some(task) = self.selected_task() {
                    let (id, state) = (task.id, task.state);
                    let result = self.store.explain(id);
                    if let Some(breakdown) = self.report(result) {
                        self.mode = Mode::Score {
                            task_id: id,
                            state,
                            breakdown,
                        };
                    }
                }
            }
            _ => {}
        }
    }

    fn key_weights(&mut self, key: KeyEvent, mut sel: usize) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('w') => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(self.projects.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Char(c @ '0'..='5') => {
                if let Some(project) = self.projects.get(sel) {
                    let id = project.id;
                    let result = self
                        .store
                        .set_weight(id, c.to_digit(10).unwrap() as i64)
                        .and_then(|_| self.refresh());
                    self.report(result);
                }
            }
            _ => {}
        }
        self.mode = Mode::Weights { sel };
    }

    fn key_add_project(
        &mut self,
        key: KeyEvent,
        mut name: String,
        mut path: String,
        on_path: bool,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab => {
                self.mode = Mode::AddProject {
                    name,
                    path,
                    on_path: !on_path,
                };
                return;
            }
            KeyCode::Enter => {
                if !on_path {
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path: true,
                    };
                    return;
                }
                if name.trim().is_empty() {
                    self.status = Some("project name is required".into());
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path,
                    };
                    return;
                }
                let result = self
                    .store
                    .create_project(name.trim(), path.trim())
                    .and_then(|_| self.refresh());
                if self.report(result).is_none() {
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path,
                    };
                }
                return;
            }
            KeyCode::Backspace => {
                if on_path {
                    path.pop();
                } else {
                    name.pop();
                }
            }
            KeyCode::Char(c) => {
                if on_path {
                    path.push(c);
                } else {
                    name.push(c);
                }
            }
            _ => {}
        }
        self.mode = Mode::AddProject {
            name,
            path,
            on_path,
        };
    }

    fn key_pick_project(&mut self, key: KeyEvent, mut sel: usize) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(self.projects.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(project) = self.projects.get(sel) {
                    self.pending_editor = Some(EditorRequest::Create {
                        project_id: project.id,
                    });
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::PickProject { sel };
    }

    fn key_transition(
        &mut self,
        key: KeyEvent,
        task_id: i64,
        actions: Vec<Action>,
        mut sel: usize,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(actions.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                let action = actions[sel].clone();
                let kind = match action {
                    Action::Ask(_) => Some(PromptKind::Ask),
                    Action::Answer(_) => Some(PromptKind::Answer),
                    Action::RejectWork(_) => Some(PromptKind::RejectWork),
                    _ => None,
                };
                match kind {
                    Some(kind) => {
                        self.mode = Mode::Prompt {
                            task_id,
                            kind,
                            buffer: String::new(),
                        };
                    }
                    None => self.apply_and_refresh(task_id, action),
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::Transition {
            task_id,
            actions,
            sel,
        };
    }

    fn key_prompt(&mut self, key: KeyEvent, task_id: i64, kind: PromptKind, mut buffer: String) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.apply_and_refresh(task_id, kind.action(buffer));
                return;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        self.mode = Mode::Prompt {
            task_id,
            kind,
            buffer,
        };
    }

    // --- editor application (called by main after the $EDITOR round-trip) ---

    pub fn create_from_form(
        &mut self,
        project_id: i64,
        form: crate::editor::TaskForm,
    ) -> voro_core::Result<()> {
        for dep in &form.blocks {
            self.store.task(*dep)?;
        }
        let task = self.store.create_task(voro_core::NewTask {
            project_id,
            title: form.title,
            body: form.body,
            priority: form.priority,
            state: form.state.unwrap_or(TaskState::Proposed),
            agent: form.agent,
        })?;
        if !form.blocks.is_empty() {
            self.store.set_blocks_deps(task.id, &form.blocks)?;
        }
        self.refresh()
    }

    pub fn update_from_form(
        &mut self,
        task_id: i64,
        form: crate::editor::TaskForm,
    ) -> voro_core::Result<()> {
        self.store.update_task(
            task_id,
            voro_core::TaskEdit {
                title: form.title,
                body: form.body,
                priority: form.priority,
                agent: form.agent,
            },
        )?;
        self.store.set_blocks_deps(task_id, &form.blocks)?;
        self.refresh()
    }
}
