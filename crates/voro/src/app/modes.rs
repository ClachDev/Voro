//! The modal modes (DESIGN.md §9) and the keys of the modals every task
//! screen shares: the transition menu, prompts, detail popup and pickers.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::{Action, AgentsConfig, Priority, Task};

use super::{App, CreateFlow, DefaultKind, ViewerFormState, ViewerOption, transition_actions};
use crate::dispatch::Filing;

/// What a text prompt is collecting, and the transition it feeds — or, for the
/// two launch kinds, the agent launch it feeds instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    Ask,
    RejectWork,
    /// The one-line brief a note-driven refine hands its agent (DESIGN.md §6).
    RefineNote,
    /// The one line the quick-message key says into a task's existing agent
    /// session (DESIGN.md §8).
    SessionMessage,
}

impl PromptKind {
    pub fn title(self) -> &'static str {
        match self {
            PromptKind::Ask => "Question",
            PromptKind::RejectWork => "Rejection feedback",
            PromptKind::RefineNote => "Refine note — what needs fixing",
            PromptKind::SessionMessage => "Message to the agent's session",
        }
    }

    /// The transition this prompt feeds, where it feeds one. The two launch
    /// kinds feed none from *here*: a refine note's round opens with its own
    /// write (DESIGN.md §6), and a session message applies its own transition
    /// — a reject-with-feedback, on a review or waiting task — before it
    /// sends, so the send never outruns the state.
    fn action(self, text: String) -> Option<Action> {
        match self {
            PromptKind::Ask => Some(Action::Ask(text)),
            PromptKind::RejectWork => Some(Action::RejectWork(text)),
            PromptKind::RefineNote | PromptKind::SessionMessage => None,
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
        flow: CreateFlow,
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
    /// Collecting a GitHub PR reference to track on a task (DESIGN.md §11c).
    /// Unlike `Prompt`, this feeds a store mutation (`set_pr`), not a state
    /// transition, so it carries no `PromptKind`.
    LinkPr {
        task_id: i64,
        buffer: String,
    },
    /// Collecting the dispatch WIP cap from the Config screen's settings list
    /// (DESIGN.md §5/§7). A one-line entry like [`LinkPr`](Mode::LinkPr), and
    /// like it it names nothing else: the setting it writes is the one the
    /// selection was on when it opened.
    EditMaxRunning {
        buffer: String,
    },
    /// Collecting the one line `n` expands into a task or a milestone
    /// (DESIGN.md §6/§8). Like `LinkPr` and unlike `Prompt` it names no task —
    /// there is none yet — so it carries the project the proposal will land in
    /// instead.
    QuickCreate {
        project_id: i64,
        filing: Filing,
        buffer: String,
    },
    /// Confirming that `pr` should push a review task's branch and open a ready
    /// PR (DESIGN.md §8). Confirming runs the same `crate::pr::create` the CLI
    /// calls; a tracked PR skips this and jumps to the PR instead.
    ConfirmPr {
        task_id: i64,
        branch: String,
        title: String,
    },
    Detail {
        task_id: i64,
        scroll: u16,
    },
    /// Dispatch-via-picker (DESIGN.md §8): agents loaded fresh from `voro.toml`
    /// when the picker opens, to catch a config changed since the last dispatch.
    AgentPicker {
        task_id: i64,
        agents: Vec<String>,
        /// The agent that plain dispatch (the resolved-agent key) would use —
        /// the task's own override, else the config default — highlighted in
        /// the list independently of cursor position.
        resolved: Option<String>,
        sel: usize,
    },
    /// Attaching a task to milestones or detaching it (DESIGN.md §9): every
    /// open milestone, the task's own project's first, with ⏎ adding or
    /// removing the task's `blocks` edge to the highlighted one in place. The
    /// ticks are read from the App's per-refresh dependency map, as the
    /// document picker's are; `back` is the detail popup's scroll to return to.
    MilestonePicker {
        task_id: i64,
        milestones: Vec<Task>,
        sel: usize,
        back: Option<u16>,
    },
    /// Toggling a task's document links (DESIGN.md §3/§8): every registered
    /// document, the task's own project's first, with ⏎ linking or unlinking the
    /// highlighted one in place. Which are linked is read from the App's own
    /// per-refresh map rather than carried here, so a toggle's refresh is the
    /// only thing the list needs to stay current. `back` is the detail popup's
    /// scroll when the picker was opened from it, so closing returns there.
    DocPicker {
        task_id: i64,
        docs: Vec<voro_core::Doc>,
        sel: usize,
        back: Option<u16>,
    },
    /// Picking a project's viewer on the projects screen (DESIGN.md §8/§11a):
    /// the default viewer, each named viewer from `voro.toml`, and a trailing
    /// "new viewer…" that opens the add-viewer form. Loaded fresh so a
    /// just-added viewer shows up.
    ViewerPicker {
        project_id: i64,
        options: Vec<ViewerOption>,
        /// The viewer the project names as stored, flagged in the list
        /// independently of cursor position.
        current: Option<String>,
        sel: usize,
    },
    /// The add/edit-viewer form on the Config screen (DESIGN.md §5): a name and
    /// a command template. Both paths — the Config screen and the review-action
    /// picker's "new viewer…" — share it.
    ViewerForm(ViewerFormState),
    /// The current screen's full key map (DESIGN.md §9), opened with `?`. It is
    /// a peek rather than a screen — any key but `tab` dismisses it — and the
    /// page is the only state it carries, since the screen it describes is the
    /// App's. It counts up without bound; the renderer knows how many pages the
    /// terminal makes of the map and wraps within them.
    KeyMap {
        page: usize,
    },
    /// Picking `default_agent` or `default_viewer` from the configured set
    /// (DESIGN.md §5), on the Config screen.
    DefaultPicker {
        kind: DefaultKind,
        names: Vec<String>,
        current: Option<String>,
        sel: usize,
    },
}

impl Mode {
    /// The cursor of a pick-from-list popup, where this mode is one. The
    /// text-entry forms and the detail popup have none, which is what makes them
    /// ignore the mouse (DESIGN.md §9).
    fn picker_sel(&self) -> Option<usize> {
        match self {
            Mode::PickProject { sel, .. }
            | Mode::Transition { sel, .. }
            | Mode::AgentPicker { sel, .. }
            | Mode::DocPicker { sel, .. }
            | Mode::MilestonePicker { sel, .. }
            | Mode::ViewerPicker { sel, .. }
            | Mode::DefaultPicker { sel, .. } => Some(*sel),
            _ => None,
        }
    }

    fn picker_sel_mut(&mut self) -> Option<&mut usize> {
        match self {
            Mode::PickProject { sel, .. }
            | Mode::Transition { sel, .. }
            | Mode::AgentPicker { sel, .. }
            | Mode::DocPicker { sel, .. }
            | Mode::MilestonePicker { sel, .. }
            | Mode::ViewerPicker { sel, .. }
            | Mode::DefaultPicker { sel, .. } => Some(sel),
            _ => None,
        }
    }
}

impl App {
    /// A click on a picker option: move the cursor there, or — if it is already
    /// there — hand the picker the Enter its key handler answers, so clicking
    /// confirms through exactly the path the keyboard takes.
    pub(super) fn click_picker_option(&mut self, index: usize) {
        match self.mode.picker_sel() {
            Some(sel) if sel == index => self.on_key(KeyEvent::from(KeyCode::Enter)),
            Some(_) => {
                if let Some(sel) = self.mode.picker_sel_mut() {
                    *sel = index;
                }
            }
            None => {}
        }
    }

    /// Drive the create-PR confirmation modal (DESIGN.md §8). Enter (or `y`)
    /// hands the push and the `gh pr create` to a background thread and closes
    /// the modal at once — the browser waits on the URL, the operator does not
    /// — and `poll_pr_create` records and opens what lands; esc (or `n`)
    /// cancels without touching anything.
    pub(super) fn key_confirm_pr(
        &mut self,
        key: KeyEvent,
        task_id: i64,
        branch: String,
        title: String,
    ) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.start_pr_create(task_id);
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
    pub(super) fn key_link_pr(&mut self, key: KeyEvent, task_id: i64, mut buffer: String) {
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

    /// Drive the quick-create prompt (DESIGN.md §6/§8). Enter hands the typed
    /// line to a background agent, esc cancels. The buffer is one line — the
    /// terse intent the agent expands — so this stays a simple line editor.
    pub(super) fn key_quick_create(
        &mut self,
        key: KeyEvent,
        project_id: i64,
        filing: Filing,
        mut buffer: String,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.quick_propose(project_id, filing, &buffer);
                return;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        self.mode = Mode::QuickCreate {
            project_id,
            filing,
            buffer,
        };
    }

    /// Open the agent picker (DESIGN.md §8): agents are loaded from `voro.toml`
    /// now, not cached, so a config changed since the last dispatch — the
    /// usage-cap case this exists for — is reflected. A load failure reports via
    /// the status line rather than opening an empty or stale modal.
    pub(super) fn open_agent_picker(&mut self, task_id: i64, task_agent: Option<String>) {
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let agents = config.agent_names();
        if agents.is_empty() {
            self.status = Some("no agents are configured".into());
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

    pub(super) fn key_agent_picker(
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

    /// Open the document picker on a task (DESIGN.md §8): every registered
    /// document, since a task in any project may cite any plan (§3), with the
    /// task's own project's listed first — the ones a triage is most likely to
    /// reach for. Read fresh from the store rather than from the refresh cache,
    /// which only holds documents something already links to. Returns whether it
    /// opened, so a caller with a screen to restore knows to give way.
    pub(super) fn open_doc_picker(&mut self, task_id: i64, back: Option<u16>) -> bool {
        let Ok(task) = self.store.task(task_id) else {
            return false;
        };
        let mut docs = match self.store.all_docs() {
            Ok(docs) => docs,
            Err(e) => {
                self.status = Some(e.to_string());
                return false;
            }
        };
        if docs.is_empty() {
            self.status =
                Some("no documents registered — add one with voro doc add <project> <path>".into());
            return false;
        }
        docs.sort_by_key(|doc| (doc.project_id != task.project_id, doc.id));
        self.mode = Mode::DocPicker {
            task_id,
            docs,
            sel: 0,
            back,
        };
        true
    }

    /// Drive the document picker: ⏎ links or unlinks the highlighted document
    /// through the same `voro-core` calls `doc link`/`doc unlink` make, and the
    /// picker stays open on the refreshed list so several can be toggled in one
    /// visit. Esc returns to the detail popup it was opened from, if any.
    pub(super) fn key_doc_picker(
        &mut self,
        key: KeyEvent,
        task_id: i64,
        docs: Vec<voro_core::Doc>,
        mut sel: usize,
        back: Option<u16>,
    ) {
        match key.code {
            KeyCode::Esc => {
                if let Some(scroll) = back {
                    self.mode = Mode::Detail { task_id, scroll };
                }
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(docs.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => self.toggle_doc_link(task_id, &docs[sel]),
            _ => {}
        }
        self.mode = Mode::DocPicker {
            task_id,
            docs,
            sel,
            back,
        };
    }

    pub(super) fn key_pick_project(&mut self, key: KeyEvent, mut sel: usize, flow: CreateFlow) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(self.creatable_projects().len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(project_id) = self.creatable_projects().get(sel).map(|p| p.id) {
                    self.start_create(project_id, flow);
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::PickProject { sel, flow };
    }

    pub(super) fn key_transition(
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
                    Action::RejectWork(_) => Some(PromptKind::RejectWork),
                    _ => None,
                };
                let milestone = self
                    .all
                    .iter()
                    .any(|r| r.task.id == task_id && r.task.milestone);
                match kind {
                    Some(kind) => {
                        let buffer = self.prompt_seed(task_id, kind);
                        self.mode = Mode::Prompt {
                            task_id,
                            kind,
                            buffer,
                        };
                    }
                    None if milestone && matches!(action, Action::Complete(_)) => {
                        self.close_milestone(task_id)
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

    pub(super) fn key_prompt(
        &mut self,
        key: KeyEvent,
        task_id: i64,
        kind: PromptKind,
        mut buffer: String,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                match kind.action(buffer.clone()) {
                    Some(action) => self.apply_and_refresh(task_id, action),
                    None => match kind {
                        PromptKind::SessionMessage => self.send_session_message(task_id, &buffer),
                        // RefineNote, the only other launch-feeding kind.
                        _ => self.refine_with_note(task_id, &buffer),
                    },
                }
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

    pub(super) fn key_detail(&mut self, key: KeyEvent, task_id: i64, mut scroll: u16) {
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
                    let actions = transition_actions(task);
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
            KeyCode::Char('!') => self.toggle_deep(task_id),
            // The picker takes over the screen, so hand it this popup's scroll
            // to restore; when nothing opens it, fall through and stay put.
            KeyCode::Char('c') => {
                if self.open_doc_picker(task_id, Some(scroll)) {
                    return;
                }
            }
            KeyCode::Char('m') => {
                if self.open_milestone_picker(task_id, Some(scroll)) {
                    return;
                }
            }
            // The popup only opens on the selected task, so the selection-based
            // helper pages the right log.
            KeyCode::Char('l') => self.view_session_log(),
            _ => {}
        }
        self.mode = Mode::Detail { task_id, scroll };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Screen;
    use crate::app::tests::{alt_key, app_with, key, scratch_env};
    use voro_core::{NewTask, Store, TaskState};

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
                assert_eq!(*actions, Store::legal_actions(TaskState::Ready, false));
            }
            _ => panic!("enter in the detail view should open the transition menu"),
        }

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
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
        alt_key(&mut app, KeyCode::Char('1'));
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

    /// `D` opens the picker listing every agent from `voro.toml`, with the
    /// one plain dispatch would resolve to marked regardless of cursor
    /// position; picking a different one dispatches with that override.
    #[test]
    fn agent_picker_lists_agents_resolved_marked_and_dispatches_the_choice() {
        // `sleep 1 &&` keeps the stub alive past the dispatch's own refresh,
        // for the same reconcile-on-read race noted above.
        let (mut store, ctx, project_path) = scratch_env(
            "picker",
            Some(
                "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"sleep 1 && cat {prompt_file}\"\n\n\
                 [agents.special]\ncmd = \"sleep 1 && cat {prompt_file}\"\n",
            ),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
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
                // the built-in claude/codex layer in alongside the user agents
                assert_eq!(
                    agents,
                    &vec![
                        "claude".to_string(),
                        "codex".to_string(),
                        "special".to_string(),
                        "stub".to_string(),
                    ]
                );
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

    /// An invalid `voro.toml` is only discovered when the picker is
    /// opened — it is loaded fresh each time, never cached — and surfaces
    /// through the ordinary status-line error style instead of a stale or
    /// empty modal. (A *missing* file is not a failure: the built-ins
    /// load, so the picker opens on them.)
    #[test]
    fn agent_picker_reports_a_config_load_failure_without_opening() {
        // an agent whose dispatch drops the {prompt_file} placeholder fails
        // validation, so the whole config fails to load
        let (mut store, ctx, project_path) = scratch_env(
            "picker-invalid",
            Some("[agents.bad]\ncmd = \"run with no placeholder\"\n"),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
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
                .contains("only ready or stalled tasks can be dispatched"),
            "{:?}",
            app.status
        );
    }

    /// `c` opens the picker over every registered document and ⏎ toggles the
    /// highlighted one, so a link and its removal both happen without leaving
    /// the TUI (DESIGN.md §8).
    #[test]
    fn doc_picker_toggles_the_selected_task_s_links_in_place() {
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.all[0].task.id;
        let project_id = app.projects[0].id;
        let plan = app
            .store
            .create_doc(project_id, None, "docs/plan.md", Some("The Plan"))
            .unwrap();
        app.store
            .create_doc(project_id, None, "docs/rfc.md", None)
            .unwrap();
        app.refresh().unwrap();

        key(&mut app, KeyCode::Char('c'));
        let docs = match &app.mode {
            Mode::DocPicker {
                task_id: id,
                docs,
                sel,
                back,
            } => {
                assert_eq!(*id, task_id);
                assert_eq!(*sel, 0);
                assert_eq!(*back, None);
                docs.clone()
            }
            _ => panic!("c should open the document picker"),
        };
        assert_eq!(docs.len(), 2);
        assert!(!app.doc_linked(task_id, plan.id));

        // ⏎ links the highlighted document and the picker stays open on it,
        // now marked, so a second ⏎ takes the link away again.
        key(&mut app, KeyCode::Enter);
        assert!(app.doc_linked(task_id, plan.id));
        assert!(matches!(app.mode, Mode::DocPicker { sel: 0, .. }));
        assert_eq!(
            app.store.docs_for_task(task_id).unwrap(),
            vec![plan.clone()]
        );

        key(&mut app, KeyCode::Enter);
        assert!(!app.doc_linked(task_id, plan.id));
        assert!(app.store.docs_for_task(task_id).unwrap().is_empty());

        // Esc from a picker opened off the cockpit lands back on the cockpit.
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// The case the picker exists for: citing a plan while triaging a proposal.
    /// Proposals ride the queue folded into a per-project digest (DESIGN.md §7)
    /// which names no task of its own, so `c` reaches one only once the digest
    /// is expanded and the cursor has moved onto the proposal beneath it — and
    /// on the digest row itself the key correctly does nothing.
    #[test]
    fn doc_picker_reaches_a_proposal_under_an_expanded_digest() {
        let mut app = app_with(&[TaskState::Proposed]);
        let task_id = app.all[0].task.id;
        let project_id = app.projects[0].id;
        let plan = app
            .store
            .create_doc(project_id, None, "docs/plan.md", Some("The Plan"))
            .unwrap();
        app.refresh().unwrap();

        assert_eq!(app.selected_task_id(), None, "the digest names no task");
        key(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::Normal));

        key(&mut app, KeyCode::Enter);
        app.move_selection(1);
        assert_eq!(app.selected_task_id(), Some(task_id));

        key(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::DocPicker { .. }));
        key(&mut app, KeyCode::Enter);
        assert!(app.doc_linked(task_id, plan.id));
    }

    /// A task may cite a plan owned by any project (DESIGN.md §3), so the
    /// picker spans them all — with the task's own project's documents first,
    /// where a triage most often reaches.
    #[test]
    fn doc_picker_lists_every_project_s_documents_own_first() {
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.all[0].task.id;
        let own = app.projects[0].id;
        let other = app.store.create_project("other", "/tmp/other").unwrap().id;
        // registered first, so id order alone would put it at the top
        let strategy = app
            .store
            .create_doc(other, None, "docs/strategy.md", Some("Strategy"))
            .unwrap();
        let plan = app
            .store
            .create_doc(own, None, "docs/plan.md", Some("The Plan"))
            .unwrap();
        app.refresh().unwrap();

        key(&mut app, KeyCode::Char('c'));
        match &app.mode {
            Mode::DocPicker { docs, .. } => {
                assert_eq!(
                    docs.iter().map(|d| d.id).collect::<Vec<_>>(),
                    vec![plan.id, strategy.id]
                );
            }
            _ => panic!("c should open the document picker"),
        }

        // and the out-of-project document links just like an own one
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Enter);
        assert!(app.doc_linked(task_id, strategy.id));
    }

    /// With nothing registered there is nothing to pick, so the picker says so
    /// on the status line — pointing at the CLI verb that registers one, which
    /// the TUI deliberately does not — rather than opening empty.
    #[test]
    fn doc_picker_with_no_documents_reports_instead_of_opening() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status.as_deref().unwrap_or("").contains("voro doc add"),
            "{:?}",
            app.status
        );
    }

    /// Opened from the task browser's detail popup, the picker returns to it on
    /// esc with its scroll intact — the reading position survives the detour.
    #[test]
    fn doc_picker_opened_from_the_detail_popup_returns_to_it() {
        let mut app = app_with(&[TaskState::Proposed]);
        let task_id = app.all[0].task.id;
        let project_id = app.projects[0].id;
        app.store
            .create_doc(project_id, None, "docs/plan.md", None)
            .unwrap();
        app.refresh().unwrap();

        alt_key(&mut app, KeyCode::Char('2'));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('j'));
        assert!(matches!(app.mode, Mode::Detail { scroll: 1, .. }));

        key(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::DocPicker { back: Some(1), .. }));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        assert!(matches!(
            app.mode,
            Mode::Detail {
                task_id: id,
                scroll: 1
            } if id == task_id
        ));
    }
}
