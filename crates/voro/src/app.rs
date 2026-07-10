use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::{
    Action, AgentsConfig, Blocker, Candidate, Event, Project, RunningRow, ScoreBreakdown, Store,
    Task, TaskState, Triage, scheduler,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Cockpit,
    Tasks,
}

/// One selectable row on the cockpit; indices point into the App caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitRow {
    Queue(usize),
    Running(usize),
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: Task,
    pub project: String,
    pub weight: i64,
    /// The task's `blocks` dependencies with each blocker's state, so the
    /// browser can show a parked row what it is waiting on.
    pub blockers: Vec<Blocker>,
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
        /// `Some(id)` when this popup is editing an existing project (rename +
        /// path-edit) rather than creating a new one.
        editing: Option<i64>,
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
    Detail {
        task_id: i64,
        scroll: u16,
    },
    History {
        task_id: i64,
        events: Vec<Event>,
        scroll: u16,
    },
    /// Dispatch-via-picker (DESIGN.md §8): agents loaded fresh from
    /// `agents.toml` when the picker opens, never cached, since the whole
    /// point is to catch a config that's changed since the last dispatch.
    AgentPicker {
        task_id: i64,
        agents: Vec<String>,
        /// The agent that plain dispatch (the resolved-agent key) would use —
        /// the task's own override, else the config default — highlighted in
        /// the list independently of cursor position.
        resolved: Option<String>,
        sel: usize,
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
        Action::Triage(Triage::Parked) => "triage → parked",
        Action::Triage(Triage::Ready) => "triage → ready",
        Action::Triage(Triage::Reject) => "triage → rejected",
        Action::Start => "start → running",
        Action::Ask(_) => "ask a question → needs-input",
        Action::Answer(_) => "answer the question → running",
        Action::Complete => "complete → review",
        Action::Accept => "accept → done",
        Action::RejectWork(_) => "reject with feedback → running",
        Action::Abort => "abort → ready",
        Action::Park => "park → parked",
        Action::Unpark => "unpark → ready",
        Action::Abandon => "abandon → rejected",
    }
}

pub struct App {
    pub store: Store,
    /// Where a continuation dispatch (DESIGN.md §6, "fed to the session")
    /// finds its inputs and puts its artefacts — the same context the CLI's
    /// `dispatch`/`continue`/`answer` verbs use, so the TUI's answer action
    /// behaves identically.
    dispatch_ctx: crate::dispatch::DispatchCtx,
    pub screen: Screen,
    pub should_quit: bool,
    pub status: Option<String>,

    pub projects: Vec<Project>,
    pub queue: Vec<Candidate>,
    /// The cockpit's running strip (DESIGN.md §9): live agent sessions plus
    /// every `running` task with no live session, so a task started by hand or
    /// one whose session died before reconcile demoted it is still visible and
    /// flagged for attention rather than hidden.
    pub running: Vec<RunningRow>,
    pub all: Vec<TaskRow>,
    /// Ready tasks whose most recent session ended `failed` or `capped`
    /// (DESIGN.md §8) — read fresh from session history on every refresh,
    /// never stored on the task itself.
    pub redispatch: std::collections::HashSet<i64>,

    pub cockpit_rows: Vec<CockpitRow>,
    pub cockpit_sel: usize,
    pub tasks_sel: usize,

    pub mode: Mode,
    pub pending_editor: Option<EditorRequest>,

    /// Last `PRAGMA data_version` seen, used to detect commits from other
    /// processes and refresh without reacting to our own mutations.
    last_data_version: i64,
}

/// Browser grouping: attention states first, closed last.
fn browse_order(state: TaskState) -> u8 {
    match state {
        TaskState::Proposed => 0,
        TaskState::NeedsInput => 1,
        TaskState::Review => 2,
        TaskState::Ready => 3,
        TaskState::Running => 4,
        TaskState::Parked => 5,
        TaskState::Done => 6,
        TaskState::Rejected => 7,
    }
}

impl App {
    pub fn new(store: Store, dispatch_ctx: crate::dispatch::DispatchCtx) -> voro_core::Result<App> {
        let mut app = App {
            store,
            dispatch_ctx,
            screen: Screen::Cockpit,
            should_quit: false,
            status: None,
            projects: Vec::new(),
            queue: Vec::new(),
            running: Vec::new(),
            all: Vec::new(),
            redispatch: std::collections::HashSet::new(),
            cockpit_rows: Vec::new(),
            cockpit_sel: 0,
            tasks_sel: 0,
            mode: Mode::Normal,
            pending_editor: None,
            last_data_version: 0,
        };
        app.refresh()?;
        app.last_data_version = app.store.data_version()?;
        Ok(app)
    }

    /// Refresh if another process has committed since the last check. Cheap
    /// enough to call every poll tick; `PRAGMA data_version` ignores our own
    /// writes, so this fires only on genuinely external changes.
    pub fn poll_external(&mut self) -> voro_core::Result<()> {
        let version = self.store.data_version()?;
        if version != self.last_data_version {
            self.last_data_version = version;
            self.refresh()?;
        }
        Ok(())
    }

    /// Reload every view from the store. Called after any mutation; the data
    /// volumes are trivial, so correctness beats cleverness.
    pub fn refresh(&mut self) -> voro_core::Result<()> {
        // Reconcile-on-read (DESIGN.md §8): finalise any session whose
        // process has already exited before anything below reads state that
        // depends on it.
        crate::reconcile::reconcile_live_sessions(&mut self.store)?;

        self.projects = self.store.projects()?;
        let candidates = self.store.candidates()?;
        self.queue = scheduler::queue(&candidates).into_iter().cloned().collect();

        let mut blockers = self.store.blockers_by_task()?;
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
                let blockers = blockers.remove(&task.id).unwrap_or_default();
                TaskRow {
                    task,
                    project,
                    weight,
                    blockers,
                }
            })
            .collect();
        all.sort_by_key(|r| (browse_order(r.task.state), r.task.id));
        self.redispatch = all
            .iter()
            .filter(|r| r.task.state == TaskState::Ready)
            .filter_map(|r| {
                self.store
                    .redispatch_flag(r.task.id)
                    .ok()?
                    .then_some(r.task.id)
            })
            .collect();
        self.all = all;
        self.running = self.store.running_rows()?;

        self.cockpit_rows = (0..self.queue.len()).map(CockpitRow::Queue).collect();
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
                CockpitRow::Queue(i) => Some(self.queue.get(*i)?.task.id),
                CockpitRow::Running(i) => Some(self.running.get(*i)?.task_id),
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

    /// The primary action of the current selection. On the Tasks screen every
    /// row opens its detail view. On the cockpit — where the detail pane
    /// already shows the body — a needs-input task opens the answer prompt
    /// directly and any other task opens its transition menu.
    fn activate_selection(&mut self) {
        if self.screen == Screen::Tasks {
            if let Some(task_id) = self.selected_task_id() {
                self.mode = Mode::Detail { task_id, scroll: 0 };
            }
            return;
        }
        if let Some(task) = self.selected_task() {
            if task.state == TaskState::NeedsInput {
                self.mode = Mode::Prompt {
                    task_id: task.id,
                    kind: PromptKind::Answer,
                    buffer: String::new(),
                };
            } else {
                let actions = Store::legal_actions(task.state);
                if !actions.is_empty() {
                    self.mode = Mode::Transition {
                        task_id: task.id,
                        actions,
                        sel: 0,
                    };
                }
            }
        }
    }

    /// What Enter does for the current selection, phrased for the status
    /// line; None when it does nothing.
    pub fn enter_hint(&self) -> Option<&'static str> {
        match self.screen {
            Screen::Tasks => self.all.get(self.tasks_sel).map(|_| "⏎ view"),
            Screen::Cockpit => match self.cockpit_rows.get(self.cockpit_sel)? {
                CockpitRow::Queue(i) => match self.queue.get(*i)?.task.state {
                    TaskState::NeedsInput => Some("⏎ answer"),
                    TaskState::Proposed => Some("⏎ triage"),
                    TaskState::Review => Some("⏎ review"),
                    _ => Some("⏎ act"),
                },
                CockpitRow::Running(_) => Some("⏎ act"),
            },
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

    /// Apply a transition and refresh. An `Answer` on a task with prior
    /// session history additionally triggers a continuation dispatch
    /// (DESIGN.md §6: "fed to the session" means resuming the work with the
    /// answer in hand, not writing to a live pipe) — the same rule and the
    /// same mechanics `voro answer` uses on the CLI, so the two stay
    /// consistent. A task that was only ever started by hand has no session
    /// history and the transition stands alone.
    fn apply_and_refresh(&mut self, task_id: i64, action: Action) {
        let has_history = matches!(action, Action::Answer(_))
            && self
                .store
                .sessions_for(task_id)
                .is_ok_and(|sessions| !sessions.is_empty());
        let result = self.store.apply(task_id, action);
        if self.report(result).is_some() {
            if has_history {
                match crate::dispatch::continue_dispatch(
                    &mut self.store,
                    &self.dispatch_ctx,
                    task_id,
                    None,
                ) {
                    Ok(summary) => self.status = Some(summary),
                    Err(e) => self.status = Some(format!("answered, but continuation failed: {e}")),
                }
            }
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
                editing,
            } => self.key_add_project(key, name, path, on_path, editing),
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
            Mode::Detail { task_id, scroll } => self.key_detail(key, task_id, scroll),
            Mode::History {
                task_id,
                events,
                scroll,
            } => self.key_history(key, task_id, events, scroll),
            Mode::AgentPicker {
                task_id,
                agents,
                resolved,
                sel,
            } => self.key_agent_picker(key, task_id, agents, resolved, sel),
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
            KeyCode::Enter => self.activate_selection(),
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
                    editing: None,
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
            KeyCode::Char('h') => self.open_history(),
            KeyCode::Char('d') => {
                if let Some((task_id, _)) = self.ready_selected_task() {
                    self.dispatch_task(task_id, None);
                }
            }
            KeyCode::Char('D') => {
                if let Some((task_id, agent)) = self.ready_selected_task() {
                    self.open_agent_picker(task_id, agent);
                }
            }
            KeyCode::Char('o') => self.open_selected_in_viewer(),
            KeyCode::Char('g') => self.open_selected_pr(),
            _ => {}
        }
    }

    /// Open the event-history popup for the current selection, wherever a
    /// task row is selected (cockpit queue/running or the task browser).
    fn open_history(&mut self) {
        if let Some(task_id) = self.selected_task_id() {
            let result = self.store.events_for(task_id);
            if let Some(events) = self.report(result) {
                self.mode = Mode::History {
                    task_id,
                    events,
                    scroll: 0,
                };
            }
        }
    }

    /// The selected task's id and agent override, if there is a selection and
    /// it is `ready` — dispatch's own precondition (DESIGN.md §8). Anything
    /// else sets a status message and returns `None`, the same "no-op with an
    /// explanation" the transition keybindings (`s`) use for a state with
    /// nowhere to go, rather than silently doing nothing.
    fn ready_selected_task(&mut self) -> Option<(i64, Option<String>)> {
        let (id, state, agent) = {
            let task = self.selected_task()?;
            (task.id, task.state, task.agent.clone())
        };
        if state != TaskState::Ready {
            self.status = Some(format!(
                "task is {state} — only ready tasks can be dispatched"
            ));
            return None;
        }
        Some((id, agent))
    }

    /// Dispatch-with-resolved-agent, or the picker's chosen override — both
    /// dispatch actions (DESIGN.md §8/§9) land here. Dispatch errors (dirty
    /// tree, unknown agent, missing config) surface through `self.status`,
    /// the same error style every other action already uses; they never
    /// panic or fail silently.
    fn dispatch_task(&mut self, task_id: i64, agent_override: Option<String>) {
        let result = crate::dispatch::dispatch(
            &mut self.store,
            &self.dispatch_ctx,
            task_id,
            agent_override.as_deref(),
        );
        match result {
            Ok(summary) => self.status = Some(summary),
            Err(e) => self.status = Some(e),
        }
        let refreshed = self.refresh();
        self.report(refreshed);
    }

    /// Open the selected task's checkout in the configured viewer (DESIGN.md
    /// §11a) so its diff can be seen. Only `review`/`running` tasks have a diff
    /// worth opening; anything else, or a missing `[viewer]`, reports through
    /// the status line rather than doing nothing — the same "no-op with an
    /// explanation" style the dispatch keys use.
    fn open_selected_in_viewer(&mut self) {
        let (id, state) = match self.selected_task() {
            Some(task) => (task.id, task.state),
            None => return,
        };
        if !matches!(state, TaskState::Review | TaskState::Running) {
            self.status = Some(format!(
                "task is {state} — only review or running tasks open in a viewer"
            ));
            return;
        }
        match crate::dispatch::open(&mut self.store, &self.dispatch_ctx, id) {
            Ok(summary) => self.status = Some(summary),
            Err(e) => self.status = Some(e),
        }
    }

    /// Jump to the selected task's tracked GitHub PR in a browser (DESIGN.md
    /// §11c). A task with no PR reports through the status line — the same
    /// "no-op with an explanation" style as the dispatch and open keys —
    /// rather than doing nothing. Opening the PR never touches the store, so
    /// no refresh follows.
    fn open_selected_pr(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        match crate::pr::open(&self.store, id) {
            Ok(summary) => self.status = Some(summary),
            Err(e) => self.status = Some(e),
        }
    }

    /// The initial text of a transition prompt. A `RejectWork` prompt on a task
    /// with a tracked PR is pre-filled with that PR's review comments (DESIGN.md
    /// §11c), so a GitHub review reaches the agent without retyping; the human
    /// can still edit before submitting. Everything else — and a PR with no
    /// pullable comments, or a `gh` failure — starts empty, with the reason on
    /// the status line.
    fn prompt_seed(&mut self, task_id: i64, kind: PromptKind) -> String {
        if kind != PromptKind::RejectWork {
            return String::new();
        }
        let tracked = self
            .store
            .task(task_id)
            .ok()
            .and_then(|t| t.pr_url)
            .is_some();
        if !tracked {
            return String::new();
        }
        match crate::pr::pull_review_feedback(&self.store, task_id) {
            Ok(body) => {
                self.status = Some("pre-filled feedback from the PR's review comments".into());
                body
            }
            Err(e) => {
                self.status = Some(format!("{e}; type the feedback instead"));
                String::new()
            }
        }
    }

    /// Open the agent picker (DESIGN.md §8): agents are loaded from
    /// `agents.toml` right now, not cached from some earlier read, so a
    /// config that changed since the last dispatch — the usage-cap case this
    /// exists for — is always reflected. A load failure renders in the same
    /// status-line error style as a failed dispatch rather than opening an
    /// empty or stale modal.
    fn open_agent_picker(&mut self, task_id: i64, task_agent: Option<String>) {
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let agents = config.agent_names();
        if agents.is_empty() {
            self.status = Some("agents.toml defines no agents".into());
            return;
        }
        let resolved = config.resolve(task_agent.as_deref()).ok().map(|r| r.name);
        let sel = resolved
            .as_ref()
            .and_then(|name| agents.iter().position(|a| a == name))
            .unwrap_or(0);
        self.mode = Mode::AgentPicker {
            task_id,
            agents,
            resolved,
            sel,
        };
    }

    fn key_agent_picker(
        &mut self,
        key: KeyEvent,
        task_id: i64,
        agents: Vec<String>,
        resolved: Option<String>,
        mut sel: usize,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(agents.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                let agent = agents[sel].clone();
                self.dispatch_task(task_id, Some(agent));
                return;
            }
            _ => {}
        }
        self.mode = Mode::AgentPicker {
            task_id,
            agents,
            resolved,
            sel,
        };
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
            KeyCode::Char('r') => {
                if let Some(project) = self.projects.get(sel) {
                    self.mode = Mode::AddProject {
                        name: project.name.clone(),
                        path: project.path.clone(),
                        on_path: false,
                        editing: Some(project.id),
                    };
                }
                return;
            }
            KeyCode::Char('d') => {
                if let Some(project) = self.projects.get(sel) {
                    let id = project.id;
                    let result = self.store.delete_project(id).and_then(|_| self.refresh());
                    self.report(result);
                    sel = sel.min(self.projects.len().saturating_sub(1));
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
        editing: Option<i64>,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab => {
                self.mode = Mode::AddProject {
                    name,
                    path,
                    on_path: !on_path,
                    editing,
                };
                return;
            }
            KeyCode::Enter => {
                if !on_path {
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path: true,
                        editing,
                    };
                    return;
                }
                if name.trim().is_empty() {
                    self.status = Some("project name is required".into());
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path,
                        editing,
                    };
                    return;
                }
                let result = match editing {
                    Some(id) => self
                        .store
                        .rename_project(id, name.trim())
                        .and_then(|_| self.store.set_path(id, path.trim()))
                        .and_then(|_| self.refresh()),
                    None => self
                        .store
                        .create_project(name.trim(), path.trim())
                        .and_then(|_| self.refresh()),
                };
                if self.report(result).is_none() {
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path,
                        editing,
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
            editing,
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
                        let buffer = self.prompt_seed(task_id, kind);
                        self.mode = Mode::Prompt {
                            task_id,
                            kind,
                            buffer,
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

    fn key_detail(&mut self, key: KeyEvent, task_id: i64, mut scroll: u16) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return,
            KeyCode::Char('j') | KeyCode::Down => scroll = scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('s') => {
                if let Some(task) = self.all.iter().map(|r| &r.task).find(|t| t.id == task_id) {
                    let actions = Store::legal_actions(task.state);
                    if actions.is_empty() {
                        self.status = Some(format!("task is {} — nowhere to go", task.state));
                    } else {
                        self.mode = Mode::Transition {
                            task_id,
                            actions,
                            sel: 0,
                        };
                        return;
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::Detail { task_id, scroll };
    }

    fn key_history(&mut self, key: KeyEvent, task_id: i64, events: Vec<Event>, mut scroll: u16) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('h') => return,
            KeyCode::Char('j') | KeyCode::Down => scroll = scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => scroll = scroll.saturating_sub(1),
            _ => {}
        }
        self.mode = Mode::History {
            task_id,
            events,
            scroll,
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

#[cfg(test)]
mod tests {
    use super::*;
    use voro_core::{NewTask, Priority};

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    /// A `DispatchCtx` that is never actually used to spawn anything in these
    /// tests — none of them build up session history on a task before
    /// answering it, so `apply_and_refresh`'s continuation path never fires.
    fn dummy_ctx() -> crate::dispatch::DispatchCtx {
        crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new("/nonexistent/voro.db"))
    }

    /// A store with one project and one task per requested state, reached
    /// through the real transition machine.
    fn app_with(states: &[TaskState]) -> App {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        for state in states {
            let created = if *state == TaskState::Proposed {
                TaskState::Proposed
            } else {
                TaskState::Ready
            };
            let task = store
                .create_task(NewTask {
                    project_id: project.id,
                    title: format!("{state} task"),
                    body: String::new(),
                    priority: Priority::P1,
                    state: created,
                    agent: None,
                })
                .unwrap();
            match state {
                TaskState::Ready | TaskState::Proposed => {}
                TaskState::NeedsInput => {
                    store.apply(task.id, Action::Start).unwrap();
                    store.apply(task.id, Action::Ask("A or B?".into())).unwrap();
                }
                TaskState::Review => {
                    store.apply(task.id, Action::Start).unwrap();
                    store.apply(task.id, Action::Complete).unwrap();
                }
                TaskState::Done => {
                    store.apply(task.id, Action::Start).unwrap();
                    store.apply(task.id, Action::Complete).unwrap();
                    store.apply(task.id, Action::Accept).unwrap();
                }
                other => panic!("fixture does not build {other} tasks"),
            }
        }
        App::new(store, dummy_ctx()).unwrap()
    }

    #[test]
    fn enter_on_needs_input_row_answers_and_requeues() {
        let mut app = app_with(&[TaskState::NeedsInput]);
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Queue(_)
        ));
        assert_eq!(app.enter_hint(), Some("⏎ answer"));

        key(&mut app, KeyCode::Enter);
        let task_id = match app.mode {
            Mode::Prompt {
                task_id,
                kind: PromptKind::Answer,
                ..
            } => task_id,
            _ => panic!("enter on a needs-input row should open the answer prompt"),
        };

        key(&mut app, KeyCode::Char('B'));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        assert!(app.queue.is_empty());
    }

    /// A scratch database, a freshly-`git init`ed clean project, and (unless
    /// `agents_toml` is `None`, for the missing-config case) an `agents.toml`
    /// at that content — the same scratch shape `dispatch.rs`'s and
    /// `cli.rs`'s own tests use, duplicated here since those are private to
    /// their modules.
    fn scratch_env(
        name: &str,
        agents_toml: Option<&str>,
    ) -> (Store, crate::dispatch::DispatchCtx, std::path::PathBuf) {
        use std::process::{Command, Stdio};

        let root = std::env::temp_dir().join(format!(
            "voro-app-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project_path = root.join("project");
        std::fs::create_dir_all(&project_path).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&project_path)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let db_path = root.join("voro.db");
        let agents_path = root.join("agents.toml");
        if let Some(toml) = agents_toml {
            std::fs::write(&agents_path, toml).unwrap();
        }
        let store = Store::open(&db_path).unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
        };
        (store, ctx, project_path)
    }

    /// The TUI's answer action is the CLI's `voro answer` under a different
    /// keybinding (task #31, DESIGN.md §6): a task with prior session history
    /// gets a continuation dispatched automatically when answered here too.
    #[test]
    fn answering_a_task_with_session_history_triggers_a_continuation() {
        use std::process::{Command, Stdio};

        let root = std::env::temp_dir().join(format!(
            "voro-app-answer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project_path = root.join("project");
        std::fs::create_dir_all(&project_path).unwrap();
        let status = Command::new("git")
            .arg("-C")
            .arg(&project_path)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let db_path = root.join("voro.db");
        let agents_path = root.join("agents.toml");
        // The continuation this test triggers must still be alive when
        // `apply_and_refresh`'s own `self.refresh()` reconciles-on-read
        // immediately afterwards — an instantly-exiting stub (`cat`, as
        // `dispatch.rs`'s tests use) would otherwise race that read and get
        // finalised as a failed session before the assertions below run.
        std::fs::write(
            &agents_path,
            "default = \"stub\"\n\n[agents.stub]\ncmd = \"sleep 1 && cat {prompt_file}\"\n",
        )
        .unwrap();

        let mut store = Store::open(&db_path).unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: db_path.clone(),
            agents_path,
            runtime_dir: root.join("sessions"),
        };
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                title: "Do the thing".into(),
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        store.apply(task.id, Action::Ask("A or B?".into())).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('B'));
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Running);
        assert_eq!(app.store.sessions_for(task.id).unwrap().len(), 2);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("continued task"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn enter_on_review_row_opens_review_actions() {
        let mut app = app_with(&[TaskState::Review]);
        assert_eq!(app.enter_hint(), Some("⏎ review"));

        key(&mut app, KeyCode::Enter);
        match &app.mode {
            Mode::Transition { actions, .. } => {
                assert_eq!(*actions, Store::legal_actions(TaskState::Review));
            }
            _ => panic!("enter on a review row should open the transition menu"),
        }
    }

    #[test]
    fn enter_on_ready_row_leads_with_start() {
        let mut app = app_with(&[TaskState::Ready]);
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Queue(_)
        ));
        assert_eq!(app.enter_hint(), Some("⏎ act"));

        key(&mut app, KeyCode::Enter);
        let task_id = match &app.mode {
            Mode::Transition {
                actions,
                sel: 0,
                task_id,
            } => {
                assert_eq!(actions[0], Action::Start);
                *task_id
            }
            _ => panic!("enter on a ready row should open the transition menu"),
        };

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn enter_hint_is_absent_where_enter_does_nothing() {
        let mut app = app_with(&[]);
        assert_eq!(app.enter_hint(), None);
        app.toggle_screen();
        assert_eq!(app.screen, Screen::Tasks);
        assert_eq!(app.enter_hint(), None);
    }

    #[test]
    fn enter_on_proposed_row_opens_triage_menu() {
        let mut app = app_with(&[TaskState::Proposed]);
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Queue(_)
        ));
        assert_eq!(app.enter_hint(), Some("⏎ triage"));

        key(&mut app, KeyCode::Enter);
        let task_id = match &app.mode {
            Mode::Transition {
                actions, task_id, ..
            } => {
                assert_eq!(*actions, Store::legal_actions(TaskState::Proposed));
                *task_id
            }
            _ => panic!("enter on a proposed row should open the triage menu"),
        };

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Ready);
        // the triaged task re-enters the queue as startable work
        assert_eq!(app.queue.len(), 1);
        assert_eq!(app.enter_hint(), Some("⏎ act"));
    }

    #[test]
    fn tasks_screen_enter_opens_detail_then_transitions() {
        let mut app = app_with(&[TaskState::Ready]);
        app.toggle_screen();
        assert_eq!(app.enter_hint(), Some("⏎ view"));

        key(&mut app, KeyCode::Enter);
        let task_id = match app.mode {
            Mode::Detail { task_id, scroll: 0 } => task_id,
            _ => panic!("enter on a tasks-screen row should open the detail view"),
        };

        key(&mut app, KeyCode::Enter);
        match &app.mode {
            Mode::Transition { actions, .. } => {
                assert_eq!(*actions, Store::legal_actions(TaskState::Ready));
            }
            _ => panic!("enter in the detail view should open the transition menu"),
        }

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn h_opens_history_on_the_cockpit_and_the_task_browser() {
        let mut app = app_with(&[TaskState::NeedsInput]);

        key(&mut app, KeyCode::Char('h'));
        let events = match &app.mode {
            Mode::History {
                task_id, events, ..
            } => {
                assert_eq!(*task_id, app.queue[0].task.id);
                events.clone()
            }
            _ => panic!("h on a cockpit row should open the history popup"),
        };
        // created, then start, then ask — oldest first
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            vec!["created", "transition", "transition"]
        );

        key(&mut app, KeyCode::Char('j'));
        assert!(matches!(app.mode, Mode::History { scroll: 1, .. }));
        key(&mut app, KeyCode::Char('h'));
        assert!(matches!(app.mode, Mode::Normal));

        app.toggle_screen();
        key(&mut app, KeyCode::Char('h'));
        assert!(matches!(app.mode, Mode::History { .. }));
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn detail_view_scrolls_closes_and_dead_ends_gracefully() {
        let mut app = app_with(&[TaskState::Done]);
        app.toggle_screen();

        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char('k'));
        assert!(matches!(app.mode, Mode::Detail { scroll: 1, .. }));

        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Detail { .. }));
        assert!(app.status.as_deref().unwrap_or("").contains("nowhere"));

        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn weights_modal_rename_prefills_and_saves() {
        let mut app = app_with(&[]);
        let project_id = app.projects[0].id;
        key(&mut app, KeyCode::Char('w'));
        assert!(matches!(app.mode, Mode::Weights { sel: 0 }));

        key(&mut app, KeyCode::Char('r'));
        match &app.mode {
            Mode::AddProject {
                name,
                path,
                editing,
                ..
            } => {
                assert_eq!(name, "demo");
                assert_eq!(path, "/tmp/demo");
                assert_eq!(*editing, Some(project_id));
            }
            _ => panic!("r in weights mode should open the AddProject modal prefilled"),
        }

        for _ in 0.."demo".len() {
            key(&mut app, KeyCode::Backspace);
        }
        for c in "renamed".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter); // move to the path field
        key(&mut app, KeyCode::Enter); // save

        // saving closes the modal, matching the create-project flow
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.projects[0].id, project_id);
        assert_eq!(app.projects[0].name, "renamed");
        assert_eq!(app.projects[0].path, "/tmp/demo");
        // tasks reference the project by id, so renaming leaves them intact
        assert_eq!(app.store.project(project_id).unwrap().name, "renamed");
    }

    #[test]
    fn weights_modal_deletes_a_taskless_project() {
        let mut app = app_with(&[]);
        key(&mut app, KeyCode::Char('w'));
        key(&mut app, KeyCode::Char('d'));
        assert!(app.projects.is_empty());
        assert!(matches!(app.mode, Mode::Weights { .. }));
        assert!(app.status.is_none());
    }

    #[test]
    fn weights_modal_delete_refuses_when_project_has_a_task() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('w'));
        key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.projects.len(), 1);
        assert!(app.status.as_deref().unwrap_or("").contains("park"));
    }

    // --- dispatch keybindings (task #28, DESIGN.md §8/§9) ---

    /// `d` on a ready task dispatches it with the resolved agent — the same
    /// mechanics `voro dispatch` uses — and reports the success summary.
    #[test]
    fn dispatch_key_dispatches_a_ready_task_with_the_resolved_agent() {
        // `sleep 1 &&` keeps the stub process alive past `dispatch_task`'s own
        // `refresh()`, whose reconcile-on-read would otherwise race an
        // instantly-exiting stub and finalise the session as failed/ready
        // before the assertions below run (see the answer-continuation test
        // above for the same race).
        let (mut store, ctx, project_path) = scratch_env(
            "dispatch",
            Some("default = \"stub\"\n\n[agents.stub]\ncmd = \"sleep 1 && cat {prompt_file}\"\n"),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                title: "Do the thing".into(),
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Char('d'));

        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Running);
        let sessions = app.store.sessions_for(task.id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].agent, "stub");
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("dispatched task"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// Dispatch requires `ready` (DESIGN.md §8); on anything else the key
    /// no-ops with a status message rather than erroring deep inside dispatch
    /// or silently doing nothing, mirroring how `s` reports a state with
    /// nowhere to go.
    #[test]
    fn dispatch_key_on_a_non_ready_task_reports_and_does_not_mutate() {
        // `Done` never appears in the cockpit queue at all, so select it on
        // the Tasks screen instead, which lists every state.
        let mut app = app_with(&[TaskState::Done]);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('d'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("only ready tasks can be dispatched"),
            "{:?}",
            app.status
        );
    }

    /// `D` opens the picker listing every agent from `agents.toml`, with the
    /// one plain dispatch would resolve to marked regardless of cursor
    /// position; picking a different one dispatches with that override.
    #[test]
    fn agent_picker_lists_agents_resolved_marked_and_dispatches_the_choice() {
        // `sleep 1 &&` keeps the stub alive past the dispatch's own refresh,
        // for the same reconcile-on-read race noted above.
        let (mut store, ctx, project_path) = scratch_env(
            "picker",
            Some(
                "default = \"stub\"\n\n[agents.stub]\ncmd = \"sleep 1 && cat {prompt_file}\"\n\n\
                 [agents.special]\ncmd = \"sleep 1 && cat {prompt_file}\"\n",
            ),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Char('D'));

        let (agents, resolved_sel) = match &app.mode {
            Mode::AgentPicker {
                agents,
                resolved,
                sel,
                ..
            } => {
                assert_eq!(agents, &vec!["special".to_string(), "stub".to_string()]);
                assert_eq!(resolved.as_deref(), Some("stub"));
                (agents.clone(), *sel)
            }
            _ => panic!("D should open the agent picker"),
        };
        assert_eq!(
            agents[resolved_sel], "stub",
            "cursor starts on the resolved agent"
        );

        // move off the resolved default onto "special" and dispatch it
        key(&mut app, KeyCode::Char('k'));
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Running);
        assert_eq!(app.store.sessions_for(task.id).unwrap()[0].agent, "special");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A missing/invalid `agents.toml` is only discovered when the picker is
    /// opened — it is loaded fresh each time, never cached — and surfaces
    /// through the ordinary status-line error style instead of a stale or
    /// empty modal.
    #[test]
    fn agent_picker_reports_a_config_load_failure_without_opening() {
        let (mut store, ctx, project_path) = scratch_env("picker-missing", None);
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        store
            .create_task(NewTask {
                project_id: project.id,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Char('D'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status.is_some(),
            "a missing config should report an error"
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    // --- open-in-viewer keybinding (task #24, DESIGN.md §11a) ---

    /// `o` on a review row runs the configured `[viewer]` and reports the
    /// summary through the status line — the TUI half of `voro open`.
    #[test]
    fn open_key_opens_a_review_task_in_the_configured_viewer() {
        let (mut store, ctx, project_path) = scratch_env(
            "open",
            Some(
                "default = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
                 [viewer]\ncmd = \"true\"\n",
            ),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
            })
            .unwrap();
        store.apply(task.id, Action::Start).unwrap();
        store.apply(task.id, Action::Complete).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Char('o'));

        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains(&format!("opened task {}", task.id)),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// Only `review`/`running` tasks have a diff to open; anything else no-ops
    /// with an explanation rather than silently, mirroring the dispatch keys.
    #[test]
    fn open_key_on_a_non_review_task_reports_and_does_not_open() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('o'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("only review or running tasks"),
            "{:?}",
            app.status
        );
    }

    // --- PR tracking (task, DESIGN.md §11c) ---

    /// `g` on a task with no tracked PR reports through the status line rather
    /// than shelling out to `gh` — a network-free no-op-with-explanation.
    #[test]
    fn jump_to_pr_key_on_a_task_without_a_pr_reports() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('g'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no tracked PR"),
            "{:?}",
            app.status
        );
    }

    /// Rejecting a review task with no tracked PR opens the ordinary feedback
    /// prompt, empty — the pre-fill only fires when a PR is tracked (DESIGN.md
    /// §11c), so this path never touches `gh`.
    #[test]
    fn reject_prompt_starts_empty_without_a_tracked_pr() {
        let mut app = app_with(&[TaskState::Review]);
        key(&mut app, KeyCode::Enter); // transition menu for the review row
        key(&mut app, KeyCode::Char('j')); // Accept -> RejectWork
        key(&mut app, KeyCode::Enter);
        match &app.mode {
            Mode::Prompt {
                kind: PromptKind::RejectWork,
                buffer,
                ..
            } => assert!(buffer.is_empty(), "buffer was {buffer:?}"),
            _ => panic!("expected an empty reject prompt"),
        }
    }

    /// `D` shares the same readiness precondition as `d`.
    #[test]
    fn agent_picker_key_on_a_non_ready_task_reports_and_does_not_open() {
        let mut app = app_with(&[TaskState::Done]);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('D'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("only ready tasks can be dispatched"),
            "{:?}",
            app.status
        );
    }
}
