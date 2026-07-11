use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::{
    Action, AgentsConfig, Blocker, Candidate, Event, PrRef, Priority, Project, RunningRow,
    ScoreBreakdown, Store, Task, TaskState, Triage, scheduler,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Cockpit,
    Tasks,
    Projects,
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
    /// Collecting a GitHub PR reference to track on a task (DESIGN.md §11c),
    /// reached by pressing the jump-to-PR key on a task that has none. Unlike
    /// `Prompt`, this feeds a store mutation (`set_pr`), not a state
    /// transition, so it carries no `PromptKind`.
    LinkPr {
        task_id: i64,
        buffer: String,
    },
    /// Confirming that `pr` should push a review task's branch and open a ready
    /// PR (DESIGN.md §8), reached by the PR key on a review task that has none.
    /// The modal states exactly what is about to happen; confirming runs the
    /// same `crate::pr::create` core routine the CLI calls. A tracked PR skips
    /// this and jumps to the PR instead.
    ConfirmPr {
        task_id: i64,
        branch: String,
        title: String,
    },
    Detail {
        task_id: i64,
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

/// A request for main() to suspend the terminal and run an agent's
/// `attach`/`resume` command in the foreground (task #75) — like the editor,
/// these are full-screen interactive programs that own the terminal until
/// the user detaches or the session ends.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    /// The verb template with `{session}` already substituted.
    pub command: String,
    /// The project checkout to run it in.
    pub cwd: String,
}

pub fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Triage(Triage::Parked) => "triage → parked",
        Action::Triage(Triage::Ready) => "triage → ready",
        Action::Triage(Triage::Reject) => "triage → rejected",
        Action::Start => "start → running",
        Action::Ask(_) => "ask a question → needs-input",
        Action::Answer(_) => "answer the question → running",
        Action::Complete(_) => "complete → review",
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
    /// The cockpit's running strip (DESIGN.md §9): one row per `running` task
    /// with its open session if any, so a task started by hand or one whose
    /// session ended without reporting (reconcile leaves it running, §8) is
    /// still visible. Filtered on task state, so `review`/`needs-input` tasks —
    /// whose session stays open behind the scenes — stay in the queue, not here.
    pub running: Vec<RunningRow>,
    pub all: Vec<TaskRow>,
    /// Ready tasks whose most recent session ended `failed` or `capped`
    /// (DESIGN.md §8) — read fresh from session history on every refresh,
    /// never stored on the task itself.
    pub redispatch: std::collections::HashSet<i64>,
    /// Review tasks carrying only one of a branch and a summary (DESIGN.md §8):
    /// the half-finished done report a dispatched session left behind, which a
    /// PR cannot be opened from. Re-derived per refresh like `redispatch`.
    pub incomplete_report: std::collections::HashSet<i64>,

    pub cockpit_rows: Vec<CockpitRow>,
    pub cockpit_sel: usize,
    pub tasks_sel: usize,
    pub projects_sel: usize,

    pub mode: Mode,
    /// Whether the detail views fold the score decomposition (DESIGN.md §7) and
    /// the event history in — toggled by `x` and `h`. Held per app-state rather
    /// than per task so the choice persists as the selection moves, and shared
    /// by the cockpit detail pane and the tasks-screen Detail popup.
    pub show_score: bool,
    pub show_history: bool,
    pub pending_editor: Option<EditorRequest>,
    pub pending_attach: Option<AttachRequest>,

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
            incomplete_report: std::collections::HashSet::new(),
            cockpit_rows: Vec::new(),
            cockpit_sel: 0,
            tasks_sel: 0,
            projects_sel: 0,
            mode: Mode::Normal,
            show_score: false,
            show_history: false,
            pending_editor: None,
            pending_attach: None,
            last_data_version: 0,
        };
        app.refresh()?;
        app.last_data_version = app.store.data_version()?;
        Ok(app)
    }

    /// Where main() records the outcome of an attach/resume round-trip — the
    /// same rolling launch log a viewer open writes to (DESIGN.md §11a), so a
    /// failing attach leaves a breadcrumb even though the TUI paints back over
    /// its output.
    pub fn launch_log_path(&self) -> std::path::PathBuf {
        self.dispatch_ctx.launch_log_path()
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
        crate::reconcile::reconcile_live_sessions(&mut self.store, &self.dispatch_ctx.agents_path)?;

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
        self.incomplete_report = all
            .iter()
            .filter(|r| r.task.state == TaskState::Review)
            .filter_map(|r| {
                self.store
                    .incomplete_report_flag(r.task.id)
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
        self.projects_sel = self.projects_sel.min(self.projects.len().saturating_sub(1));
        Ok(())
    }

    pub fn selected_task_id(&self) -> Option<i64> {
        match self.screen {
            Screen::Cockpit => match self.cockpit_rows.get(self.cockpit_sel)? {
                CockpitRow::Queue(i) => Some(self.queue.get(*i)?.task.id),
                CockpitRow::Running(i) => Some(self.running.get(*i)?.task_id),
            },
            Screen::Tasks => Some(self.all.get(self.tasks_sel)?.task.id),
            Screen::Projects => None,
        }
    }

    pub fn move_selection(&mut self, delta: i64) {
        let (sel, len) = match self.screen {
            Screen::Cockpit => (&mut self.cockpit_sel, self.cockpit_rows.len()),
            Screen::Tasks => (&mut self.tasks_sel, self.all.len()),
            Screen::Projects => (&mut self.projects_sel, self.projects.len()),
        };
        if len == 0 {
            return;
        }
        *sel = (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
    }

    /// Tab cycles cockpit → tasks → projects → cockpit; `1`/`2`/`3` jump
    /// directly (DESIGN.md §9).
    pub fn toggle_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Cockpit => Screen::Tasks,
            Screen::Tasks => Screen::Projects,
            Screen::Projects => Screen::Cockpit,
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
            Screen::Projects => None,
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

    /// Apply a transition and refresh. An `Answer` or `RejectWork` on a task
    /// with prior session history additionally triggers a continuation
    /// dispatch (DESIGN.md §6/§8: "fed to the session" means resuming the work
    /// with the answer or feedback in hand, not writing to a live pipe) — the
    /// same rule and the same mechanics `voro answer`/`voro reject` use on the
    /// CLI, so the two stay consistent. Because review keeps the session open,
    /// a reject continues the *same* agent session. A task only ever started by
    /// hand has no session history and the transition stands alone.
    fn apply_and_refresh(&mut self, task_id: i64, action: Action) {
        let continuation_input = match &action {
            Action::Answer(text) | Action::RejectWork(text) => Some(text.clone()),
            _ => None,
        };
        let has_history = continuation_input.is_some()
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
                    continuation_input.as_deref(),
                ) {
                    Ok(summary) => self.status = Some(summary),
                    Err(e) => {
                        self.status =
                            Some(format!("transition applied, but continuation failed: {e}"))
                    }
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
            Mode::LinkPr { task_id, buffer } => self.key_link_pr(key, task_id, buffer),
            Mode::ConfirmPr {
                task_id,
                branch,
                title,
            } => self.key_confirm_pr(key, task_id, branch, title),
            Mode::Detail { task_id, scroll } => self.key_detail(key, task_id, scroll),
            Mode::AgentPicker {
                task_id,
                agents,
                resolved,
                sel,
            } => self.key_agent_picker(key, task_id, agents, resolved, sel),
        }
    }

    fn key_normal(&mut self, key: KeyEvent) {
        // Navigation shared by every screen: quit, screen switching (tab
        // cycles, `1`/`2`/`3` jump), and moving the selection.
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Tab => {
                self.toggle_screen();
                return;
            }
            KeyCode::Char('1') => {
                self.screen = Screen::Cockpit;
                return;
            }
            KeyCode::Char('2') => {
                self.screen = Screen::Tasks;
                return;
            }
            KeyCode::Char('3') => {
                self.screen = Screen::Projects;
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                return;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                return;
            }
            _ => {}
        }
        // The projects screen owns weight/admin (DESIGN.md §9); its keys
        // (`0`–`5`, `r`, `a`, `d`) are local and reinterpret keys that mean
        // other things on the task-oriented screens.
        if self.screen == Screen::Projects {
            self.key_projects(key);
            return;
        }
        match key.code {
            KeyCode::Char('r') => {
                let result = self.refresh();
                self.report(result);
            }
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Char('n') => match self.projects.len() {
                0 => {
                    self.status =
                        Some("no projects yet — add one on the projects screen (3)".into())
                }
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
            // The score and history sections live in the detail pane on the
            // cockpit and in the Detail popup on the tasks screen; on the
            // cockpit `x`/`h` fold them into the pane in place, while on the
            // tasks screen they are local to the popup (see `key_detail`).
            KeyCode::Char('x') if self.screen == Screen::Cockpit => {
                self.show_score = !self.show_score;
            }
            KeyCode::Char('h') if self.screen == Screen::Cockpit => {
                self.show_history = !self.show_history;
            }
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
            KeyCode::Char('a') => self.jump_into_session(),
            _ => {}
        }
    }

    /// Jump into the selected task's agent session (task #75): `attach` for a
    /// running task, `resume` for a review or redispatch-flagged one. The
    /// actual run happens in main() via `pending_attach`, with the TUI torn
    /// down around it — attach/resume are full-screen interactive. Every
    /// missing piece (state, session, captured ref, verb) reports through the
    /// status line, the same "no-op with an explanation" style the dispatch
    /// keys use.
    fn jump_into_session(&mut self) {
        let (task_id, state, project_id) = match self.selected_task() {
            Some(task) => (task.id, task.state, task.project_id),
            None => return,
        };
        let attach = match state {
            TaskState::Running => true,
            TaskState::Review => false,
            TaskState::Ready if self.redispatch.contains(&task_id) => false,
            _ => {
                self.status = Some(format!(
                    "task is {state} — jump-in works on running, review, or \
                     redispatch-flagged tasks"
                ));
                return;
            }
        };
        let sessions = match self.store.sessions_for(task_id) {
            Ok(sessions) => sessions,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let Some(session) = sessions.first() else {
            self.status = Some(format!(
                "task {task_id} has no recorded session to jump into"
            ));
            return;
        };
        let Some(session_ref) = session.session_ref.clone() else {
            self.status = Some(format!(
                "no session reference was captured for session {} — nothing to {}",
                session.id,
                if attach { "attach to" } else { "resume" }
            ));
            return;
        };
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let verb_name = if attach { "attach" } else { "resume" };
        let template = config
            .agent(&session.agent)
            .and_then(|a| if attach { a.attach() } else { a.resume() });
        let Some(template) = template else {
            self.status = Some(format!(
                "agent '{}' defines no {verb_name} template in {}",
                session.agent,
                self.dispatch_ctx.agents_path.display()
            ));
            return;
        };
        let project = match self.store.project(project_id) {
            Ok(project) => project,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        self.pending_attach = Some(AttachRequest {
            command: template.replace(
                voro_core::SESSION_PLACEHOLDER,
                &crate::dispatch::shell_quote(std::path::Path::new(&session_ref)),
            ),
            cwd: project.path,
        });
    }

    /// The score decomposition (DESIGN.md §7) for a task, for the detail
    /// views' `x` toggle. A failed lookup yields `None` so the section is
    /// simply omitted rather than surfacing an error mid-render.
    pub fn score_breakdown(&self, task_id: i64) -> Option<ScoreBreakdown> {
        self.store.explain(task_id).ok()
    }

    /// A task's event history, oldest first, for the detail views' `h` toggle.
    /// A read error yields an empty history for the same reason.
    pub fn task_events(&self, task_id: i64) -> Vec<Event> {
        self.store.events_for(task_id).unwrap_or_default()
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

    /// The PR key. With a tracked PR: jump to it in a browser (DESIGN.md §11c),
    /// which never touches the store, so no refresh follows. With none: on a
    /// `review` task, open the confirmation modal to *create* one from its
    /// done-time summary (DESIGN.md §8) — or, if it is not yet PR-ready, report
    /// the missing branch/summary on the status line; on any other state, fall
    /// back to the link-an-existing-PR prompt (the TUI face of `set --pr`).
    fn open_selected_pr(&mut self) {
        let Some(task) = self.selected_task() else {
            return;
        };
        let (id, state) = (task.id, task.state);
        let has_pr = task.pr_url.is_some();
        if has_pr {
            match crate::pr::open(&self.store, id) {
                Ok(summary) => self.status = Some(summary),
                Err(e) => self.status = Some(e),
            }
            return;
        }
        if state != TaskState::Review {
            self.mode = Mode::LinkPr {
                task_id: id,
                buffer: String::new(),
            };
            return;
        }
        match crate::pr::plan(&self.store, id) {
            Ok(plan) => {
                self.mode = Mode::ConfirmPr {
                    task_id: id,
                    branch: plan.branch,
                    title: plan.title,
                }
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// Drive the create-PR confirmation modal (DESIGN.md §8). Enter (or `y`)
    /// runs the same `crate::pr::create` routine the CLI's `pr` calls — push
    /// the branch, open a ready PR, record the URL — then refreshes so the row
    /// flips to "PR open"; esc (or `n`) cancels without touching anything.
    fn key_confirm_pr(&mut self, key: KeyEvent, task_id: i64, branch: String, title: String) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                match crate::pr::create(&mut self.store, task_id) {
                    Ok(summary) => self.status = Some(summary),
                    Err(e) => self.status = Some(e),
                }
                let result = self.refresh();
                self.report(result);
            }
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.status = Some(format!("cancelled — no PR opened for #{task_id}"));
            }
            _ => {
                self.mode = Mode::ConfirmPr {
                    task_id,
                    branch,
                    title,
                };
            }
        }
    }

    /// Drive the link-a-PR prompt (DESIGN.md §11c). Enter validates and stores
    /// the reference; esc cancels. The buffer is one line — a PR URL or the
    /// `owner/repo#n` shorthand — so this stays a simple line editor.
    fn key_link_pr(&mut self, key: KeyEvent, task_id: i64, mut buffer: String) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.link_pr(task_id, &buffer);
                return;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        self.mode = Mode::LinkPr { task_id, buffer };
    }

    /// Validate and track a PR reference on a task, then refresh so the new
    /// link shows immediately. An unparseable reference keeps the prompt open
    /// with the typed text intact and the parse error on the status line, so
    /// the human can fix a typo without retyping the whole URL.
    fn link_pr(&mut self, task_id: i64, raw: &str) {
        let pr = match PrRef::parse(raw) {
            Ok(pr) => pr,
            Err(e) => {
                self.status = Some(e.to_string());
                self.mode = Mode::LinkPr {
                    task_id,
                    buffer: raw.to_string(),
                };
                return;
            }
        };
        if let Err(e) = self.store.set_pr(task_id, Some(&pr.url)) {
            self.status = Some(e.to_string());
            return;
        }
        self.status = Some(format!("linked {}", pr.url));
        let result = self.refresh();
        self.report(result);
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

    /// The projects screen's local keys (DESIGN.md §9). `0`–`5` sets the
    /// selected project's weight immediately — the every-morning action, one
    /// keystroke per project; `r` opens the AddProject form pre-filled to
    /// rename/re-path, `a` opens it blank to add a project, `d` deletes behind
    /// the store's own guard (only projects with no tasks). Movement and
    /// screen switching are handled by `key_normal` before it delegates here.
    fn key_projects(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c @ '0'..='5') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let id = project.id;
                    let result = self
                        .store
                        .set_weight(id, c.to_digit(10).unwrap() as i64)
                        .and_then(|_| self.refresh());
                    self.report(result);
                }
            }
            KeyCode::Char('r') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    self.mode = Mode::AddProject {
                        name: project.name.clone(),
                        path: project.path.clone(),
                        on_path: false,
                        editing: Some(project.id),
                    };
                }
            }
            KeyCode::Char('a') => {
                self.mode = Mode::AddProject {
                    name: String::new(),
                    path: String::new(),
                    on_path: false,
                    editing: None,
                };
            }
            KeyCode::Char('d') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let id = project.id;
                    let result = self.store.delete_project(id).and_then(|_| self.refresh());
                    self.report(result);
                    self.projects_sel =
                        self.projects_sel.min(self.projects.len().saturating_sub(1));
                }
            }
            _ => {}
        }
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
            // Fold the score and history sections into the popup in place; the
            // toggles are shared with the cockpit detail pane.
            KeyCode::Char('x') => self.show_score = !self.show_score,
            KeyCode::Char('h') => self.show_history = !self.show_history,
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
            KeyCode::Char(c @ '0'..='3') => {
                if let Ok(priority) = Priority::from_int((c as u8 - b'0') as i64) {
                    self.set_priority(task_id, priority);
                }
            }
            _ => {}
        }
        self.mode = Mode::Detail { task_id, scroll };
    }

    /// Re-prioritise the viewed task in place (task #88), the review-time fast
    /// path that skips the edit form. Routes through `voro-core` so the change
    /// is logged, then refreshes to re-score and re-sort.
    fn set_priority(&mut self, task_id: i64, priority: Priority) {
        match self.store.set_priority(task_id, priority) {
            Ok(_) => {
                self.status = Some(format!("priority set to {priority}"));
                let result = self.refresh();
                self.report(result);
            }
            Err(e) => self.status = Some(e.to_string()),
        }
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
                    store.apply(task.id, Action::Complete(None)).unwrap();
                }
                TaskState::Done => {
                    store.apply(task.id, Action::Start).unwrap();
                    store.apply(task.id, Action::Complete(None)).unwrap();
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
            ref_capture_timeout: std::time::Duration::ZERO,
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
            ref_capture_timeout: std::time::Duration::ZERO,
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

    /// `x` and `h` fold the score and history sections into the cockpit detail
    /// pane in place — they flip per-app-state flags, not popups, and stay in
    /// Normal mode so the pane keeps following the selection.
    #[test]
    fn x_and_h_toggle_the_cockpit_detail_sections() {
        let mut app = app_with(&[TaskState::NeedsInput]);
        assert!(!app.show_score && !app.show_history);

        key(&mut app, KeyCode::Char('x'));
        assert!(app.show_score);
        assert!(matches!(app.mode, Mode::Normal));
        key(&mut app, KeyCode::Char('h'));
        assert!(app.show_history);
        assert!(matches!(app.mode, Mode::Normal));

        // the same keys close the sections again
        key(&mut app, KeyCode::Char('x'));
        key(&mut app, KeyCode::Char('h'));
        assert!(!app.show_score && !app.show_history);
    }

    /// The event history the `h` toggle draws comes straight from the store,
    /// oldest first — the data the retired popup used to load for itself.
    #[test]
    fn task_events_reads_history_oldest_first() {
        let app = app_with(&[TaskState::NeedsInput]);
        let events = app.task_events(app.queue[0].task.id);
        // created, then start, then ask — oldest first
        assert_eq!(
            events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            vec!["created", "transition", "transition"]
        );
    }

    /// On the tasks screen the sections live inside the Detail popup: `x`/`h`
    /// on the list itself do nothing, but inside the popup they toggle the same
    /// shared flags without closing it, and the choice persists back out to the
    /// cockpit.
    #[test]
    fn tasks_screen_toggles_score_and_history_inside_the_detail_popup() {
        let mut app = app_with(&[TaskState::Ready]);
        app.toggle_screen();
        assert_eq!(app.screen, Screen::Tasks);

        // inert on the list — the sections are a popup concern here
        key(&mut app, KeyCode::Char('x'));
        key(&mut app, KeyCode::Char('h'));
        assert!(!app.show_score && !app.show_history);

        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Detail { .. }));
        key(&mut app, KeyCode::Char('x'));
        assert!(app.show_score);
        assert!(
            matches!(app.mode, Mode::Detail { .. }),
            "toggling score keeps the detail popup open"
        );
        key(&mut app, KeyCode::Char('h'));
        assert!(app.show_history);
        assert!(matches!(app.mode, Mode::Detail { .. }));

        // the flags outlive the popup and the screen switch
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Cockpit);
        assert!(app.show_score && app.show_history);
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

    // --- projects screen (task, DESIGN.md §9) ---

    /// Tab cycles cockpit → tasks → projects → cockpit, and `1`/`2`/`3` jump
    /// to a screen directly regardless of where the cursor is.
    #[test]
    fn tab_and_digits_move_between_the_three_screens() {
        let mut app = app_with(&[]);
        assert_eq!(app.screen, Screen::Cockpit);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Tasks);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Projects);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Cockpit);

        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Cockpit);
        key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.screen, Screen::Tasks);
    }

    /// The morning ritual: `0`–`5` on the projects screen sets the selected
    /// project's weight through the store in a single keystroke.
    #[test]
    fn digit_on_projects_screen_sets_weight_through_the_store() {
        let mut app = app_with(&[]);
        let project_id = app.projects[0].id;
        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);

        key(&mut app, KeyCode::Char('5'));
        assert_eq!(app.projects[0].weight, 5);
        assert_eq!(app.store.project(project_id).unwrap().weight, 5);

        key(&mut app, KeyCode::Char('0'));
        assert_eq!(app.store.project(project_id).unwrap().weight, 0);
    }

    #[test]
    fn projects_screen_rename_prefills_and_saves() {
        let mut app = app_with(&[]);
        let project_id = app.projects[0].id;
        key(&mut app, KeyCode::Char('3'));

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
            _ => panic!("r on the projects screen should open the AddProject modal prefilled"),
        }

        for _ in 0.."demo".len() {
            key(&mut app, KeyCode::Backspace);
        }
        for c in "renamed".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter); // move to the path field
        for _ in 0.."/tmp/demo".len() {
            key(&mut app, KeyCode::Backspace);
        }
        for c in "/tmp/moved".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter); // save

        // saving closes the form, matching the create-project flow
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.projects[0].id, project_id);
        assert_eq!(app.projects[0].name, "renamed");
        // the same form re-paths in one save
        assert_eq!(app.projects[0].path, "/tmp/moved");
        // tasks reference the project by id, so renaming leaves them intact
        let stored = app.store.project(project_id).unwrap();
        assert_eq!(stored.name, "renamed");
        assert_eq!(stored.path, "/tmp/moved");
    }

    /// `a` opens a blank AddProject form to add a new project.
    #[test]
    fn projects_screen_add_opens_a_blank_form() {
        let mut app = app_with(&[]);
        key(&mut app, KeyCode::Char('3'));
        key(&mut app, KeyCode::Char('a'));
        match &app.mode {
            Mode::AddProject {
                name,
                path,
                editing,
                ..
            } => {
                assert!(name.is_empty());
                assert!(path.is_empty());
                assert_eq!(*editing, None);
            }
            _ => panic!("a on the projects screen should open a blank AddProject form"),
        }
    }

    #[test]
    fn projects_screen_deletes_a_taskless_project() {
        let mut app = app_with(&[]);
        key(&mut app, KeyCode::Char('3'));
        key(&mut app, KeyCode::Char('d'));
        assert!(app.projects.is_empty());
        assert_eq!(app.screen, Screen::Projects);
        assert!(app.status.is_none());
    }

    #[test]
    fn projects_screen_delete_refuses_when_project_has_a_task() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('3'));
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

    // --- jump-in keybinding (task #75) ---

    /// A project with one dispatched task whose agent defines the session
    /// verbs, its session's ref recorded, and a canned `sessions` listing
    /// that keeps reconciliation believing the session is live.
    fn jump_in_env() -> (Store, crate::dispatch::DispatchCtx, i64, std::path::PathBuf) {
        let (mut store, ctx, project_path) = scratch_env("jumpin", None);
        let listing = project_path.parent().unwrap().join("listing.json");
        std::fs::write(&listing, r#"[{"sessionId": "ref-1", "state": "working"}]"#).unwrap();
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}}\"\n\
                 sessions = \"cat '{}'\"\n\
                 attach = \"agent attach {{session}}\"\n\
                 resume = \"agent resume {{session}}\"\n",
                listing.display()
            ),
        )
        .unwrap();
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
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        let session_id = store.sessions_for(task.id).unwrap()[0].id;
        store.set_session_ref(session_id, "ref-1").unwrap();
        (store, ctx, task.id, project_path)
    }

    /// `a` on a running task queues the agent's `attach` command — ref
    /// substituted, project path as cwd — for main() to run with the TUI
    /// suspended.
    #[test]
    fn attach_key_prepares_the_attach_command_for_a_running_task() {
        let (store, ctx, _task_id, project_path) = jump_in_env();
        let mut app = App::new(store, ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('a'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");
        assert_eq!(request.cwd, project_path.to_str().unwrap());

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// `a` on a review task uses `resume` — the session is finished; the
    /// point is reopening it, not attaching to a live one.
    #[test]
    fn attach_key_uses_resume_for_a_review_task() {
        let (mut store, ctx, task_id, project_path) = jump_in_env();
        store.apply(task_id, Action::Complete(None)).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('a'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// Without a captured ref there is nothing to substitute into the verb;
    /// the key explains instead of queuing a broken command. The fixture's
    /// verb-less stub agent also exercises the pid-reconcile path: the dead
    /// session is finalised but the task is left running (DESIGN.md §8),
    /// surfaced as an orphaned running row whose jump-in is `attach`.
    #[test]
    fn attach_key_without_a_captured_ref_reports_and_does_nothing() {
        let (mut store, ctx, project_path) = scratch_env(
            "jumpin-noref",
            Some(
                "default = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {prompt_file}\"\n\
                 attach = \"agent attach {session}\"\n\
                 resume = \"agent resume {session}\"\n",
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
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        // the stub exits immediately; wait for it so App::new's
        // reconcile-on-read reliably finds the pid dead
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut app = App::new(store, ctx).unwrap();
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Running);
        // the dead session's task is left running and surfaced as an orphan
        // (no live session) in the running strip, not auto-flagged for redispatch
        assert!(!app.redispatch.contains(&task.id));
        assert!(
            app.running
                .iter()
                .any(|r| r.task_id == task.id && r.session_id.is_none()),
            "orphaned running row expected: {:?}",
            app.running
        );
        app.toggle_screen();
        key(&mut app, KeyCode::Char('a'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no session reference"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// States with no session to jump into no-op with an explanation.
    #[test]
    fn attach_key_on_an_unflagged_ready_task_reports_the_states_that_work() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('a'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("jump-in"),
            "{:?}",
            app.status
        );
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
        store.apply(task.id, Action::Complete(None)).unwrap();

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

    /// `g` on a task with no tracked PR opens the link-a-PR prompt rather than
    /// shelling out to `gh` — a network-free path to set one from the TUI.
    #[test]
    fn jump_to_pr_key_on_a_task_without_a_pr_opens_the_link_prompt() {
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.selected_task_id().unwrap();
        key(&mut app, KeyCode::Char('g'));
        match app.mode {
            Mode::LinkPr {
                task_id: id,
                ref buffer,
            } => {
                assert_eq!(id, task_id);
                assert!(buffer.is_empty(), "buffer was {buffer:?}");
            }
            _ => panic!("expected the link-PR prompt, got {:?}", app.status),
        }
    }

    /// Typing a reference and submitting tracks it (canonicalised) on the task
    /// and closes the prompt, so the link shows without touching the CLI.
    #[test]
    fn link_pr_prompt_stores_a_valid_reference() {
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.selected_task_id().unwrap();
        key(&mut app, KeyCode::Char('g'));
        for c in "acme/widget#7".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            app.store.task(task_id).unwrap().pr_url.as_deref(),
            Some("https://github.com/acme/widget/pull/7")
        );
        assert!(
            app.status.as_deref().unwrap_or("").contains("linked"),
            "{:?}",
            app.status
        );
    }

    /// An unparseable reference keeps the prompt open with the typed text
    /// intact and the parse error on the status line, so a typo is fixable
    /// without retyping.
    #[test]
    fn link_pr_prompt_keeps_prompt_open_on_an_invalid_reference() {
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.selected_task_id().unwrap();
        key(&mut app, KeyCode::Char('g'));
        for c in "not-a-pr".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        match app.mode {
            Mode::LinkPr { ref buffer, .. } => assert_eq!(buffer, "not-a-pr"),
            _ => panic!("expected the prompt to stay open"),
        }
        assert!(app.status.is_some());
        assert!(app.store.task(task_id).unwrap().pr_url.is_none());
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
