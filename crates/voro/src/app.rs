use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::ui::Hit;
use voro_core::{
    Action, ActionRow, AgentsConfig, CompletionReport, DepKind, DepRef, DigestRow, Event, PrRef,
    Priority, Project, Queue, QueueRow, RefineOutcome, RunningRow, ScoreBreakdown, StateCounts,
    Store, Task, TaskState, Triage, WipGate, scheduler,
};

/// Lines `PgDn`/`PgUp` move the focus card in one press. A fixed step, since
/// the key handler runs without the pane's geometry.
const DETAIL_PAGE_STEP: i64 = 10;

/// What an operator with no project registered is told, wherever they meet the
/// fact: the empty cockpit's box and `n`'s refusal say the same thing in the
/// same words, and in the keys README.md teaches.
pub const NO_PROJECTS_HINT: &str = "no projects yet — press tab to Projects, then a to add one";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Cockpit,
    Tasks,
    Projects,
    Config,
}

/// Which global default a [`Mode::DefaultPicker`] is setting (DESIGN.md §5):
/// `default_agent` or `default_viewer`, both pick-from-list on the Config screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultKind {
    Agent,
    Viewer,
}

/// One option in the project's viewer picker (DESIGN.md §8/§11a). Beyond the
/// viewers themselves — `None` for the config default, then each named one —
/// the trailing `NewViewer` entry opens the add-viewer form and pins the
/// project to the viewer it creates, first-time viewer setup without a detour
/// through the Config screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerOption {
    Viewer(Option<String>),
    NewViewer,
}

/// How a project's viewer choice reads on screen and at the shell: the name it
/// pins, or the config default when it names none (DESIGN.md §8).
pub fn viewer_label(viewer: Option<&str>) -> &str {
    viewer.unwrap_or("default viewer")
}

/// An agent row on the Config screen (DESIGN.md §5): the effective set with
/// provenance and the default marked, read-only in this cut.
#[derive(Debug, Clone)]
pub struct ConfigAgentRow {
    pub name: String,
    pub dispatch: String,
    pub provenance: &'static str,
    pub is_default: bool,
    pub verbs: Vec<&'static str>,
    pub missing_verbs: Vec<&'static str>,
    /// What `{model}` resolves to for this agent as `(ordinary, deep, plan)`,
    /// with the fallbacks already applied; `None` when it names no model.
    pub models: Option<(String, String, String)>,
}

/// A named viewer row on the Config screen: every viewer `open` can run, the
/// built-ins included, so the starred default is always visible. Only the rows
/// backed by a `voro.toml` table are `editable` — a built-in is overridden
/// rather than changed in place (DESIGN.md §11a).
#[derive(Debug, Clone)]
pub struct ConfigViewerRow {
    pub name: String,
    pub cmd: String,
    pub is_default: bool,
    pub provenance: &'static str,
    pub editable: bool,
}

/// One selectable row on the cockpit; indices point into the App caches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitRow {
    /// One row of the scheduler's queue, by index.
    Queue(usize),
    /// A proposal inside an expanded digest row (DESIGN.md §7): the digest's
    /// queue index, then the proposal's index within it.
    Proposal(usize, usize),
    Running(usize),
}

#[derive(Debug, Clone)]
pub struct TaskRow {
    pub task: Task,
    pub project: String,
    pub weight: i64,
    /// The task's `blocks` dependencies with each blocker's state, so the
    /// browser can show a parked row what it is waiting on. Filtered from the
    /// same [`Store::deps_by_task`] load that feeds the detail panes.
    pub blockers: Vec<DepRef>,
}

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

/// The add/edit-viewer form's fields, which always travel together: the two
/// values being edited, which field the cursor is on, whether this is an edit
/// (which locks the name), the project to pin on success, and whether the
/// command is still following the name.
#[derive(Clone)]
pub struct ViewerFormState {
    pub name: String,
    pub cmd: String,
    pub on_cmd: bool,
    pub editing: bool,
    pub review_project: Option<i64>,
    /// Whether the command is still *following* the name — rewritten to
    /// `<name> {path}` on every keystroke in the name field, so the
    /// operator watches the line they are about to save assemble itself
    /// (DESIGN.md §5). Writing in the command field decouples it, and
    /// emptying that field couples it again, which is the whole undo:
    /// nothing they typed is ever overwritten, and nothing they did not
    /// type is ever kept against their will. Always false on an edit,
    /// where the command already exists and is theirs.
    pub cmd_tracks_name: bool,
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
    /// a peek rather than a screen — any key dismisses it — and it carries no
    /// state of its own, since the screen it describes is the App's.
    KeyMap,
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
            | Mode::ViewerPicker { sel, .. }
            | Mode::DefaultPicker { sel, .. } => Some(sel),
            _ => None,
        }
    }
}

/// Which create flow the project picker feeds (DESIGN.md §8/§9): the manual
/// `$EDITOR` form on `n`, or an interactive agent planning session on `N` —
/// the same lowercase-default, uppercase-variant pairing as `d`/`D`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateFlow {
    Editor,
    Plan,
}

/// Which refine intensity a keypress asks for (DESIGN.md §6): a one-line note
/// feeding a headless rewrite on `r`, or the interactive planning session on
/// `R` — the same lowercase-default, uppercase-variant pairing as `n`/`N`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineFlow {
    Note,
    Interactive,
}

/// A request for main() to suspend the terminal and run $EDITOR.
#[derive(Debug, Clone, Copy)]
pub enum EditorRequest {
    Create { project_id: i64 },
    Edit { task_id: i64 },
}

/// A request for main() to suspend the terminal and run an agent's
/// `attach`/`resume` command in the foreground (task #75) — a full-screen
/// interactive program that owns the terminal until the user detaches.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    /// The verb template with `{session}` already substituted.
    pub command: String,
    /// The project checkout to run it in.
    pub cwd: String,
}

/// What the quick-message key needs resolved before it can send: the session
/// the line lands in, the template that puts it there, and the listing the
/// liveness probe reads to be sure the session is between turns.
struct MessageTarget {
    /// The session row the send updates once it is confirmed — its process, and
    /// its reference where the agent's verb forks (DESIGN.md §8).
    session_id: i64,
    session_ref: String,
    template: String,
    sessions_cmd: Option<String>,
}

/// The two ways into an agent's own session (DESIGN.md §8): join one still
/// running, or reopen one that has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpVerb {
    Attach,
    Resume,
}

/// Which verb a task's state implies — the fallback for when the agent cannot
/// say whether its session is still live. `None` for a state with no session
/// worth jumping into, which is what gates the key. `waiting` keeps its session
/// open exactly as `review` does (DESIGN.md §8), so it jumps in on the same
/// terms — the strip is where the operator now meets it.
fn state_jump_verb(state: TaskState) -> Option<JumpVerb> {
    match state {
        TaskState::Running => Some(JumpVerb::Attach),
        TaskState::Review | TaskState::Waiting | TaskState::Stalled => Some(JumpVerb::Resume),
        _ => None,
    }
}

/// Which states accept a quick message into the task's session (DESIGN.md §8) —
/// the states whose session is open and between turns, so a headless resume
/// lands as the next thing the agent reads. `running` and `refining` are refused
/// because the session is mid-turn with no injection channel, and `stalled`
/// because its session is dead: a headless resume there would restart the work
/// with no tracked pid and no session row, invisible to the reconciler.
/// Redispatch is the honest path for that, and `A` the one for the rest.
fn state_accepts_message(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::NeedsInput | TaskState::Review | TaskState::Waiting
    )
}

/// The template to jump in with: liveness decides the verb wherever the agent
/// can report it, and the task state stands in only when it cannot. The two
/// come apart in both directions — a `claude --bg` session outlives the
/// `running` state and refuses `--resume` while it does, and a session that
/// died before its task left `running` has nothing left to attach to.
///
/// The choice then falls back to whichever verb the agent actually defines —
/// the built-in `codex` defines only `resume` — so a one-verb agent jumps in
/// with that verb rather than erroring; `None` only when it defines neither.
fn jump_verb<'a>(
    live: Option<bool>,
    by_state: JumpVerb,
    attach: Option<&'a str>,
    resume: Option<&'a str>,
) -> Option<&'a str> {
    let want = match live {
        Some(true) => JumpVerb::Attach,
        Some(false) => JumpVerb::Resume,
        None => by_state,
    };
    match want {
        JumpVerb::Attach => attach.or(resume),
        JumpVerb::Resume => resume.or(attach),
    }
}

pub fn action_label(action: &Action) -> &'static str {
    match action {
        Action::Triage(Triage::Parked) => "triage → parked",
        Action::Triage(Triage::Ready) => "triage → ready",
        Action::Triage(Triage::Reject) => "triage → rejected",
        Action::Refine(_) => "refine → refining",
        Action::ConcludeRefine(_) => "cancel the refine → proposed",
        Action::Start => "start → running",
        Action::Ask(_) => "ask a question → needs-input",
        Action::Resume => "resume (answered in-session) → running",
        Action::Complete(_) => "complete → review",
        Action::HandOff => "hand off → waiting",
        Action::Reclaim => "reclaim → review",
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
    /// The dispatch context the TUI's dispatch/redispatch, planning, and
    /// attach/resume actions use — the same one the CLI verbs use.
    dispatch_ctx: crate::dispatch::DispatchCtx,
    pub screen: Screen,
    pub should_quit: bool,
    pub status: Option<String>,

    pub projects: Vec<Project>,
    /// The next-action queue (DESIGN.md §7), ranked by attention price and
    /// carrying the dispatch gate's state when it is suppressing rows.
    pub queue: Queue,
    /// The attention price band the queue was last ranked with (DESIGN.md §7),
    /// so the score decomposition can show the same division the order used.
    pub costs: voro_core::AttentionCosts,
    /// Which projects' proposal digests are expanded, so their constituent
    /// rows are selectable for triage. Keyed by project name and held across
    /// refreshes, so triaging one proposal does not collapse the rest.
    pub expanded_digests: std::collections::HashSet<String>,
    /// The cockpit's running strip (DESIGN.md §9): one row per `running`,
    /// `refining`, or `waiting` task with its open session if any, so a task
    /// started by hand is still visible. Filtered on task state, so
    /// `review`/`needs-input` tasks stay in the queue.
    pub running: Vec<RunningRow>,
    /// Task counts by state (DESIGN.md §12), rendered as the persistent header
    /// indicator so the backlogs stay felt even when a low-scoring row falls
    /// past the queue's uniform cap (§7).
    pub counts: StateCounts,
    pub all: Vec<TaskRow>,
    /// Review tasks carrying a branch and no summary (DESIGN.md §8): the
    /// half-written done report a dispatched session left behind, which a PR
    /// cannot be opened from. Re-derived per refresh, never stored.
    pub incomplete_report: std::collections::HashSet<i64>,
    /// Review tasks whose checkout has no git remote (DESIGN.md §8): there is
    /// nowhere to open a pull request, so their rows advertise the local review
    /// path instead of a `pr` that could only fail. Re-derived per refresh from
    /// the checkouts themselves, one `git remote` per distinct repo.
    pub local_review: std::collections::HashSet<i64>,
    /// Proposals whose last refine round rewrote the body (DESIGN.md §6): what
    /// renders the `↻ refined` marker, so the operator triages the improved
    /// version knowing it moved. Re-derived per refresh and cleared by triage
    /// itself, since the flag is gated on `proposed`.
    pub refined: std::collections::HashSet<i64>,
    /// Proposals whose last refine round died without rewriting anything: the
    /// `⚠ refine failed` marker, same lifecycle as `refined`. A failed round has
    /// to look different from a proposal nobody refined — the operator should
    /// never have to notice an absence.
    pub refine_failed: std::collections::HashSet<i64>,
    /// Every dependency edge, both directions, keyed by task id — what the
    /// detail views render as `blocked by #N` / `blocks #N` (task #103).
    /// Loaded whole per refresh so the render path never queries the store.
    pub deps: std::collections::HashMap<i64, Vec<DepRef>>,
    pub dependents: std::collections::HashMap<i64, Vec<DepRef>>,
    /// The plan documents each task derives from (DESIGN.md §3), keyed by task
    /// id and loaded whole per refresh like the dependency maps. Read-only in
    /// the TUI: registering and linking documents is a CLI affair.
    pub docs: std::collections::HashMap<i64, Vec<voro_core::Doc>>,
    /// Where each document resolves to, keyed by doc id — a relative location
    /// joined onto its checkout. Resolved once per refresh beside `docs`, so
    /// the render path can show the real location without querying the store.
    pub doc_locations: std::collections::HashMap<i64, String>,
    /// Each task's newest session (tasks #73/#110), keyed by task id: what the
    /// detail views render — a stalled task's post-mortem (DESIGN.md §8), an
    /// open session's agent and log — and what gates the `l` log key. Loaded per
    /// refresh like the dependency maps, so the render path never queries the store.
    pub last_sessions: std::collections::HashMap<i64, voro_core::Session>,
    /// The stale-branch probe's verdict for the *currently selected* task
    /// (DESIGN.md §8): its id and whether its tracked PR reports a merge
    /// conflict. Filled when a background probe returns — one `gh` call, started
    /// once the selection has rested on a review task with a PR — and cleared
    /// the moment the selection moves, so re-selecting the row probes afresh.
    /// Never a per-row sweep: the queue stays unannotated and the network is
    /// touched at most once per settled selection. `None` while a probe is in
    /// flight, which renders as no marker — a missing signal is never a conflict.
    pub conflict_selected: Option<(i64, bool)>,
    /// The background thread and debounce clock behind `conflict_selected`.
    probe: crate::probe::ConflictProbe,
    /// The background threads capturing the revisions rejections were made
    /// against (DESIGN.md §8), drained by `poll_reviewed_capture`.
    capture: crate::probe::ReviewedCapture,

    pub cockpit_rows: Vec<CockpitRow>,
    pub cockpit_sel: usize,
    pub tasks_sel: usize,
    pub projects_sel: usize,

    /// The Config screen's view of `voro.toml` (DESIGN.md §5), reloaded every
    /// refresh so an edit — from either this screen or a dispatch — is reflected
    /// immediately. Agents are read-only; the viewers are what `config_sel`
    /// selects, and the ones a `voro.toml` table backs are what edit/delete act
    /// on — a built-in row is selectable but refuses both.
    pub config_agents: Vec<ConfigAgentRow>,
    pub config_viewers: Vec<ConfigViewerRow>,
    /// The legacy anonymous `[viewer]` table's command, shown read-only.
    pub config_anon_viewer: Option<String>,
    /// A `voro.toml` that failed to parse, surfaced on the screen rather than
    /// silently rendering an empty config.
    pub config_error: Option<String>,
    pub config_sel: usize,

    pub mode: Mode,
    /// Whether the detail views fold the score decomposition (DESIGN.md §7) and
    /// the event history in — toggled by `x` and `h`. Held per app-state so the
    /// choice persists as the selection moves, shared by the cockpit pane and
    /// the tasks-screen Detail popup.
    pub show_score: bool,
    pub show_history: bool,
    /// Vertical scroll offset of the cockpit focus card (DESIGN.md §9), driven
    /// by `J`/`K` and `PgDn`/`PgUp`. Reset to the top when the selection moves,
    /// since the pane follows the selection.
    pub detail_scroll: u16,
    /// The largest useful `detail_scroll` for the pane as last rendered — the
    /// key handler has no geometry of its own, so `draw_detail` records the
    /// overflow here for `scroll_detail` to clamp against.
    pub detail_max_scroll: std::cell::Cell<u16>,
    pub pending_editor: Option<EditorRequest>,
    pub pending_attach: Option<AttachRequest>,
    /// A planning session waiting for main() to suspend the terminal and run
    /// it (DESIGN.md §8) — the same round-trip as `pending_attach`, kept
    /// separate so main() can label its log breadcrumbs and refresh message.
    pub pending_plan: Option<crate::dispatch::PlanLaunch>,

    /// Last `PRAGMA data_version` seen, used to detect commits from other
    /// processes and refresh without reacting to our own mutations.
    last_data_version: i64,
}

/// Browser grouping: attention states first, closed last.
fn browse_order(state: TaskState) -> u8 {
    match state {
        TaskState::Proposed => 0,
        TaskState::Refining => 1,
        TaskState::NeedsInput => 2,
        TaskState::Review => 3,
        TaskState::Stalled => 4,
        TaskState::Ready => 5,
        TaskState::Running => 6,
        TaskState::Waiting => 7,
        TaskState::Parked => 8,
        TaskState::Done => 9,
        TaskState::Rejected => 10,
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
            queue: Queue {
                rows: Vec::new(),
                at_capacity: None,
            },
            costs: voro_core::AttentionCosts::default(),
            expanded_digests: std::collections::HashSet::new(),
            running: Vec::new(),
            counts: StateCounts::default(),
            all: Vec::new(),
            incomplete_report: std::collections::HashSet::new(),
            local_review: std::collections::HashSet::new(),
            refined: std::collections::HashSet::new(),
            refine_failed: std::collections::HashSet::new(),
            deps: std::collections::HashMap::new(),
            dependents: std::collections::HashMap::new(),
            docs: std::collections::HashMap::new(),
            doc_locations: std::collections::HashMap::new(),
            last_sessions: std::collections::HashMap::new(),
            conflict_selected: None,
            probe: crate::probe::ConflictProbe::default(),
            capture: crate::probe::ReviewedCapture::default(),
            cockpit_rows: Vec::new(),
            cockpit_sel: 0,
            tasks_sel: 0,
            projects_sel: 0,
            config_agents: Vec::new(),
            config_viewers: Vec::new(),
            config_anon_viewer: None,
            config_error: None,
            config_sel: 0,
            mode: Mode::Normal,
            show_score: false,
            show_history: false,
            detail_scroll: 0,
            detail_max_scroll: std::cell::Cell::new(0),
            pending_editor: None,
            pending_attach: None,
            pending_plan: None,
            last_data_version: 0,
        };
        app.refresh()?;
        // A database with nothing registered opens where the first step is
        // (DESIGN.md §9): the cockpit has nothing to show and its `n` cannot
        // proceed without a project. Startup only — `refresh` runs after every
        // mutation and on every external-change poll, and the screen is the
        // operator's after that.
        if app.projects.is_empty() {
            app.screen = Screen::Projects;
            app.status = Some(
                "welcome to voro — press a to add your first project, then n to create a task"
                    .into(),
            );
        }
        app.last_data_version = app.store.data_version()?;
        Ok(app)
    }

    /// Where main() records the outcome of an attach/resume round-trip — the
    /// same rolling launch log a viewer open writes to (DESIGN.md §11a), so a
    /// failing attach leaves a breadcrumb the TUI cannot paint over.
    pub fn launch_log_path(&self) -> std::path::PathBuf {
        self.dispatch_ctx.launch_log_path()
    }

    /// The `voro.toml` the Config screen views and edits (DESIGN.md §5), for
    /// the screen to show the operator which file is in play.
    pub fn config_path(&self) -> &std::path::Path {
        &self.dispatch_ctx.agents_path
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

        self.deps = self.store.deps_by_task()?;
        self.dependents = self.store.dependents_by_task()?;
        self.docs = self.store.docs_by_task()?;
        self.doc_locations = self
            .store
            .all_docs()?
            .iter()
            .filter_map(|doc| Some((doc.id, self.store.resolve_doc(doc).ok()?)))
            .collect();

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
                let blockers = self
                    .deps
                    .get(&task.id)
                    .map(|deps| {
                        deps.iter()
                            .filter(|d| d.kind == DepKind::Blocks)
                            .cloned()
                            .collect()
                    })
                    .unwrap_or_default();
                TaskRow {
                    task,
                    project,
                    weight,
                    blockers,
                }
            })
            .collect();
        all.sort_by_key(|r| (browse_order(r.task.state), r.task.id));
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
        let mut forges = crate::pr::ForgeMemo::default();
        self.local_review = all
            .iter()
            .filter(|r| r.task.state == TaskState::Review && r.task.pr_url.is_none())
            .filter_map(|r| {
                let repo = self.store.repo_for_task(&r.task).ok()?;
                (!forges.takes_pull_requests(&repo.path)).then_some(r.task.id)
            })
            .collect();
        let proposals = || all.iter().filter(|r| r.task.state == TaskState::Proposed);
        self.refined = proposals()
            .filter_map(|r| {
                self.store
                    .refined_flag(r.task.id)
                    .ok()?
                    .then_some(r.task.id)
            })
            .collect();
        self.refine_failed = proposals()
            .filter_map(|r| {
                self.store
                    .refine_failed_flag(r.task.id)
                    .ok()?
                    .then_some(r.task.id)
            })
            .collect();
        self.last_sessions = self.store.latest_sessions()?;
        self.all = all;
        self.running = self.store.running_rows()?;
        self.counts = self.store.state_counts()?;

        // The queue is priced by what each row asks of the operator and gated
        // on how much is already in flight (DESIGN.md §7). A `voro.toml` that
        // will not parse falls back to the defaults here rather than emptying
        // the cockpit — the Config screen is where the error is surfaced.
        let config = AgentsConfig::load(&self.dispatch_ctx.agents_path);
        let costs = config.as_ref().map(AgentsConfig::costs).unwrap_or_default();
        let gate = WipGate {
            running: self.counts.running,
            max_running: config
                .as_ref()
                .map_or(scheduler::DEFAULT_MAX_RUNNING, |c| c.max_running()),
        };
        self.costs = costs;
        self.queue = scheduler::queue(&candidates, &costs, gate);

        self.cockpit_rows = self.build_cockpit_rows();

        self.load_config_view(config);

        self.cockpit_sel = self
            .cockpit_sel
            .min(self.cockpit_rows.len().saturating_sub(1));
        self.tasks_sel = self.tasks_sel.min(self.all.len().saturating_sub(1));
        self.projects_sel = self.projects_sel.min(self.projects.len().saturating_sub(1));
        self.config_sel = self
            .config_sel
            .min(self.config_viewers.len().saturating_sub(1));
        Ok(())
    }

    /// Flatten the queue into selectable rows: every queue row, with an
    /// expanded digest's proposals listed beneath it, then the running strip.
    fn build_cockpit_rows(&self) -> Vec<CockpitRow> {
        let mut rows = Vec::new();
        for (i, row) in self.queue.rows.iter().enumerate() {
            rows.push(CockpitRow::Queue(i));
            if let QueueRow::Digest(digest) = row
                && self.expanded_digests.contains(&digest.project_name)
            {
                rows.extend((0..digest.tasks.len()).map(|j| CockpitRow::Proposal(i, j)));
            }
        }
        rows.extend((0..self.running.len()).map(CockpitRow::Running));
        rows
    }

    /// What a task's raw score becomes once priced by its next action
    /// (DESIGN.md §7) — what the queue actually ranked it by.
    pub fn effective_score(&self, task: &Task, total: f64) -> Option<voro_core::EffectiveScore> {
        scheduler::effective_score(task, total, &self.costs)
    }

    /// The queue's task rows in order, by id — digests contribute nothing,
    /// since they name a backlog rather than a task.
    #[cfg(test)]
    pub fn queue_task_ids(&self) -> Vec<i64> {
        self.queue
            .rows
            .iter()
            .filter_map(|row| match row {
                QueueRow::Action(row) => Some(row.candidate.task.id),
                QueueRow::Digest(_) => None,
            })
            .collect()
    }

    /// The digest a queue row holds, if it is one.
    pub fn digest(&self, queue_index: usize) -> Option<&DigestRow> {
        match self.queue.rows.get(queue_index)? {
            QueueRow::Digest(digest) => Some(digest),
            QueueRow::Action(_) => None,
        }
    }

    /// One proposal inside a digest row.
    pub fn digest_child(&self, queue_index: usize, child: usize) -> Option<&ActionRow> {
        self.digest(queue_index)?.tasks.get(child)
    }

    /// Fold a digest row open or shut, so its proposals become selectable for
    /// triage (DESIGN.md §7). Rebuilds the row list in place; the selection
    /// stays on the digest, which is where the operator pressed Enter.
    fn toggle_digest(&mut self, queue_index: usize) {
        let Some(digest) = self.digest(queue_index) else {
            return;
        };
        let project = digest.project_name.clone();
        if !self.expanded_digests.remove(&project) {
            self.expanded_digests.insert(project);
        }
        self.cockpit_rows = self.build_cockpit_rows();
        self.cockpit_sel = self
            .cockpit_sel
            .min(self.cockpit_rows.len().saturating_sub(1));
    }

    /// Reload the Config screen's `voro.toml` view (DESIGN.md §5). A parse
    /// failure is held in `config_error` and shown on the screen; the agent and
    /// dispatch paths load the file independently, so this only feeds rendering.
    fn load_config_view(&mut self, config: voro_core::Result<AgentsConfig>) {
        let config = match config {
            Ok(config) => config,
            Err(e) => {
                self.config_agents.clear();
                self.config_viewers.clear();
                self.config_anon_viewer = None;
                self.config_error = Some(e.to_string());
                return;
            }
        };
        let default_agent = config.default_name();
        self.config_agents = config
            .entries()
            .map(|(name, template, provenance)| {
                let verbs = [
                    ("sessions", template.sessions()),
                    ("attach", template.attach()),
                    ("resume", template.resume()),
                    ("plan", template.plan()),
                ]
                .into_iter()
                .filter_map(|(verb, defined)| defined.map(|_| verb))
                .collect();
                ConfigAgentRow {
                    name: name.to_string(),
                    dispatch: template.dispatch().to_string(),
                    provenance: provenance.label(),
                    is_default: Some(name) == default_agent.as_deref(),
                    verbs,
                    missing_verbs: config.override_missing_verbs(name),
                    models: template.model().map(|model| {
                        (
                            model.to_string(),
                            template.model_deep().unwrap_or(model).to_string(),
                            template.model_plan().unwrap_or(model).to_string(),
                        )
                    }),
                }
            })
            .collect();
        let default_viewer = config.default_viewer_name();
        self.config_viewers = config
            .viewer_entries()
            .into_iter()
            .map(|(name, cmd, provenance)| ConfigViewerRow {
                is_default: Some(name) == default_viewer.as_deref(),
                name: name.to_string(),
                cmd: cmd.to_string(),
                provenance: provenance.label(),
                editable: provenance != voro_core::Provenance::BuiltIn,
            })
            .collect();
        self.config_anon_viewer = config.anonymous_viewer_cmd().map(str::to_string);
        self.config_error = None;
    }

    pub fn selected_task_id(&self) -> Option<i64> {
        match self.screen {
            Screen::Cockpit => match self.cockpit_rows.get(self.cockpit_sel)? {
                // A digest names no single task; its children do.
                CockpitRow::Queue(i) => match self.queue.rows.get(*i)? {
                    QueueRow::Action(row) => Some(row.candidate.task.id),
                    QueueRow::Digest(_) => None,
                },
                CockpitRow::Proposal(i, j) => Some(self.digest_child(*i, *j)?.candidate.task.id),
                CockpitRow::Running(i) => Some(self.running.get(*i)?.task_id),
            },
            Screen::Tasks => Some(self.all.get(self.tasks_sel)?.task.id),
            Screen::Projects | Screen::Config => None,
        }
    }

    /// The selected task's tracked PR URL, when it is a `review` task carrying
    /// one — the only selection there is anything to probe for (DESIGN.md §8).
    /// Read from the refreshed rows rather than the store, so the tick costs no
    /// query.
    fn review_pr_url(&self, id: i64) -> Option<String> {
        self.all
            .iter()
            .find(|r| r.task.id == id)
            .filter(|r| r.task.state == TaskState::Review)
            .and_then(|r| r.task.pr_url.clone())
    }

    /// Advance the stale-branch probe one tick (DESIGN.md §8): collect a
    /// finished verdict and start a new probe when one is due. Both halves are
    /// non-blocking — the `gh` call itself runs on a background thread — so the
    /// event loop never stalls on the network, and the probe only starts once
    /// the selection has rested (`probe::SETTLE`), so scrolling through a queue
    /// of review tasks spawns nothing for the rows passed over. A verdict that
    /// arrives after the selection has moved on is discarded rather than shown
    /// against the wrong task.
    pub fn poll_conflict_probe(&mut self) {
        let selected = self.selected_task_id();
        // The verdict belongs to the row it was taken for; moving off it means
        // there is nothing to show, and coming back re-probes for a fresh one.
        if self
            .conflict_selected
            .is_some_and(|(id, _)| Some(id) != selected)
        {
            self.conflict_selected = None;
        }
        if let Some((id, verdict)) = self.probe.take_result()
            && Some(id) == selected
        {
            self.conflict_selected = Some((id, verdict.conflicts()));
        }

        let target = selected.and_then(|id| self.review_pr_url(id).map(|url| (id, url)));
        let inputs = crate::probe::ProbeInputs {
            target: target.as_ref().map(|(id, _)| *id),
            cached: self.conflict_selected.map(|(id, _)| id),
            in_flight: self.probe.in_flight(),
            rested: self.probe.settle(selected, std::time::Instant::now()),
        };
        if let Some((id, url)) = target
            && crate::probe::probe_due(inputs)
        {
            self.probe.start(id, url);
        }
    }

    /// Capture the revision a rejection was made against, off the event loop
    /// (DESIGN.md §8). The keypress gains nothing by waiting for `gh`: the
    /// value is read only when the rework comes back for re-review, minutes or
    /// hours later, and rejecting has just moved the task to `running`, where
    /// neither read path consults it. A task with neither a PR nor a branch has
    /// nothing to read, so it starts nothing.
    fn capture_reviewed(&mut self, task_id: i64) {
        // The head is read a moment after the keypress rather than at it, so a
        // rework commit pushed inside that window would be captured as reviewed
        // and left out of the delta. `voro reject` on the CLI stays synchronous
        // for anyone who wants the tight capture.
        if let Some(source) = crate::pr::reviewed_source(&self.store, task_id) {
            self.capture.start(task_id, source);
        }
    }

    /// Record every revision a background capture has finished (DESIGN.md §8).
    /// Each belongs to the task it was captured for, not to the selection, so
    /// nothing is discarded here. Errors are swallowed as the synchronous path
    /// swallows them: an unrecorded revision costs a full diff on the
    /// re-review, never a failed reject. Quitting before a capture lands loses
    /// it the same way, which is why no refresh follows either — nothing
    /// rendered reads the reviewed revision, which `pr` and `open` read on
    /// demand.
    pub fn poll_reviewed_capture(&mut self) {
        for (task_id, sha) in self.capture.take_results() {
            let _ = self.store.record_reviewed(task_id, &sha);
        }
    }

    pub fn move_selection(&mut self, delta: i64) {
        let (sel, len) = match self.screen {
            Screen::Cockpit => (&mut self.cockpit_sel, self.cockpit_rows.len()),
            Screen::Tasks => (&mut self.tasks_sel, self.all.len()),
            Screen::Projects => (&mut self.projects_sel, self.projects.len()),
            Screen::Config => (&mut self.config_sel, self.config_viewers.len()),
        };
        if len == 0 {
            return;
        }
        *sel = (*sel as i64 + delta).clamp(0, len as i64 - 1) as usize;
        // The focus card follows the selection, so start each new body at the top.
        self.detail_scroll = 0;
    }

    /// Put the selection on `index` of the current screen's list, the way a
    /// click does. Out-of-range indices are ignored rather than clamped: a stale
    /// hit-map naming a row the last refresh dropped should move nothing.
    fn select_index(&mut self, index: usize) {
        let (sel, len) = match self.screen {
            Screen::Cockpit => (&mut self.cockpit_sel, self.cockpit_rows.len()),
            Screen::Tasks => (&mut self.tasks_sel, self.all.len()),
            Screen::Projects => (&mut self.projects_sel, self.projects.len()),
            Screen::Config => (&mut self.config_sel, self.config_viewers.len()),
        };
        if index >= len {
            return;
        }
        *sel = index;
        self.detail_scroll = 0;
    }

    /// Scroll the cockpit focus card, clamped to the overflow `draw_detail`
    /// last measured so `K` past the top or `J` past the bottom simply stops.
    fn scroll_detail(&mut self, delta: i64) {
        let max = self.detail_max_scroll.get() as i64;
        self.detail_scroll = (self.detail_scroll as i64 + delta).clamp(0, max) as u16;
    }

    /// Tab cycles cockpit → tasks → projects → config → cockpit; `1`/`2`/`3`/`4`
    /// jump directly (DESIGN.md §9).
    pub fn toggle_screen(&mut self) {
        self.screen = match self.screen {
            Screen::Cockpit => Screen::Tasks,
            Screen::Tasks => Screen::Projects,
            Screen::Projects => Screen::Config,
            Screen::Config => Screen::Cockpit,
        };
    }

    /// The primary action of the current selection. On the Tasks screen every
    /// row opens its detail view. On the cockpit — where the detail pane
    /// already shows the body — a needs-input task resumes directly (the
    /// operator has answered the question in the agent's own session, DESIGN.md
    /// §6/§8) and any other task opens its transition menu.
    fn activate_selection(&mut self) {
        if self.screen == Screen::Tasks {
            if let Some(task_id) = self.selected_task_id() {
                self.mode = Mode::Detail { task_id, scroll: 0 };
            }
            return;
        }
        if let Some(CockpitRow::Queue(i)) = self.cockpit_rows.get(self.cockpit_sel)
            && self.digest(*i).is_some()
        {
            self.toggle_digest(*i);
            return;
        }
        if let Some(task) = self.selected_task() {
            if task.state == TaskState::NeedsInput {
                let id = task.id;
                self.apply_and_refresh(id, Action::Resume);
            } else {
                let actions = Store::legal_actions(task.state, task.human);
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
            Screen::Config => self
                .config_viewers
                .get(self.config_sel)
                .filter(|v| v.editable)
                .map(|_| "⏎ edit"),
            Screen::Tasks => self.all.get(self.tasks_sel).map(|_| "⏎ view"),
            Screen::Cockpit => match self.cockpit_rows.get(self.cockpit_sel)? {
                CockpitRow::Queue(i) => match self.queue.rows.get(*i)? {
                    QueueRow::Digest(digest) => {
                        if self.expanded_digests.contains(&digest.project_name) {
                            Some("⏎ collapse")
                        } else {
                            Some("⏎ expand")
                        }
                    }
                    QueueRow::Action(row) => match row.candidate.task.state {
                        TaskState::NeedsInput => Some("⏎ resume"),
                        TaskState::Review => Some("⏎ review"),
                        _ => Some("⏎ act"),
                    },
                },
                CockpitRow::Proposal(..) => Some("⏎ triage"),
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

    /// Apply a transition and refresh. `resume` (needs-input → running) and
    /// reject-with-feedback (review → running) both leave the agent's session
    /// open, so the operator answers the question or addresses the feedback in
    /// that same session — Voro only moves the state (DESIGN.md §6/§8).
    fn apply_and_refresh(&mut self, task_id: i64, action: Action) {
        let rejected = matches!(action, Action::RejectWork(_));
        let result = self.store.apply(task_id, action);
        if self.report(result).is_some() {
            if rejected {
                // The head the operator just judged, so the re-review can be
                // narrowed to the rework (DESIGN.md §8) — captured off the loop,
                // so the redraw does not wait on `gh`.
                self.capture_reviewed(task_id);
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
            Mode::PickProject { sel, flow } => self.key_pick_project(key, sel, flow),
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
            Mode::DocPicker {
                task_id,
                docs,
                sel,
                back,
            } => self.key_doc_picker(key, task_id, docs, sel, back),
            Mode::ViewerPicker {
                project_id,
                options,
                current,
                sel,
            } => self.key_viewer_picker(key, project_id, options, current, sel),
            Mode::ViewerForm(form) => self.key_viewer_form(key, form),
            // The key map is dismissed by any key, and `on_key` has already
            // restored `Mode::Normal`, so there is nothing left to do.
            Mode::KeyMap => {}
            Mode::DefaultPicker {
                kind,
                names,
                current,
                sel,
            } => self.key_default_picker(key, kind, names, current, sel),
        }
    }

    /// Route a left click at `(col, row)` through the hit-map the last draw
    /// built (DESIGN.md §9). A click is a selection move and nothing more — it
    /// never fires the row's action — except inside a picker, where a click on
    /// the option already under the cursor confirms it as ⏎ would. Clicks
    /// anywhere the map does not cover do nothing.
    pub fn on_mouse(&mut self, col: u16, row: u16, hits: &crate::ui::HitMap) {
        self.status = None;
        let Some(hit) = hits.at(col, row) else {
            return;
        };
        match hit {
            Hit::CockpitRow(i) if self.screen == Screen::Cockpit => self.select_index(i),
            Hit::TaskRow(i) if self.screen == Screen::Tasks => self.select_index(i),
            Hit::ProjectRow(i) if self.screen == Screen::Projects => self.select_index(i),
            Hit::ViewerRow(i) if self.screen == Screen::Config => self.select_index(i),
            Hit::PickerOption(i) => self.click_picker_option(i),
            _ => {}
        }
    }

    /// A click on a picker option: move the cursor there, or — if it is already
    /// there — hand the picker the Enter its key handler answers, so clicking
    /// confirms through exactly the path the keyboard takes.
    fn click_picker_option(&mut self, index: usize) {
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

    fn key_normal(&mut self, key: KeyEvent) {
        // Navigation shared by every screen: quit, the key map, tab cycling,
        // and moving the selection. `?` belongs here rather than in the
        // trailing match, which the projects and Config screens never reach.
        match key.code {
            KeyCode::Char('q') => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('?') => {
                self.mode = Mode::KeyMap;
                return;
            }
            KeyCode::Tab => {
                self.toggle_screen();
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
        // The projects screen's keys (DESIGN.md §9) reinterpret the digit keys
        // as weights, so its handler gets first refusal before the global
        // `1`/`2`/`3` screen jump below.
        if self.screen == Screen::Projects {
            self.key_projects(key);
            return;
        }
        // The Config screen has its own letter actions that would collide with
        // the global ones (`a`, `d`, `e`), so it too intercepts before the match
        // below; it keeps the digit jumps, which mean nothing else there.
        if self.screen == Screen::Config {
            self.key_config(key);
            return;
        }
        // Direct screen jumps, on the screens where digits mean nothing else.
        match key.code {
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
            KeyCode::Char('4') => {
                self.screen = Screen::Config;
                return;
            }
            _ => {}
        }
        match key.code {
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let result = self.refresh();
                self.report(result);
            }
            KeyCode::Char('r') => self.refine_selected(RefineFlow::Note),
            KeyCode::Char('R') => self.refine_selected(RefineFlow::Interactive),
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Char('n') => self.new_task(CreateFlow::Editor),
            KeyCode::Char('N') => self.new_task(CreateFlow::Plan),
            KeyCode::Char('e') => {
                if let Some(id) = self.selected_task_id() {
                    self.pending_editor = Some(EditorRequest::Edit { task_id: id });
                }
            }
            KeyCode::Char('s') => {
                if let Some(task) = self.selected_task() {
                    let actions = Store::legal_actions(task.state, task.human);
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
            // On the cockpit `x`/`h` fold score/history into the detail pane;
            // the tasks-screen equivalents are local to the popup (`key_detail`).
            KeyCode::Char('x') if self.screen == Screen::Cockpit => {
                self.show_score = !self.show_score;
            }
            KeyCode::Char('h') if self.screen == Screen::Cockpit => {
                self.show_history = !self.show_history;
            }
            // Scroll the focus card body (task #2): `j`/`k` already move the row
            // selection, so shifted `J`/`K` and the page keys drive the pane.
            KeyCode::Char('J') if self.screen == Screen::Cockpit => self.scroll_detail(1),
            KeyCode::Char('K') if self.screen == Screen::Cockpit => self.scroll_detail(-1),
            KeyCode::PageDown if self.screen == Screen::Cockpit => {
                self.scroll_detail(DETAIL_PAGE_STEP)
            }
            KeyCode::PageUp if self.screen == Screen::Cockpit => {
                self.scroll_detail(-DETAIL_PAGE_STEP)
            }
            KeyCode::Char('d') => {
                if let Some((task_id, _)) = self.dispatchable_selected_task() {
                    self.dispatch_task(task_id, None);
                }
            }
            KeyCode::Char('D') => {
                if let Some((task_id, agent)) = self.dispatchable_selected_task() {
                    self.open_agent_picker(task_id, agent);
                }
            }
            KeyCode::Char('!') => {
                if let Some(id) = self.selected_task_id() {
                    self.toggle_deep(id);
                }
            }
            KeyCode::Char('c') => {
                if let Some(id) = self.selected_task_id() {
                    self.open_doc_picker(id, None);
                }
            }
            // `C` is not a variant of `c` — cancelling a refine and linking a
            // document merely share a letter, so they keep their own slots
            // (DESIGN.md §9).
            KeyCode::Char('C') => self.cancel_refine_selected(),
            KeyCode::Char('o') => self.open_selected_in_viewer(),
            KeyCode::Char('g') => self.open_selected_pr(),
            // `a`/`A` are the quick and interactive halves of one action, the
            // same pairing as `r`/`R` (DESIGN.md §9).
            KeyCode::Char('a') => self.message_session(),
            KeyCode::Char('A') => self.jump_into_session(),
            KeyCode::Char('l') => self.view_session_log(),
            KeyCode::Char('w') => self.hand_off_selected(),
            _ => {}
        }
    }

    /// Toggle the selected task's deep flag (DESIGN.md §8): `!` moves it
    /// between the agent's workhorse model and its strongest one. Routes
    /// through `voro-core` so the change is logged, and reports the store's
    /// refusal — of a human task — on the status line.
    fn toggle_deep(&mut self, task_id: i64) {
        let Ok(task) = self.store.task(task_id) else {
            return;
        };
        let to = !task.deep;
        let result = self
            .store
            .set_deep(task_id, to)
            .and_then(|_| self.refresh());
        if self.report(result).is_some() {
            self.status = Some(if to {
                format!("task {task_id} is deep — dispatches on the agent's strongest model")
            } else {
                format!("task {task_id} is no longer deep — dispatches on the workhorse")
            });
        }
    }

    /// Hand a review task off to an external party (DESIGN.md §6): `w` on a
    /// review row moves it `review → waiting`, out of the queue until it is the
    /// operator's move again. A non-review selection reports why via the status
    /// line, the same no-op-with-explanation style as the other action keys.
    fn hand_off_selected(&mut self) {
        let Some(task) = self.selected_task() else {
            return;
        };
        if task.state != TaskState::Review {
            self.status = Some(format!(
                "task is {} — hand off works on a review task",
                task.state
            ));
            return;
        }
        self.apply_and_refresh(task.id, Action::HandOff);
    }

    /// Page through the selected task's newest session log (tasks #73/#110), in
    /// any state that has a session on record. `$PAGER` (default `less`) owns
    /// the terminal, so this runs through `pending_attach` with the TUI torn
    /// down around it, like attach/resume. Missing pieces report via the status
    /// line.
    fn view_session_log(&mut self) {
        let Some(task_id) = self.selected_task().map(|t| t.id) else {
            return;
        };
        let Some(session) = self.last_sessions.get(&task_id) else {
            self.status = Some(format!("task {task_id} has no session on record"));
            return;
        };
        let Some(log_path) = session.log_path.clone() else {
            self.status = Some(format!("session {} recorded no log path", session.id));
            return;
        };
        let cwd = match self.task_checkout(task_id) {
            Ok(path) => path,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        self.pending_attach = Some(AttachRequest {
            command: format!(
                "${{PAGER:-less}} {}",
                crate::dispatch::shell_quote(std::path::Path::new(&log_path))
            ),
            cwd,
        });
    }

    /// The checkout a task's work lives in (DESIGN.md §8): its resolved repo's
    /// path. Every TUI action that needs a working directory for a task —
    /// paging its log, attaching to its session, asking whether it can take a
    /// pull request — comes here rather than reading a project path.
    fn task_checkout(&self, task_id: i64) -> voro_core::Result<String> {
        let task = self.store.task(task_id)?;
        Ok(self.store.repo_for_task(&task)?.path)
    }

    /// The repo a task names, as (name, path), or `None` when it runs in its
    /// project's default — the detail pane renders the line only when it says
    /// something the project row does not.
    /// The verb a task's row advertises (DESIGN.md §3), degraded to the local
    /// review path where the checkout has no remote to open a pull request on
    /// (§8). Every rendered `next:` resolves through here so the advertisement
    /// and the key that serves it cannot drift apart.
    pub fn next_action(&self, task: &voro_core::Task) -> Option<voro_core::NextAction> {
        let verb = task.next_action()?;
        Some(match self.local_review.contains(&task.id) {
            true => verb.without_pull_requests(),
            false => verb,
        })
    }

    pub fn task_repo(&self, task: &voro_core::Task) -> Option<(String, String)> {
        task.repo_id?;
        let repo = self.store.repo_for_task(task).ok()?;
        Some((repo.name, repo.path))
    }

    /// A project's default repo path, for the projects screen's path column
    /// and its rename/re-path form. An unreadable repo yields `""` so a render
    /// never surfaces an error mid-frame.
    pub fn project_path(&self, project_id: i64) -> String {
        self.store
            .default_repo(project_id)
            .map(|r| r.path)
            .unwrap_or_default()
    }

    /// How many repos a project has, for the projects screen's `+N repos` tag.
    pub fn repo_count(&self, project_id: i64) -> usize {
        self.store.repos(project_id).map(|r| r.len()).unwrap_or(1)
    }

    /// Whether the selection is a review task, so it can be handed off with
    /// `w` — what gates that key's key-line hint.
    pub fn selected_can_hand_off(&self) -> bool {
        self.selected_task()
            .is_some_and(|t| t.state == TaskState::Review)
    }

    /// Whether the selection has work to look at locally — what gates the `o`
    /// hint (DESIGN.md §9), and the same pair of states the key itself allows.
    pub fn selected_has_a_diff(&self) -> bool {
        self.selected_task()
            .is_some_and(|t| matches!(t.state, TaskState::Review | TaskState::Running))
    }

    /// Whether the selection is the task `g` has a PR to show or create — what
    /// gates that hint (DESIGN.md §9). The key stays bound in every state, for
    /// jumping to a tracked PR and linking one; only the advertisement is
    /// narrowed to the moment the PR is the review.
    pub fn selected_is_in_review(&self) -> bool {
        self.selected_task()
            .is_some_and(|t| t.state == TaskState::Review)
    }

    /// Whether the selection still has a dispatch ahead of it, so the model
    /// `!` picks would be used — what gates that hint (DESIGN.md §9). Deep is a
    /// property of the *next* launch, so on a task whose work is done — under
    /// review, handed off, or closed — the line stops offering a toggle that
    /// changes nothing the operator is about to see. The key itself stays bound
    /// in every state, as `g` does; only the advertisement is narrowed, which is
    /// what makes room for the review keys beside it.
    pub fn selected_can_go_deep(&self) -> bool {
        self.selected_task().is_some_and(|t| {
            !matches!(
                t.state,
                TaskState::Review | TaskState::Waiting | TaskState::Done | TaskState::Rejected
            )
        })
    }

    /// Whether the selection is somewhere dispatch can act from (DESIGN.md §8)
    /// — what gates the `d/D` hint, so the line stops advertising a dispatch
    /// that would only answer with the state it refuses.
    pub fn selected_can_dispatch(&self) -> bool {
        self.selected_task()
            .is_some_and(|t| matches!(t.state, TaskState::Ready | TaskState::Stalled))
    }

    /// Whether the selection has a session that could take a quick message —
    /// what gates the `a/A` hint. The state gate plus a session on record; the
    /// captured ref and the agent's `message` verb are left to the key itself,
    /// since a jump-in (`A`) is worth advertising either way.
    pub fn selected_can_message(&self) -> bool {
        self.selected_task().is_some_and(|t| {
            state_accepts_message(t.state) && self.last_sessions.contains_key(&t.id)
        })
    }

    /// Whether the selection is a refine in flight, so `C` can cancel it — what
    /// gates that key's key-line hint.
    pub fn selected_is_refining(&self) -> bool {
        self.selected_task()
            .is_some_and(|t| t.state == TaskState::Refining)
    }

    /// Begin creating a task in one of the two flows (DESIGN.md §9): straight
    /// into it when there is exactly one project, via the project picker when
    /// there are several, and a pointer to the projects screen when there are
    /// none.
    fn new_task(&mut self, flow: CreateFlow) {
        match self.projects.len() {
            0 => self.status = Some(NO_PROJECTS_HINT.into()),
            1 => self.start_create(self.projects[0].id, flow),
            _ => self.mode = Mode::PickProject { sel: 0, flow },
        }
    }

    /// Launch the chosen create flow on a project: queue the `$EDITOR` form,
    /// or assemble a planning session (DESIGN.md §8) for main() to run in the
    /// foreground. An agent without a `plan` verb — or any other assembly
    /// failure — reports what to configure through the status line, the same
    /// "no-op with an explanation" style as the dispatch keys.
    fn start_create(&mut self, project_id: i64, flow: CreateFlow) {
        match flow {
            CreateFlow::Editor => {
                self.pending_editor = Some(EditorRequest::Create { project_id });
            }
            CreateFlow::Plan => {
                match crate::dispatch::plan_session(
                    &self.store,
                    &self.dispatch_ctx,
                    crate::dispatch::PlanTarget::Create { project_id },
                ) {
                    Ok(launch) => self.pending_plan = Some(launch),
                    Err(e) => self.status = Some(e),
                }
            }
        }
    }

    /// Whether a task's body is still a brief rather than work under way — what
    /// gates the refine keys. A `ready` task qualifies as much as a `proposed`
    /// one: its verdict was issued against the body, so rewriting the body sends
    /// it back through triage (DESIGN.md §6).
    pub fn is_refinable(&self, task_id: i64) -> bool {
        self.all.iter().any(|r| {
            r.task.id == task_id && matches!(r.task.state, TaskState::Proposed | TaskState::Ready)
        })
    }

    /// Refine the selected task (DESIGN.md §6). Refine is an event on a brief
    /// rather than a verdict on one, so it answers from the queue — where the
    /// operator reads the body and notices it is sub-standard — and not only
    /// from behind the triage menu, which collects verdicts. A selection whose
    /// body is no longer a brief awaiting work reports why via the status line,
    /// the same no-op-with-explanation style as the other action keys.
    fn refine_selected(&mut self, flow: RefineFlow) {
        let Some(task) = self.selected_task() else {
            return;
        };
        let (task_id, state) = (task.id, task.state);
        if state == TaskState::Refining {
            self.status = Some(format!(
                "task {task_id} is already being refined — C cancels the round"
            ));
            return;
        }
        if !matches!(state, TaskState::Proposed | TaskState::Ready) {
            self.status = Some(format!(
                "task is {state} — refine works on a proposal or a ready task"
            ));
            return;
        }
        match flow {
            RefineFlow::Note => {
                self.mode = Mode::Prompt {
                    task_id,
                    kind: PromptKind::RefineNote,
                    buffer: String::new(),
                }
            }
            RefineFlow::Interactive => self.refine_interactively(task_id),
        }
    }

    /// Note-driven refine (DESIGN.md §6): hand the body, the note, and the
    /// discovered-from context to a headless agent that rewrites the body in
    /// place. The task leaves the queue for `refining` while the round runs and
    /// comes back `proposed` for a verdict on the improved version.
    fn refine_with_note(&mut self, task_id: i64, note: &str) {
        match crate::dispatch::refine(&mut self.store, &self.dispatch_ctx, task_id, note) {
            Ok(summary) => {
                self.status = Some(summary);
                let result = self.refresh();
                self.report(result);
            }
            Err(e) => self.status = Some(e),
        }
    }

    /// Open an interactive refine round once its child has a pid (DESIGN.md
    /// §6): `proposed → refining` plus the session row, in one write. A refusal
    /// — the operator triaged the proposal from another window between assembly
    /// and spawn — reports on the status line and leaves the session running,
    /// since pulling the terminal out from under a conversation the operator is
    /// already in would be the worse failure.
    pub fn open_refine_round(&mut self, refine: &crate::dispatch::RefineLaunch, pid: i64) {
        if let Err(e) =
            self.store
                .record_refine_launch(refine.task_id, "", &refine.agent, Some(pid), None)
        {
            self.status = Some(format!("refine of task {} unrecorded: {e}", refine.task_id));
        }
    }

    /// Close an interactive refine round when its session returns. The agent's
    /// own `voro set --body-file` concludes the round as it applies the rewrite,
    /// so a task that has already left `refining` needs nothing here; one still
    /// in it means the operator quit without concluding, which is a no-op rather
    /// than a failure (DESIGN.md §6) — `cancelled`, no marker.
    pub fn close_refine_round(&mut self, task_id: i64) {
        let Ok(task) = self.store.task(task_id) else {
            return;
        };
        if task.state != TaskState::Refining {
            return;
        }
        if let Err(e) = self
            .store
            .conclude_refine(task_id, RefineOutcome::Cancelled)
        {
            self.status = Some(e.to_string());
        }
    }

    /// Cancel a refine round in flight (DESIGN.md §6): kill the agent, close its
    /// session `aborted`, and return the task to `proposed` unmarked. This is
    /// the escape hatch for an agent that is *hung* — still alive, so reconcile
    /// will never catch it — which is why it kills the process rather than only
    /// moving the state. A selection that is not refining reports why, the same
    /// no-op-with-explanation style as the other action keys.
    fn cancel_refine_selected(&mut self) {
        let Some(task) = self.selected_task() else {
            return;
        };
        let (task_id, state) = (task.id, task.state);
        if state != TaskState::Refining {
            self.status = Some(format!(
                "task is {state} — cancel works on a refine in flight"
            ));
            return;
        }
        let killed = self.kill_open_session(task_id);
        let result = self
            .store
            .conclude_refine(task_id, RefineOutcome::Cancelled)
            .and_then(|_| self.refresh());
        if self.report(result).is_some() {
            self.status = Some(format!("refine of task {task_id} cancelled{killed}"));
        }
    }

    /// Kill the process *group* of a task's open session, best-effort,
    /// returning what to say about it. A headless expansion is spawned into its
    /// own group (pgid = pid), so the negated pid reaches the agent under the
    /// launching shell rather than only the shell — which is the case this key
    /// exists for, a detached round nobody is watching. Deliberately no
    /// plain-pid fallback: a recorded pid outlives its process and can be
    /// recycled, and signalling a stranger is worse than the round the operator
    /// can end by quitting it. An interactive round's child is in voro's own
    /// group (it must be, to own the terminal) and so names no group here; that
    /// round ends by the operator leaving the session, which concludes it.
    fn kill_open_session(&self, task_id: i64) -> String {
        let Some(pid) = self
            .store
            .sessions_for(task_id)
            .ok()
            .and_then(|sessions| sessions.into_iter().find(|s| s.ended_at.is_none()))
            .and_then(|session| session.pid)
        else {
            return String::new();
        };
        let killed = std::process::Command::new("kill")
            .args(["-TERM", "--"])
            .arg(format!("-{pid}"))
            .status()
            .is_ok_and(|status| status.success());
        if killed {
            format!(" — agent (pid {pid}) killed")
        } else {
            format!(" — agent (pid {pid}) could not be killed")
        }
    }

    /// Interactive refine (DESIGN.md §6): the planning harness pointed at a
    /// task that already exists, so the operator talks the body into shape and
    /// the agent applies it with `set --body-file`. Same foreground round-trip
    /// as `N`, and the same "no-op with an explanation" failure style.
    fn refine_interactively(&mut self, task_id: i64) {
        match crate::dispatch::plan_session(
            &self.store,
            &self.dispatch_ctx,
            crate::dispatch::PlanTarget::Refine { task_id },
        ) {
            Ok(launch) => self.pending_plan = Some(launch),
            Err(e) => self.status = Some(e),
        }
    }

    /// Jump into the selected task's agent session (task #75). Which verb that
    /// takes is decided by the session itself where the agent can say: a live
    /// session is `attach`ed to, a finished one `resume`d — task state only
    /// standing in when liveness is unknowable. The two do not follow from each
    /// other, since a `claude --bg` session commonly outlives the `running`
    /// state (DESIGN.md §8's stale-review rebase attaches to a `review` task's
    /// session) and `--resume` refuses a session still held by the supervisor.
    /// The run happens in main() via `pending_attach`, with the TUI torn down
    /// around it. Every missing piece (state, session, captured ref, verb)
    /// reports via the status line.
    fn jump_into_session(&mut self) {
        let (task_id, state) = match self.selected_task() {
            Some(task) => (task.id, task.state),
            None => return,
        };
        let Some(by_state) = state_jump_verb(state) else {
            self.status = Some(format!(
                "task is {state} — jump-in works on running, review, waiting, or \
                 stalled tasks"
            ));
            return;
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
                match by_state {
                    JumpVerb::Attach => "attach to",
                    JumpVerb::Resume => "resume",
                }
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
        let agent = config.agent(&session.agent);
        // A synchronous probe: the TUI is about to hand the terminal to a
        // full-screen session anyway, so one listing costs nothing felt.
        let live = crate::session_probe::session_is_live(
            agent.and_then(|a| a.sessions()),
            Some(&session_ref),
        );
        let template = jump_verb(
            live,
            by_state,
            agent.and_then(|a| a.attach()),
            agent.and_then(|a| a.resume()),
        );
        let Some(template) = template else {
            self.status = Some(format!(
                "agent '{}' defines no attach or resume template in {}",
                session.agent,
                self.dispatch_ctx.agents_path.display()
            ));
            return;
        };
        let cwd = match self.task_checkout(task_id) {
            Ok(path) => path,
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
            cwd,
        });
    }

    /// Quick-message the selected task's agent session (DESIGN.md §8): `a`
    /// collects one line and fires it into the session headlessly, so steering
    /// an agent costs a sentence rather than the attach round-trip `A` still
    /// performs. The state gate and the session's own pieces are checked here,
    /// before the input opens, so a refusal costs no typing.
    fn message_session(&mut self) {
        let (task_id, state) = match self.selected_task() {
            Some(task) => (task.id, task.state),
            None => return,
        };
        if !state_accepts_message(state) {
            self.status = Some(format!(
                "task is {state} — a message lands on a needs-input, review, or \
                 waiting task; A jumps into the session instead"
            ));
            return;
        }
        if self.message_target(task_id).is_none() {
            return;
        }
        self.mode = Mode::Prompt {
            task_id,
            kind: PromptKind::SessionMessage,
            buffer: String::new(),
        };
    }

    /// Resolve what a quick message needs, reporting whichever piece is missing
    /// on the status line exactly as `jump_into_session` does. Config is loaded
    /// fresh, so an agent that gained a `message` verb since the TUI started
    /// gains the key too.
    fn message_target(&mut self, task_id: i64) -> Option<MessageTarget> {
        let sessions = match self.store.sessions_for(task_id) {
            Ok(sessions) => sessions,
            Err(e) => {
                self.status = Some(e.to_string());
                return None;
            }
        };
        let Some(session) = sessions.first() else {
            self.status = Some(format!("task {task_id} has no recorded session to message"));
            return None;
        };
        let Some(session_ref) = session.session_ref.clone() else {
            self.status = Some(format!(
                "no session reference was captured for session {} — nothing to message",
                session.id
            ));
            return None;
        };
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return None;
            }
        };
        let agent = config.agent(&session.agent);
        let Some(template) = agent.and_then(|a| a.message()) else {
            self.status = Some(format!(
                "agent '{}' defines no message template in {} — A jumps into the session instead",
                session.agent,
                self.dispatch_ctx.agents_path.display()
            ));
            return None;
        };
        Some(MessageTarget {
            session_id: session.id,
            session_ref,
            template: template.to_string(),
            sessions_cmd: agent.and_then(|a| a.sessions()).map(str::to_string),
        })
    }

    /// Send the collected line into the task's session (DESIGN.md §8). A
    /// review or waiting task's message *is* its rejection, and the send goes
    /// first: a message that never left would otherwise leave the feedback in
    /// the body, the task back in `running`, and the agent none the wiser —
    /// which is exactly the state a redispatch cannot tell from a stall. So the
    /// spawn is confirmed, then the session row and the transition commit
    /// together, and a refused send leaves the task precisely where it was. A
    /// `needs-input` task transitions not at all — the answer lives in the
    /// transcript and the agent's own `voro resume` moves it back to `running`
    /// (DESIGN.md §6).
    fn send_session_message(&mut self, task_id: i64, message: &str) {
        if message.trim().is_empty() {
            self.status = Some("a message is required".into());
            return;
        }
        let state = match self.store.task(task_id) {
            Ok(task) => task.state,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        if !state_accepts_message(state) {
            self.status = Some(format!("task is now {state} — nothing was sent"));
            return;
        }
        let Some(target) = self.message_target(task_id) else {
            return;
        };
        // A live session is mid-turn, so a headless resume would either be
        // refused or land out of order; the operator wants the real terminal.
        if crate::session_probe::session_is_live(
            target.sessions_cmd.as_deref(),
            Some(&target.session_ref),
        ) == Some(true)
        {
            self.status = Some(format!(
                "task {task_id}'s session is still running — A attaches to it"
            ));
            return;
        }
        let cwd = match self.task_checkout(task_id) {
            Ok(path) => path,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let rejected = matches!(state, TaskState::Review | TaskState::Waiting);
        // A rejection reaches the session framed as one: the feedback, plus the
        // instruction to answer it point by point at `done` (DESIGN.md §8). An
        // ordinary message is said as written.
        let framed = rejected
            .then(|| crate::dispatch::rework_message(task_id, &self.dispatch_ctx.db_path, message));
        let sent = crate::dispatch::send_message(
            &self.dispatch_ctx,
            crate::dispatch::SessionMessage {
                task_id,
                template: &target.template,
                session_ref: &target.session_ref,
                message: framed.as_deref().unwrap_or(message),
                cwd,
            },
        );
        let sent = match sent {
            Ok(sent) => sent,
            Err(e) => {
                self.status = Some(format!("{e} — task {task_id} is unchanged"));
                return;
            }
        };
        // The send is under way, so the session row follows it — the process
        // now carrying the turn, and the reference the agent forked into where
        // its verb does that — and the rejection commits behind it. A store
        // failure here takes the agent down with it rather than leaving it
        // working on feedback no state records.
        let pid = sent.pid();
        if let Err(e) =
            self.store
                .record_session_send(target.session_id, sent.new_session_ref(), pid)
        {
            sent.abandon();
            self.status = Some(format!(
                "recording the send failed ({e}); the spawned agent (pid {pid}) was killed"
            ));
            return;
        }
        if rejected {
            if let Err(e) = self
                .store
                .apply(task_id, Action::RejectWork(message.to_string()))
            {
                sent.abandon();
                self.status = Some(format!("{e}; the spawned agent (pid {pid}) was killed"));
                return;
            }
            // What the operator just judged, so the re-review can be narrowed to
            // the rework (DESIGN.md §8) — off the loop, so nothing waits on `gh`.
            self.capture_reviewed(task_id);
        }
        let summary = sent.confirm(&self.dispatch_ctx);
        self.status = Some(if rejected {
            format!("{summary} — task returned to running")
        } else {
            summary
        });
        let refreshed = self.refresh();
        self.report(refreshed);
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

    /// What the selected task last reported (DESIGN.md §8): the completion
    /// summary of the cycle in hand, and the rejection feedback it answers if
    /// it is a rework. `None` for a task that has reported nothing.
    pub fn completion_report(&self, task_id: i64) -> Option<CompletionReport> {
        voro_core::completion_report(&self.store.events_for(task_id).ok()?)
    }

    /// The selected task's id and agent override, if it is `ready` or `stalled`
    /// — dispatch's own precondition (DESIGN.md §8). Any other state sets a
    /// status message and returns `None` rather than silently doing nothing.
    fn dispatchable_selected_task(&mut self) -> Option<(i64, Option<String>)> {
        let (id, state, agent) = {
            let task = self.selected_task()?;
            (task.id, task.state, task.agent.clone())
        };
        if !matches!(state, TaskState::Ready | TaskState::Stalled) {
            self.status = Some(format!(
                "task is {state} — only ready or stalled tasks can be dispatched"
            ));
            return None;
        }
        Some((id, agent))
    }

    /// Dispatch-with-resolved-agent, or the picker's chosen override — both
    /// dispatch actions (DESIGN.md §8/§9) land here. Dispatch errors (dirty
    /// tree, unknown agent, missing config) surface through `self.status`.
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

    /// Open the selected task's checkout in a configured viewer (DESIGN.md
    /// §11a): the explicit viewer key, reaching the local diff even on a GitHub
    /// project. Only `review`/`running` tasks have a diff worth opening; anything
    /// else reports via the status line.
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
        let result = crate::dispatch::open(&mut self.store, &self.dispatch_ctx, id, None);
        self.report_open(result);
    }

    /// What `o` does with what opening returned. No viewer set up at all is
    /// *answered* rather than reported: the add-viewer form opens on the spot
    /// (DESIGN.md §5), because the operator pressing `o` is one name and
    /// command away from what they asked for, and sending them to the Config
    /// screen to type the same two fields is a detour. Saving does not then
    /// open the task — `o` again does — so the key never does two things at
    /// once. Every other failure only reports, as before.
    ///
    /// Split from the keypress because the branch cannot otherwise be tested:
    /// which arm runs depends on the developer's PATH, and a test that got it
    /// wrong would launch a real editor.
    fn report_open(&mut self, result: Result<String, crate::dispatch::OpenFailure>) {
        match result {
            Ok(summary) => self.status = Some(summary),
            Err(crate::dispatch::OpenFailure::NoViewer(_)) => {
                self.status = Some(format!(
                    "no viewer set up — name one here to open this task (no built-in {} on PATH)",
                    voro_core::BUILTIN_VIEWER_NAMES.join("/")
                ));
                self.open_viewer_form(None, None);
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// The GitHub key (DESIGN.md §8) — statically the PR medium, whatever the
    /// project's review action says. With a tracked PR, jump to it in a browser
    /// (§11c). With none, a `review` task opens the create-PR confirmation
    /// modal, and any other state falls back to the link-an-existing-PR prompt.
    /// A checkout GitHub cannot take refuses before the modal, naming `o` — the
    /// local diff is the only thing left to look at.
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
        // The network-free preconditions first, so a task missing a branch or
        // summary names that gap without a `gh` round-trip; then the medium,
        // which has to answer before the modal rather than after it.
        let planned = crate::pr::plan(&self.store, id)
            .and_then(|plan| self.pr_checkout_is_github(id).map(|()| plan));
        match planned {
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

    /// Whether the task's checkout can take a pull request at all, named in the
    /// TUI's idiom so the refusal points at `o` (DESIGN.md §8).
    fn pr_checkout_is_github(&self, task_id: i64) -> Result<(), String> {
        let checkout = self.task_checkout(task_id).map_err(|e| e.to_string())?;
        crate::pr::ensure_github_repo(&checkout, "`o`")
    }

    /// Drive the create-PR confirmation modal (DESIGN.md §8). Enter (or `y`)
    /// runs the same `crate::pr::create` the CLI's `pr` calls and shows the new
    /// PR, then refreshes; esc (or `n`) cancels without touching anything.
    fn key_confirm_pr(&mut self, key: KeyEvent, task_id: i64, branch: String, title: String) {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let created = crate::pr::create(&mut self.store, task_id, "`o`");
                self.report_created_pr(task_id, created, crate::pr::open_url);
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

    /// Report a create-PR attempt, chaining a success straight into the browser
    /// (DESIGN.md §8): creating a PR is all but always followed by looking at
    /// it, so `g` does both. The create is the durable half — its URL is already
    /// recorded on the task — so a browser that will not launch is reported
    /// beside the URL rather than as a failed create.
    fn report_created_pr(
        &mut self,
        task_id: i64,
        created: Result<String, String>,
        open: impl FnOnce(&str) -> Result<String, String>,
    ) {
        self.status = Some(match created {
            Ok(url) => match open(&url) {
                Ok(_) => format!("opened {url} for task {task_id} — showing it in the browser"),
                Err(e) => format!("PR created ({url}); could not open browser: {e}"),
            },
            Err(e) => e,
        });
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

    /// Validate and track a PR reference on a task, then refresh. An unparseable
    /// reference keeps the prompt open with the typed text intact and the parse
    /// error on the status line, so a typo can be fixed without retyping.
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
    /// §11c), still editable before submitting. Everything else — and a PR with
    /// no pullable comments, or a `gh` failure — starts empty, reason on the
    /// status line.
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

    /// Open the agent picker (DESIGN.md §8): agents are loaded from `voro.toml`
    /// now, not cached, so a config changed since the last dispatch — the
    /// usage-cap case this exists for — is reflected. A load failure reports via
    /// the status line rather than opening an empty or stale modal.
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

    /// Open the document picker on a task (DESIGN.md §8): every registered
    /// document, since a task in any project may cite any plan (§3), with the
    /// task's own project's listed first — the ones a triage is most likely to
    /// reach for. Read fresh from the store rather than from the refresh cache,
    /// which only holds documents something already links to. Returns whether it
    /// opened, so a caller with a screen to restore knows to give way.
    fn open_doc_picker(&mut self, task_id: i64, back: Option<u16>) -> bool {
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
    fn key_doc_picker(
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

    /// Link or unlink one document, whichever the current state calls for, and
    /// refresh so the detail panes behind the picker show the new list.
    fn toggle_doc_link(&mut self, task_id: i64, doc: &voro_core::Doc) {
        let linked = self.doc_linked(task_id, doc.id);
        let result = if linked {
            self.store.unlink_doc(task_id, doc.id)
        } else {
            self.store.link_doc(task_id, doc.id)
        }
        .and_then(|_| self.refresh());
        if self.report(result).is_some() {
            let verb = if linked { "unlinked" } else { "linked" };
            self.status = Some(format!("{verb} {} on task {task_id}", doc.label()));
        }
    }

    /// Whether a task cites a document, read from the per-refresh link map the
    /// detail panes render — so the picker's marks and those lines can never
    /// disagree.
    pub fn doc_linked(&self, task_id: i64, doc_id: i64) -> bool {
        self.docs
            .get(&task_id)
            .is_some_and(|docs| docs.iter().any(|d| d.id == doc_id))
    }

    /// The projects screen's local keys (DESIGN.md §9). `0`–`5` sets the
    /// selected project's weight; `r` opens the AddProject form pre-filled to
    /// rename/re-path, `a` opens it blank, `d` deletes behind the store's own
    /// guard (only projects with no tasks), `v` picks the viewer, `A`
    /// toggles archived (DESIGN.md §5). Movement and screen switching are
    /// handled by `key_normal`.
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
                        path: self.project_path(project.id).to_string(),
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
            KeyCode::Char('v') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let (id, current) = (project.id, project.viewer.clone());
                    self.open_viewer_picker(id, current);
                }
            }
            KeyCode::Char('A') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let (id, name, to) = (project.id, project.name.clone(), !project.archived);
                    let result = self.store.set_archived(id, to).and_then(|_| self.refresh());
                    if self.report(result).is_some() {
                        self.status = Some(if to {
                            format!("'{name}' archived — hidden from the cockpit with its tasks")
                        } else {
                            format!("'{name}' unarchived — its tasks are back as they were")
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Open the viewer picker for a project (DESIGN.md §8/§11a): the default
    /// viewer, then each named viewer from `voro.toml`. The config is loaded
    /// fresh so a just-added `[viewers.*]` table shows up; the cursor starts on
    /// the viewer the project names.
    fn open_viewer_picker(&mut self, project_id: i64, current: Option<String>) {
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let mut options = vec![ViewerOption::Viewer(None)];
        // The built-ins are offered among the named viewers: a project may pin
        // one with no table defining it (DESIGN.md §11a).
        options.extend(
            config
                .viewer_entries()
                .into_iter()
                .map(|(name, ..)| ViewerOption::Viewer(Some(name.to_string()))),
        );
        // The quick path (DESIGN.md §5): a trailing entry that opens the
        // add-viewer form and pins this project to the viewer it creates.
        options.push(ViewerOption::NewViewer);
        let sel = options
            .iter()
            .position(|o| matches!(o, ViewerOption::Viewer(v) if *v == current))
            .unwrap_or(0);
        self.mode = Mode::ViewerPicker {
            project_id,
            options,
            current,
            sel,
        };
    }

    /// Drive the viewer picker: ⏎ stores the highlighted viewer via
    /// `set_viewer` and refreshes so the projects row reflects it;
    /// esc cancels without touching anything.
    fn key_viewer_picker(
        &mut self,
        key: KeyEvent,
        project_id: i64,
        options: Vec<ViewerOption>,
        current: Option<String>,
        mut sel: usize,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(options.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                match options.get(sel) {
                    Some(ViewerOption::Viewer(viewer)) => {
                        let viewer = viewer.clone();
                        let result = self
                            .store
                            .set_viewer(project_id, viewer.as_deref())
                            .and_then(|_| self.refresh());
                        if self.report(result).is_some() {
                            self.status =
                                Some(format!("viewer -> {}", viewer_label(viewer.as_deref())));
                        }
                    }
                    // Open the shared add-viewer form; on success it pins this
                    // project to the new viewer (DESIGN.md §5).
                    Some(ViewerOption::NewViewer) => {
                        self.open_viewer_form(None, Some(project_id));
                    }
                    None => {}
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::ViewerPicker {
            project_id,
            options,
            current,
            sel,
        };
    }

    /// The Config screen's local keys (DESIGN.md §5): `a` adds a viewer, `e`/⏎
    /// edits the selected one's command, `d` deletes it, `V`/`A` pick the default
    /// viewer/agent. Digits still jump screens; movement is `key_normal`'s.
    fn key_config(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('1') => self.screen = Screen::Cockpit,
            KeyCode::Char('2') => self.screen = Screen::Tasks,
            KeyCode::Char('3') => self.screen = Screen::Projects,
            KeyCode::Char('a') => self.open_viewer_form(None, None),
            KeyCode::Char('e') | KeyCode::Enter => self.edit_selected_viewer(),
            KeyCode::Char('d') => self.delete_selected_viewer(),
            KeyCode::Char('V') => self.open_default_picker(DefaultKind::Viewer),
            KeyCode::Char('A') => self.open_default_picker(DefaultKind::Agent),
            _ => {}
        }
    }

    /// Open the add/edit-viewer form. `existing` pre-fills it for an edit (name
    /// locked); `review_project` threads through the quick path so a viewer
    /// created from the viewer picker becomes that project's viewer.
    fn open_viewer_form(
        &mut self,
        existing: Option<(String, String)>,
        review_project: Option<i64>,
    ) {
        let (name, cmd, editing) = match existing {
            Some((name, cmd)) => (name, cmd, true),
            None => (String::new(), String::new(), false),
        };
        self.mode = Mode::ViewerForm(ViewerFormState {
            name,
            cmd,
            // An edit starts on the command field, since the name is fixed.
            on_cmd: editing,
            editing,
            review_project,
            // A new viewer's command follows its name until the operator
            // writes one; an edit's is already written.
            cmd_tracks_name: !editing,
        });
    }

    /// The command the form fills in for a name while it is still following it:
    /// the built-in's own line where the name is one, else `<name> {path}`
    /// (DESIGN.md §5). An empty name fills nothing rather than a bare
    /// placeholder, so the field starts empty and stays that way until there is
    /// something to run.
    fn tracked_viewer_cmd(name: &str) -> String {
        match name.trim().is_empty() {
            true => String::new(),
            false => voro_core::config_edit::assumed_viewer_cmd(name),
        }
    }

    /// Edit the selected viewer's command. A built-in has no table to edit —
    /// overriding it is an *add* of the same name — so it is refused with that
    /// named rather than opening a form whose write would fail.
    fn edit_selected_viewer(&mut self) {
        match self.config_viewers.get(self.config_sel) {
            Some(v) if !v.editable => {
                self.status = Some(format!(
                    "'{}' is built into voro — press a and name it '{}' to override it",
                    v.name, v.name
                ));
            }
            Some(v) => {
                let existing = (v.name.clone(), v.cmd.clone());
                self.open_viewer_form(Some(existing), None);
            }
            None => self.status = Some("no viewer selected — press a to add one".into()),
        }
    }

    /// Delete the selected viewer, refusing when a project still names it
    /// (DESIGN.md §5) — the same refusal as `voro viewer remove`, with
    /// the offending projects named. Deleting the default clears `default_viewer`.
    fn delete_selected_viewer(&mut self) {
        let Some(viewer) = self.config_viewers.get(self.config_sel) else {
            self.status = Some("no viewer selected".into());
            return;
        };
        if !viewer.editable {
            self.status = Some(format!(
                "'{}' is built into voro and cannot be deleted — press a and name it '{}' to \
                 override it",
                viewer.name, viewer.name
            ));
            return;
        }
        let name = viewer.name.clone();
        let referencing =
            voro_core::config_edit::projects_referencing_viewer(&self.projects, &name);
        if !referencing.is_empty() {
            let names: Vec<&str> = referencing.iter().map(|p| p.name.as_str()).collect();
            self.status = Some(format!(
                "'{name}' is the viewer of {} — repoint it first (v on the projects screen)",
                names.join(", ")
            ));
            return;
        }
        match voro_core::config_edit::delete_viewer(&self.dispatch_ctx.agents_path, &name) {
            Ok(cleared) => {
                self.status = Some(if cleared {
                    format!("viewer '{name}' deleted — was the default, default_viewer cleared")
                } else {
                    format!("viewer '{name}' deleted")
                });
                let result = self.refresh();
                self.report(result);
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Open the default-agent/viewer picker (DESIGN.md §5), loading `voro.toml`
    /// fresh so a just-added viewer is offered. An empty set reports what to do.
    fn open_default_picker(&mut self, kind: DefaultKind) {
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let (names, current) = match kind {
            DefaultKind::Agent => (config.agent_names(), config.default_name()),
            // The built-ins are offered too: a default naming one is exactly
            // what a fresh install wants to pin (DESIGN.md §11a).
            DefaultKind::Viewer => (
                config
                    .viewer_entries()
                    .into_iter()
                    .map(|(name, ..)| name.to_string())
                    .collect(),
                config.default_viewer_name(),
            ),
        };
        if names.is_empty() {
            self.status = Some(match kind {
                DefaultKind::Agent => "no agents are configured".into(),
                DefaultKind::Viewer => "no viewers to pick from — add one with a".into(),
            });
            return;
        }
        let sel = current
            .as_ref()
            .and_then(|c| names.iter().position(|n| n == c))
            .unwrap_or(0);
        self.mode = Mode::DefaultPicker {
            kind,
            names,
            current,
            sel,
        };
    }

    /// Drive the add/edit-viewer form. Tab toggles fields (an edit stays on the
    /// command, its name locked); ⏎ advances name → command on an add, then
    /// submits. A failed write keeps the form open with the error on the status
    /// line so a typo is fixable without retyping.
    fn key_viewer_form(&mut self, key: KeyEvent, form: ViewerFormState) {
        let ViewerFormState {
            mut name,
            mut cmd,
            on_cmd,
            editing,
            review_project,
            mut cmd_tracks_name,
        } = form;
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab => {
                let on_cmd = if editing { true } else { !on_cmd };
                self.mode = Mode::ViewerForm(ViewerFormState {
                    name,
                    cmd,
                    on_cmd,
                    editing,
                    review_project,
                    cmd_tracks_name,
                });
                return;
            }
            KeyCode::Enter => {
                if !on_cmd && !editing {
                    self.mode = Mode::ViewerForm(ViewerFormState {
                        name,
                        cmd,
                        on_cmd: true,
                        editing,
                        review_project,
                        cmd_tracks_name,
                    });
                    return;
                }
                self.submit_viewer_form(name, cmd, editing, review_project);
                return;
            }
            KeyCode::Backspace => {
                match (on_cmd, cmd_tracks_name) {
                    // Nothing to delete in a command the form is writing: it
                    // is a suggestion, not text the operator put there.
                    (true, true) => {}
                    (true, false) => {
                        cmd.pop();
                    }
                    (false, _) => {
                        name.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if on_cmd {
                    // The first character typed over a following command takes
                    // it over whole, rather than landing on the end of a line
                    // the operator never wrote.
                    if cmd_tracks_name {
                        cmd.clear();
                        cmd_tracks_name = false;
                    }
                    cmd.push(c);
                } else if !editing {
                    name.push(c);
                }
            }
            _ => {}
        }
        // Deleting back to an empty command hands it to the name again, so the
        // suggestion is recoverable with the same key that discarded it. While
        // it follows, it *is* the name's — re-derived on every keystroke — and
        // an empty command means the same thing to the writer either way.
        if on_cmd && !cmd_tracks_name && cmd.is_empty() {
            cmd_tracks_name = true;
        }
        if cmd_tracks_name {
            cmd = Self::tracked_viewer_cmd(&name);
        }
        self.mode = Mode::ViewerForm(ViewerFormState {
            name,
            cmd,
            on_cmd,
            editing,
            review_project,
            cmd_tracks_name,
        });
    }

    /// Write the viewer through the shared helper, then — for the quick path —
    /// pin the originating project to it, and refresh.
    fn submit_viewer_form(
        &mut self,
        name: String,
        cmd: String,
        editing: bool,
        review_project: Option<i64>,
    ) {
        // A blank command on an add is the common case, not a slip: it means
        // the obvious line for that name (DESIGN.md §5). Resolved here as well
        // as in the writer so the status line reports what was recorded.
        let cmd = match (editing, cmd.trim().is_empty()) {
            (false, true) => voro_core::config_edit::assumed_viewer_cmd(&name),
            _ => cmd,
        };
        let path = &self.dispatch_ctx.agents_path;
        let result = if editing {
            voro_core::config_edit::edit_viewer(path, &name, &cmd)
        } else {
            voro_core::config_edit::add_viewer(path, &name, &cmd)
        };
        if let Err(e) = result {
            self.status = Some(e.to_string());
            self.mode = Mode::ViewerForm(ViewerFormState {
                name,
                cmd,
                on_cmd: true,
                editing,
                review_project,
                // The command in hand is now what will be saved, whether the
                // form wrote it or the operator did, so a retry edits it
                // rather than watching it change under them.
                cmd_tracks_name: false,
            });
            return;
        }
        let trimmed = name.trim().to_string();
        let mut msg = if editing {
            format!("viewer '{trimmed}' updated")
        } else {
            // Name what was written, since on a blank command the operator
            // never typed it.
            format!("viewer '{trimmed}' added: {}", cmd.trim())
        };
        if voro_core::config_edit::missing_path_placeholder(&cmd) {
            msg.push_str(" (no {path} — runs in the checkout dir)");
        }
        if let Some(project_id) = review_project {
            match self.store.set_viewer(project_id, Some(&trimmed)) {
                Ok(_) => msg.push_str(" — set as this project's viewer"),
                Err(e) => msg = e.to_string(),
            }
        }
        self.status = Some(msg);
        let result = self.refresh();
        self.report(result);
        // Land the selection on what was just written, so `e`/`d` act on it
        // rather than on whichever row the list happens to sort first.
        if let Some(i) = self.config_viewers.iter().position(|v| v.name == trimmed) {
            self.config_sel = i;
        }
    }

    /// Drive the default-agent/viewer picker: ⏎ writes the choice through the
    /// shared helper and refreshes; esc cancels.
    fn key_default_picker(
        &mut self,
        key: KeyEvent,
        kind: DefaultKind,
        names: Vec<String>,
        current: Option<String>,
        mut sel: usize,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(names.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                let chosen = names[sel].clone();
                let path = &self.dispatch_ctx.agents_path;
                let result = match kind {
                    DefaultKind::Agent => voro_core::config_edit::set_default_agent(path, &chosen),
                    DefaultKind::Viewer => {
                        voro_core::config_edit::set_default_viewer(path, &chosen)
                    }
                };
                match result {
                    Ok(()) => {
                        self.status = Some(match kind {
                            DefaultKind::Agent => format!("default agent -> {chosen}"),
                            DefaultKind::Viewer => format!("default viewer -> {chosen}"),
                        });
                        let result = self.refresh();
                        self.report(result);
                    }
                    Err(e) => self.status = Some(e.to_string()),
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::DefaultPicker {
            kind,
            names,
            current,
            sel,
        };
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
                        .and_then(|_| self.store.set_default_repo_path(id, path.trim()))
                        .map(|_| ())
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

    fn key_pick_project(&mut self, key: KeyEvent, mut sel: usize, flow: CreateFlow) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(self.projects.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(project) = self.projects.get(sel) {
                    self.start_create(project.id, flow);
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::PickProject { sel, flow };
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
                    let actions = Store::legal_actions(task.state, task.human);
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
            // The popup only opens on the selected task, so the selection-based
            // helper pages the right log.
            KeyCode::Char('l') => self.view_session_log(),
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
        for dep in &form.blocked_by {
            self.store.task(*dep)?;
        }
        let task = self.store.create_task(voro_core::NewTask {
            project_id,
            repo_id: None,
            title: form.title,
            body: form.body,
            priority: form.priority,
            state: form.state.unwrap_or(TaskState::Proposed),
            agent: form.agent,
            human: form.human,
            deep: false,
        })?;
        if !form.blocked_by.is_empty() {
            self.store.set_blocks_deps(task.id, &form.blocked_by)?;
        }
        self.refresh()
    }

    pub fn update_from_form(
        &mut self,
        task_id: i64,
        form: crate::editor::TaskForm,
    ) -> voro_core::Result<()> {
        // The form edits content; `deep` is not among its fields, so it is
        // carried through untouched — `!` is the key that changes it.
        let deep = self.store.task(task_id)?.deep;
        self.store.update_task(
            task_id,
            voro_core::TaskEdit {
                title: form.title,
                body: form.body,
                priority: form.priority,
                agent: form.agent,
                human: form.human,
                deep,
            },
        )?;
        self.store.set_blocks_deps(task_id, &form.blocked_by)?;
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

    fn ctrl_key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::CONTROL));
    }

    /// A `DispatchCtx` that is never actually used to spawn anything in these
    /// tests — the transitions they drive (`resume`, reject) only move state.
    fn dummy_ctx() -> crate::dispatch::DispatchCtx {
        crate::dispatch::DispatchCtx::without_config(std::path::Path::new("/nonexistent/voro.db"))
    }

    /// A store with one project and one task per requested state, reached
    /// through the real transition machine.
    fn app_with(states: &[TaskState]) -> App {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        for state in states {
            let created = match state {
                TaskState::Proposed | TaskState::Refining => TaskState::Proposed,
                _ => TaskState::Ready,
            };
            let task = store
                .create_task(NewTask {
                    project_id: project.id,
                    repo_id: None,
                    title: format!("{state} task"),
                    body: String::new(),
                    priority: Priority::P1,
                    state: created,
                    agent: None,
                    human: false,
                    deep: false,
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
                // A hand-off, dispatched first so it carries the open session
                // `waiting` keeps (DESIGN.md §8) — what the quick-message and
                // jump-in keys read off the strip row.
                TaskState::Waiting => {
                    store
                        .record_dispatch(task.id, "claude", None, None)
                        .unwrap();
                    store.apply(task.id, Action::Complete(None)).unwrap();
                    store.apply(task.id, Action::HandOff).unwrap();
                }
                // A dispatch that died: reconcile records the outcome and
                // stalls the task (DESIGN.md §8).
                TaskState::Stalled => {
                    let (_, session) = store
                        .record_dispatch(task.id, "claude", Some(1), Some("/tmp/demo/s.log"))
                        .unwrap();
                    store.reconcile_session(session.id, false, false).unwrap();
                }
                // A refine round in flight. No pid: liveness is then unknowable,
                // so reconcile-on-read leaves the round alone instead of
                // finalising it out from under the test — and the cancel key's
                // kill has no real process to aim at.
                TaskState::Refining => {
                    store
                        .record_refine_launch(
                            task.id,
                            "thin body",
                            "claude",
                            None,
                            Some("/tmp/demo/refine.log"),
                        )
                        .unwrap();
                }
                other => panic!("fixture does not build {other} tasks"),
            }
        }
        App::new(store, dummy_ctx()).unwrap()
    }

    /// A store with nothing in it at all — the first run Voro has to land well.
    fn empty_app() -> App {
        App::new(Store::open_in_memory().unwrap(), dummy_ctx()).unwrap()
    }

    /// The first run: with no project registered the cockpit has nothing to
    /// show and its `n` cannot proceed, so the app opens where the first step
    /// is (DESIGN.md §9), saying why.
    #[test]
    fn a_clean_database_opens_on_the_projects_screen() {
        let app = empty_app();
        assert_eq!(app.screen, Screen::Projects);
        let status = app.status.clone().expect("the landing explains itself");
        assert!(status.contains("press a"), "{status}");
        assert!(status.contains('n'), "{status}");
    }

    #[test]
    fn a_database_with_a_project_opens_on_the_cockpit() {
        let app = app_with(&[]);
        assert_eq!(app.screen, Screen::Cockpit);
        assert_eq!(app.status, None);
    }

    /// The landing is decided once, at startup. `refresh` runs after every
    /// mutation and on every external-change poll, so deciding there would yank
    /// the operator to Projects mid-session — for instance on the cockpit of a
    /// database whose last project they just deleted.
    #[test]
    fn refresh_leaves_the_screen_where_the_operator_put_it() {
        let mut app = empty_app();
        app.screen = Screen::Cockpit;
        app.refresh().unwrap();
        assert_eq!(app.screen, Screen::Cockpit);
    }

    /// `n` with no projects refuses in the keys README.md teaches — `tab` and
    /// `a`, not a screen number.
    #[test]
    fn new_task_without_projects_points_at_tab_and_a() {
        let mut app = empty_app();
        app.screen = Screen::Cockpit;
        for press in ['n', 'N'] {
            app.status = None;
            key(&mut app, KeyCode::Char(press));
            assert_eq!(app.status.as_deref(), Some(NO_PROJECTS_HINT));
            assert!(app.pending_editor.is_none());
        }
    }

    /// Enter on a needs-input inbox row resumes the task directly — the
    /// operator answered in the agent's own session, so there is no answer
    /// prompt (DESIGN.md §6/§8), just the `needs-input → running` transition.
    #[test]
    fn enter_on_needs_input_row_resumes_and_requeues() {
        let mut app = app_with(&[TaskState::NeedsInput]);
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Queue(_)
        ));
        assert_eq!(app.enter_hint(), Some("⏎ resume"));
        let task_id = app.queue_task_ids()[0];

        key(&mut app, KeyCode::Enter);
        assert!(
            matches!(app.mode, Mode::Normal),
            "resume applies directly, opening no prompt"
        );
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        assert!(app.queue.rows.is_empty());
    }

    /// A scratch database, a freshly-`git init`ed clean project, and (unless
    /// `agents_toml` is `None`, for the missing-config case) a `voro.toml`
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
        let agents_path = root.join("voro.toml");
        if let Some(toml) = agents_toml {
            std::fs::write(&agents_path, toml).unwrap();
        }
        let store = Store::open(&db_path).unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        (store, ctx, project_path)
    }

    /// Resuming a dispatched task from the cockpit keeps its live agent session
    /// — the operator answered in that session, so no continuation is spawned
    /// (DESIGN.md §6/§8): the task returns to `running` on the one session it
    /// already had.
    #[test]
    fn resuming_a_task_with_a_live_session_spawns_no_continuation() {
        use std::process::{Command, Stdio};

        let root = std::env::temp_dir().join(format!(
            "voro-app-resume-{}-{}",
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
        let agents_path = root.join("voro.toml");
        // The dispatched session must still be alive when `apply_and_refresh`'s
        // own `self.refresh()` reconciles-on-read immediately after the resume —
        // an instantly-exiting stub (`cat`) would race that read and get
        // finalised as a failed session, stalling the task before the
        // assertions below run.
        std::fs::write(
            &agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"sleep 1 && cat {prompt_file}\"\n",
        )
        .unwrap();

        let mut store = Store::open(&db_path).unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: db_path.clone(),
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "Do the thing".into(),
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        store.apply(task.id, Action::Ask("A or B?".into())).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Running);
        assert_eq!(
            app.store.sessions_for(task.id).unwrap().len(),
            1,
            "resume must not spawn a second session"
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
                assert_eq!(*actions, Store::legal_actions(TaskState::Review, false));
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

    /// Proposals ride the queue as one digest row per project (DESIGN.md §7),
    /// so triage is two keystrokes: Enter folds the digest open, Enter on the
    /// proposal beneath it opens the triage menu as before.
    #[test]
    fn enter_expands_the_digest_then_opens_the_triage_menu() {
        let mut app = app_with(&[TaskState::Proposed]);
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Queue(_)
        ));
        // The digest names no task of its own — the row stands for the backlog.
        assert_eq!(app.selected_task_id(), None);
        assert_eq!(app.enter_hint(), Some("⏎ expand"));

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.enter_hint(), Some("⏎ collapse"));
        assert!(matches!(app.cockpit_rows[1], CockpitRow::Proposal(0, 0)));

        app.move_selection(1);
        assert_eq!(app.enter_hint(), Some("⏎ triage"));
        key(&mut app, KeyCode::Enter);
        let task_id = match &app.mode {
            Mode::Transition {
                actions, task_id, ..
            } => {
                assert_eq!(*actions, Store::legal_actions(TaskState::Proposed, false));
                *task_id
            }
            _ => panic!("enter on a proposed row should open the triage menu"),
        };

        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Ready);
        // the triaged task re-enters the queue as startable work
        assert_eq!(app.queue.rows.len(), 1);
        assert_eq!(app.enter_hint(), Some("⏎ act"));
    }

    /// Select a queued proposal: the cockpit collapses them into a per-project
    /// digest (DESIGN.md §7), so Enter folds it open before a constituent row
    /// can be selected.
    fn select_proposal(app: &mut App) -> i64 {
        key(app, KeyCode::Enter);
        app.move_selection(1);
        app.selected_task_id()
            .expect("a folded-open digest should select a proposal")
    }

    /// Refine answers from the queue (DESIGN.md §6): `r` over a selected
    /// proposal collects the note directly.
    #[test]
    fn refine_key_on_the_queue_collects_a_note_without_the_triage_menu() {
        let mut app = app_with(&[TaskState::Proposed]);
        let task_id = select_proposal(&mut app);

        key(&mut app, KeyCode::Char('r'));
        match &app.mode {
            Mode::Prompt {
                task_id: id,
                kind: PromptKind::RefineNote,
                buffer,
            } => {
                assert_eq!(*id, task_id);
                assert!(buffer.is_empty(), "buffer was {buffer:?}");
            }
            _ => panic!("r on a queued proposal should open the refine-note prompt"),
        }

        // The launch itself needs a configured agent, which the dummy context
        // has none of, so what is asserted is the path: submitting the note
        // reaches the dispatch and reports, and the task stays `proposed`
        // either way — refine is an event, not a verdict.
        for c in "thin".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.is_some(), "the launch outcome is reported");
        assert_eq!(
            app.store.task(task_id).unwrap().state,
            TaskState::Proposed,
            "refine never transitions the task"
        );
    }

    /// `R` over a queued proposal reaches the interactive variant. The dummy
    /// context configures no `plan` verb, so the failure lands on the status
    /// line rather than transitioning anything.
    #[test]
    fn talk_key_on_the_queue_reaches_the_plan_flow() {
        let mut app = app_with(&[TaskState::Proposed]);
        let task_id = select_proposal(&mut app);

        key(&mut app, KeyCode::Char('R'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.pending_plan.is_some() || app.status.is_some());
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Proposed);
    }

    /// A task triaged `ready` against a body the operator has since soured on
    /// refines from the queue exactly as a proposal does (DESIGN.md §6).
    #[test]
    fn refine_key_answers_on_a_ready_task_too() {
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.selected_task_id().expect("the ready row is selected");

        key(&mut app, KeyCode::Char('r'));
        match &app.mode {
            Mode::Prompt {
                task_id: id,
                kind: PromptKind::RefineNote,
                ..
            } => assert_eq!(*id, task_id),
            _ => panic!("r on a queued ready task should open the refine-note prompt"),
        }
    }

    /// Past `ready` the body is a brief already being worked, so the queue's
    /// refine keys are a no-op that says why, the same style as the other action
    /// keys — not a silent swallow.
    #[test]
    fn the_queue_refine_keys_explain_themselves_on_work_under_way() {
        let mut app = app_with(&[TaskState::Review]);

        key(&mut app, KeyCode::Char('r'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status.as_deref().is_some_and(|s| s.contains("refine")),
            "expected a status line explaining refine, got {:?}",
            app.status
        );
    }

    /// A refine in flight is out of the triage queue and on the running strip
    /// instead (DESIGN.md §6/§9) — in *this* window whether or not it launched
    /// the round, because the state is in the store rather than in a flag the
    /// launching process holds.
    #[test]
    fn a_refining_proposal_leaves_the_queue_for_the_running_strip() {
        let mut app = app_with(&[TaskState::Refining, TaskState::Ready]);
        let refining = app.all[0].task.id;

        assert!(
            !app.queue_task_ids().contains(&refining),
            "{:?}",
            app.queue_task_ids()
        );
        assert!(
            app.queue
                .rows
                .iter()
                .all(|row| !matches!(row, QueueRow::Digest(_))),
            "a refining proposal must not ride the triage digest either"
        );
        assert_eq!(app.running.len(), 1);
        assert_eq!(app.running[0].task_state, TaskState::Refining);
        assert_eq!(app.counts.refining, 1);
        assert_eq!(app.counts.proposed, 0);

        // and the strip row is selectable, so the detail card and `C` reach it
        let strip = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .expect("the refine rides the strip");
        app.cockpit_sel = strip;
        assert!(app.selected_is_refining());
    }

    /// A hand-off is work in flight someone else owns, which is the same fact
    /// the strip carries about a dispatch (DESIGN.md §9). It rides the strip
    /// while staying out of the queue and out of `next` — `waiting` earns no
    /// score (§7), and putting it on the strip does not change that.
    #[test]
    fn a_waiting_task_rides_the_strip_and_not_the_queue() {
        let mut app = app_with(&[TaskState::Waiting, TaskState::Ready]);
        let waiting = app
            .all
            .iter()
            .find(|r| r.task.state == TaskState::Waiting)
            .unwrap()
            .task
            .id;

        assert!(
            !app.queue_task_ids().contains(&waiting),
            "{:?}",
            app.queue_task_ids()
        );
        assert_eq!(app.running.len(), 1);
        assert_eq!(app.running[0].task_id, waiting);
        assert_eq!(app.running[0].task_state, TaskState::Waiting);
        assert_eq!(app.counts.waiting, 1);

        let strip = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .expect("the hand-off rides the strip");
        app.cockpit_sel = strip;
        assert_eq!(app.selected_task_id(), Some(waiting));
    }

    /// The strip's newest row kind reaches the verdicts `waiting` offers
    /// (DESIGN.md §6) through the same transition menu every other row uses —
    /// a merged PR is accepted without leaving the cockpit.
    #[test]
    fn a_verdict_applies_from_a_waiting_strip_row() {
        let mut app = app_with(&[TaskState::Waiting]);
        let task_id = app.running[0].task_id;
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        key(&mut app, KeyCode::Char('s'));
        match &app.mode {
            Mode::Transition { actions, .. } => assert_eq!(
                actions.iter().map(action_label).collect::<Vec<_>>(),
                vec![
                    "accept → done",
                    "reject with feedback → running",
                    "reclaim → review",
                    "abandon → rejected",
                ]
            ),
            _ => panic!("s on a waiting strip row should open the transition menu"),
        }
        key(&mut app, KeyCode::Enter);

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Done);
        assert!(app.running.is_empty(), "{:?}", app.running);
    }

    /// The reclaim verdict returns a hand-off to the operator's own queue, so
    /// the row leaves the strip for a queue row rather than closing.
    #[test]
    fn reclaiming_from_the_strip_returns_the_task_to_the_queue() {
        let mut app = app_with(&[TaskState::Waiting]);
        let task_id = app.running[0].task_id;

        app.apply_and_refresh(task_id, Action::Reclaim);

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Review);
        assert!(app.running.is_empty(), "{:?}", app.running);
        assert!(app.queue_task_ids().contains(&task_id));
    }

    /// Dispatch-oriented keys no-op with an explanation on a hand-off, as they
    /// do on a refine (DESIGN.md §9) — while the two session keys, which
    /// `waiting` has always accepted, keep working from the strip row.
    #[test]
    fn dispatch_keys_explain_themselves_on_a_waiting_row() {
        let mut app = app_with(&[TaskState::Waiting]);
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        for (k, expected) in [('d', "dispatched"), ('o', "viewer"), ('C', "refine")] {
            app.status = None;
            key(&mut app, KeyCode::Char(k));
            let status = app.status.as_deref().unwrap_or_default().to_string();
            assert!(status.contains("waiting"), "{k}: {status}");
            assert!(status.contains(expected), "{k}: {status}");
        }
        assert_eq!(app.store.task(1).unwrap().state, TaskState::Waiting);

        // `a` and `A` are gated on the state, which accepts `waiting`: what
        // stops them here is the fixture's missing session ref, not the row.
        for k in ['a', 'A'] {
            app.status = None;
            key(&mut app, KeyCode::Char(k));
            let status = app.status.as_deref().unwrap_or_default().to_string();
            assert!(
                !status.contains("task is waiting"),
                "{k} must not refuse a hand-off on its state: {status}"
            );
        }
    }

    /// The cancel key (DESIGN.md §6): the escape hatch for a hung agent
    /// reconcile cannot catch. The round ends unmarked — a cancel is a no-op,
    /// not a failure — and the proposal is back in the queue.
    #[test]
    fn the_cancel_key_ends_a_refine_round_and_returns_the_proposal() {
        let mut app = app_with(&[TaskState::Refining]);
        let task_id = app.running[0].task_id;
        let session = app.store.sessions_for(task_id).unwrap()[0].id;
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        key(&mut app, KeyCode::Char('C'));

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Proposed);
        assert!(app.running.is_empty());
        assert!(!app.refined.contains(&task_id));
        assert!(!app.refine_failed.contains(&task_id));
        let session = app.store.session(session).unwrap();
        assert_eq!(session.outcome, Some(voro_core::SessionOutcome::Aborted));
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("cancelled")),
            "{:?}",
            app.status
        );
    }

    /// On anything else `C` says so rather than swallowing the keypress, the
    /// same no-op-with-explanation style as the other action keys.
    #[test]
    fn the_cancel_key_explains_itself_off_a_refine() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('C'));
        assert_eq!(app.store.task(1).unwrap().state, TaskState::Ready);
        assert!(
            app.status.as_deref().is_some_and(|s| s.contains("refine")),
            "{:?}",
            app.status
        );
    }

    /// The refine keys on a task already refining point at the cancel rather
    /// than reading as the generic "not a proposal" refusal.
    #[test]
    fn the_refine_keys_name_the_cancel_on_a_round_in_flight() {
        let mut app = app_with(&[TaskState::Refining]);
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        for k in ['r', 'R'] {
            key(&mut app, KeyCode::Char(k));
            assert!(matches!(app.mode, Mode::Normal));
            let status = app.status.as_deref().unwrap_or_default().to_string();
            assert!(status.contains("already being refined"), "{k}: {status}");
        }
        assert_eq!(app.store.task(1).unwrap().state, TaskState::Refining);
    }

    /// The verdict keys cannot reach a refining task at all: `legal_actions`
    /// offers only the cancel, so the mid-refine race closes at the store layer
    /// with no guard code in the TUI (DESIGN.md §6).
    #[test]
    fn the_transition_menu_on_a_refining_task_offers_only_the_cancel() {
        let mut app = app_with(&[TaskState::Refining]);
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        key(&mut app, KeyCode::Char('s'));
        match &app.mode {
            Mode::Transition { actions, .. } => {
                assert_eq!(actions.len(), 1, "{actions:?}");
                assert!(
                    matches!(actions[0], Action::ConcludeRefine(_)),
                    "{actions:?}"
                );
            }
            _ => panic!("s on a refining task should open the transition menu"),
        }
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(1).unwrap().state, TaskState::Proposed);
    }

    /// Dispatch-oriented keys no-op with an explanation on a strip row that is a
    /// refine rather than a dispatch (DESIGN.md §9).
    #[test]
    fn dispatch_keys_explain_themselves_on_a_refining_row() {
        let mut app = app_with(&[TaskState::Refining]);
        app.cockpit_sel = app
            .cockpit_rows
            .iter()
            .position(|r| matches!(r, CockpitRow::Running(_)))
            .unwrap();

        for (k, expected) in [
            ('d', "dispatched"),
            ('a', "a message lands on"),
            ('A', "jump-in"),
            ('o', "viewer"),
        ] {
            app.status = None;
            key(&mut app, KeyCode::Char(k));
            let status = app.status.as_deref().unwrap_or_default().to_string();
            assert!(status.contains("refining"), "{k}: {status}");
            assert!(status.contains(expected), "{k}: {status}");
        }
        assert_eq!(app.store.task(1).unwrap().state, TaskState::Refining);
    }

    /// Refresh keeps a key, one modifier along: `ctrl-r` refreshes and does not
    /// fall through to refine, even with a proposal selected.
    #[test]
    fn ctrl_r_refreshes_rather_than_refining() {
        let mut app = app_with(&[TaskState::Proposed]);
        select_proposal(&mut app);

        ctrl_key(&mut app, KeyCode::Char('r'));
        assert!(
            matches!(app.mode, Mode::Normal),
            "ctrl-r should not open the refine prompt"
        );
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
                assert_eq!(*actions, Store::legal_actions(TaskState::Ready, false));
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
        let events = app.task_events(app.queue_task_ids()[0]);
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

    /// `!` is the same toggle wherever a task is selected (task #241): the
    /// cockpit queue, the tasks list, and the detail popup that opens over it.
    #[test]
    fn deep_key_toggles_the_flag_on_every_screen() {
        let mut app = app_with(&[TaskState::Ready]);
        let id = app.queue_task_ids()[0];

        key(&mut app, KeyCode::Char('!'));
        assert!(app.store.task(id).unwrap().deep);
        assert!(app.status.as_ref().unwrap().contains("strongest model"));
        key(&mut app, KeyCode::Char('!'));
        assert!(!app.store.task(id).unwrap().deep);

        app.toggle_screen();
        assert_eq!(app.screen, Screen::Tasks);
        key(&mut app, KeyCode::Char('!'));
        assert!(app.store.task(id).unwrap().deep);

        // ...and inside the detail popup, without closing it
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Detail { .. }));
        key(&mut app, KeyCode::Char('!'));
        assert!(!app.store.task(id).unwrap().deep);
        assert!(matches!(app.mode, Mode::Detail { .. }));
    }

    /// A human task is never dispatched, so it has no model to deepen; the
    /// store's refusal reaches the status line rather than the flag.
    #[test]
    fn deep_key_on_a_human_task_reports_and_changes_nothing() {
        let mut app = app_with(&[TaskState::Ready]);
        let id = app.queue_task_ids()[0];
        let task = app.store.task(id).unwrap();
        app.store
            .update_task(
                id,
                voro_core::TaskEdit {
                    title: task.title,
                    body: task.body,
                    priority: task.priority,
                    agent: None,
                    human: true,
                    deep: false,
                },
            )
            .unwrap();
        app.refresh().unwrap();

        key(&mut app, KeyCode::Char('!'));
        assert!(!app.store.task(id).unwrap().deep);
        assert!(
            app.status.as_ref().is_some_and(|s| s.contains("deep")),
            "{:?}",
            app.status
        );
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

    /// Tab cycles cockpit → tasks → projects → cockpit, and `1`/`2`/`3` jump to
    /// a screen directly from the task-oriented screens. On the projects screen
    /// the digits set weight instead, so it is reached with `3` and left via
    /// tab.
    #[test]
    fn tab_and_digits_move_between_the_four_screens() {
        let mut app = app_with(&[]);
        assert_eq!(app.screen, Screen::Cockpit);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Tasks);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Projects);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Config);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Cockpit);

        key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.screen, Screen::Tasks);
        key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Cockpit);
        key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.screen, Screen::Config);
        // The config screen's letter keys are its own, but the digit jumps still
        // work and tab cycles on.
        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        // On the projects screen the digit jump is superseded by weight-setting;
        // tab is the way back out.
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Config);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Cockpit);
    }

    /// Type each character of `s` as a `Char` key press.
    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    /// The Config screen's row index for a viewer. The built-ins share the
    /// list, so a test never assumes where a name sorts.
    fn row(app: &App, name: &str) -> usize {
        app.config_viewers
            .iter()
            .position(|v| v.name == name)
            .unwrap_or_else(|| panic!("no viewer row named {name}"))
    }

    /// `o` with no viewer set up anywhere raises the add-viewer form rather
    /// than only complaining (#405), and says why on the status line; every
    /// other way opening can fail still just reports. Driven through
    /// `report_open` rather than the key, since which arm a real keypress
    /// takes depends on what the developer has installed.
    #[test]
    fn no_viewer_at_all_raises_the_add_viewer_form() {
        let (store, ctx, _project) = scratch_env("open-no-viewer", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        app.report_open(Err(crate::dispatch::OpenFailure::NoViewer(
            "no viewer set up — run `voro viewer add …`".into(),
        )));
        assert!(
            matches!(
                app.mode,
                Mode::ViewerForm(ViewerFormState { editing: false, .. })
            ),
            "expected the add-viewer form to open"
        );
        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.starts_with("no viewer set up — name one here"),
            "{status}"
        );
        assert!(status.contains("code/cursor/zed"), "{status}");
        // the status line wraps rather than truncating (§9), so the diagnosis
        // is never lost — but the action is what the operator acts on, so it
        // still comes first
        assert!(
            status.find("name one here").unwrap() < status.find("no built-in").unwrap(),
            "{status}"
        );

        // anything else opening can fail on is reported, not answered
        app.mode = Mode::Normal;
        app.report_open(Err(crate::dispatch::OpenFailure::Failed(
            "no viewer named 'nope'".into(),
        )));
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status.as_deref(), Some("no viewer named 'nope'"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The command field writes itself from the name until the operator writes
    /// one, and comes back when what they wrote is deleted (#405). The name is
    /// the only thing they must know; the command is a suggestion they can
    /// watch, take over, or undo.
    #[test]
    fn the_form_command_follows_the_name_until_it_is_written() {
        let (store, ctx, _project) = scratch_env("config-follows", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();
        let form = |app: &App| -> (String, String, bool) {
            match &app.mode {
                Mode::ViewerForm(ViewerFormState {
                    name,
                    cmd,
                    cmd_tracks_name,
                    ..
                }) => (name.clone(), cmd.clone(), *cmd_tracks_name),
                _ => panic!("expected the viewer form"),
            }
        };

        app.screen = Screen::Cockpit;
        key(&mut app, KeyCode::Char('4'));
        key(&mut app, KeyCode::Char('a'));
        // an empty name fills nothing rather than a bare placeholder
        assert_eq!(form(&app), (String::new(), String::new(), true));

        // the command assembles itself keystroke by keystroke, backspace and all
        type_str(&mut app, "zed");
        assert_eq!(form(&app).1, "zed {path}");
        key(&mut app, KeyCode::Backspace);
        assert_eq!(form(&app).1, "ze {path}");

        // a name that is a built-in's follows that built-in's own line
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        type_str(&mut app, "code");
        assert_eq!(form(&app).1, "code -n {path}");

        // on the command field, backspace leaves a suggestion alone — there is
        // nothing there the operator typed
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(form(&app), ("code".into(), "code -n {path}".into(), true));

        // …and the first character typed takes the field over whole
        type_str(&mut app, "x");
        assert_eq!(form(&app), ("code".into(), "x".into(), false));
        type_str(&mut app, "y");
        assert_eq!(form(&app).1, "xy");

        // deleting back to empty hands it to the name again
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(form(&app), ("code".into(), "code -n {path}".into(), true));

        // a written command is not rewritten by a later name edit
        type_str(&mut app, "mine {path}");
        key(&mut app, KeyCode::Tab);
        type_str(&mut app, "r");
        assert_eq!(form(&app), ("coder".into(), "mine {path}".into(), false));

        // and what is saved is what the form showed (⏎ on the name advances,
        // ⏎ on the command saves)
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.config_viewers[row(&app, "coder")].cmd, "mine {path}");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Naming an editor is enough (#405): ⏎ through the command field records
    /// `<name> {path}`, and a name that is a built-in's records that built-in's
    /// own line, so an override starts from what it replaces.
    #[test]
    fn a_viewer_added_with_a_blank_command_gets_the_obvious_one() {
        let (store, ctx, _project) = scratch_env("config-blank-cmd", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        // No project is registered, so the app opened on Projects, where the
        // digits are weights; the jump below is a cockpit key.
        app.screen = Screen::Cockpit;
        key(&mut app, KeyCode::Char('4'));
        key(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "emacsclient");
        key(&mut app, KeyCode::Enter); // name -> command
        key(&mut app, KeyCode::Enter); // submit with the command blank
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            app.config_viewers[row(&app, "emacsclient")].cmd,
            "emacsclient {path}"
        );
        // the status names what was written, since it was never typed
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("emacsclient {path}"), "{status}");

        // and overriding a built-in reproduces it rather than guessing at it
        key(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "code");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        let code = &app.config_viewers[row(&app, "code")];
        assert_eq!(code.cmd, "code -n {path}");
        assert_eq!(code.provenance, "user override");
        assert!(code.editable);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The Config screen (DESIGN.md §5): add, edit, set-default, and delete a
    /// viewer entirely through the TUI, each edit landing in `voro.toml` and
    /// reflected on the next refresh.
    #[test]
    fn config_screen_adds_edits_defaults_and_deletes_a_viewer() {
        let (store, ctx, _project) = scratch_env("config-crud", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        // No project is registered, so the app opened on Projects, where the
        // digits are weights; the jump below is a cockpit key.
        app.screen = Screen::Cockpit;
        key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.screen, Screen::Config);
        // the built-in agents and viewers are both listed, the viewers'
        // built-in rows read-only (#405)
        assert!(app.config_agents.iter().any(|a| a.name == "claude"));
        assert!(
            app.config_viewers
                .iter()
                .any(|v| v.name == "code" && !v.editable && v.provenance == "built-in")
        );
        assert!(app.config_viewers.iter().all(|v| !v.editable));

        // e/d on a built-in row refuse, naming the override that replaces it
        app.config_sel = row(&app, "code");
        for k in ['d', 'e'] {
            key(&mut app, KeyCode::Char(k));
            let status = app.status.clone().unwrap_or_default();
            assert!(status.contains("built into voro"), "{status}");
            assert!(status.contains("override"), "{status}");
            assert!(matches!(app.mode, Mode::Normal));
        }
        assert!(app.config_viewers.iter().any(|v| v.name == "code"));

        // add: a opens the form, name → Enter → command → Enter submits
        key(&mut app, KeyCode::Char('a'));
        assert!(matches!(
            app.mode,
            Mode::ViewerForm(ViewerFormState { editing: false, .. })
        ));
        type_str(&mut app, "mine");
        key(&mut app, KeyCode::Enter);
        type_str(&mut app, "mine {path}");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        // the selection lands on what was just written
        assert_eq!(app.config_viewers[app.config_sel].name, "mine");
        assert_eq!(app.config_viewers[app.config_sel].cmd, "mine {path}");
        assert_eq!(app.config_viewers[app.config_sel].provenance, "user");
        assert_eq!(
            AgentsConfig::load(&path)
                .unwrap()
                .viewer_cmd(Some("mine"))
                .unwrap(),
            "mine {path}"
        );

        // edit: e opens the form with the name locked; append to the command
        key(&mut app, KeyCode::Char('e'));
        assert!(matches!(
            app.mode,
            Mode::ViewerForm(ViewerFormState { editing: true, .. })
        ));
        type_str(&mut app, " --wait");
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.config_viewers[row(&app, "mine")].cmd,
            "mine {path} --wait"
        );

        // default: V opens the picker over every viewer, built-ins included;
        // walk to the top and back down to `mine`, since where the cursor
        // starts depends on what is installed
        key(&mut app, KeyCode::Char('V'));
        let names = match &app.mode {
            Mode::DefaultPicker { names, .. } => names.clone(),
            _ => panic!("expected the default picker to open"),
        };
        assert!(names.iter().any(|n| n == "code"), "{names:?}");
        let target = names.iter().position(|n| n == "mine").unwrap();
        for _ in 0..names.len() {
            key(&mut app, KeyCode::Char('k'));
        }
        for _ in 0..target {
            key(&mut app, KeyCode::Char('j'));
        }
        key(&mut app, KeyCode::Enter);
        assert!(app.config_viewers[row(&app, "mine")].is_default);
        assert_eq!(
            AgentsConfig::load(&path)
                .unwrap()
                .default_viewer_name()
                .as_deref(),
            Some("mine")
        );

        // delete: d removes it and clears the now-dangling default
        app.config_sel = row(&app, "mine");
        key(&mut app, KeyCode::Char('d'));
        assert!(app.config_viewers.iter().all(|v| v.name != "mine"));
        let config = AgentsConfig::load(&path).unwrap();
        assert!(config.viewer_names().is_empty());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_viewer")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Deleting a viewer a project still names is refused on the
    /// Config screen too, naming the project (DESIGN.md §5).
    #[test]
    fn config_screen_refuses_to_delete_a_referenced_viewer() {
        let toml = "[viewers.zed]\ncmd = \"zed {path}\"\n";
        let (mut store, ctx, _project) = scratch_env("config-ref", Some(toml));
        let project = store.create_project("demo2", "/tmp/demo2").unwrap();
        store.set_viewer(project.id, Some("zed")).unwrap();
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        key(&mut app, KeyCode::Char('4'));
        app.config_sel = row(&app, "zed");
        assert!(app.config_viewers[app.config_sel].editable);
        key(&mut app, KeyCode::Char('d'));
        assert!(
            app.status.as_deref().unwrap_or("").contains("demo2"),
            "refusal should name the project: {:?}",
            app.status
        );
        // still there, in the file and the view
        assert!(app.config_viewers.iter().any(|v| v.name == "zed"));
        assert!(
            AgentsConfig::load(&path)
                .unwrap()
                .viewer_names()
                .contains(&"zed".to_string())
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The quick path (DESIGN.md §5): the projects screen's viewer picker
    /// grows a "new viewer…" entry that opens the add-viewer form and, on
    /// success, pins the project to the viewer it created.
    #[test]
    fn viewer_picker_new_viewer_creates_and_pins_it() {
        let (mut store, ctx, project_path) = scratch_env("config-quickpath", None);
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        // onto the projects screen, open the viewer picker
        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        key(&mut app, KeyCode::Char('v'));
        let n = match &app.mode {
            Mode::ViewerPicker { options, .. } => options.len(),
            _ => panic!("expected the viewer picker to open"),
        };
        // the last option is "new viewer…"; move to it and select
        for _ in 0..n {
            key(&mut app, KeyCode::Char('j'));
        }
        key(&mut app, KeyCode::Enter);
        assert!(
            matches!(
                app.mode,
                Mode::ViewerForm(ViewerFormState {
                    review_project: Some(_),
                    ..
                })
            ),
            "new viewer… should open the form carrying the project"
        );
        type_str(&mut app, "emacs");
        key(&mut app, KeyCode::Enter);
        type_str(&mut app, "emacsclient {path}");
        key(&mut app, KeyCode::Enter);

        // the viewer exists and the project is now pinned to it
        assert!(
            AgentsConfig::load(&path)
                .unwrap()
                .viewer_names()
                .contains(&"emacs".to_string())
        );
        assert_eq!(
            app.store.project(project.id).unwrap().viewer.as_deref(),
            Some("emacs")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
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

        // `1`/`2`/`3` set weight here rather than jumping screens, so every
        // value 0–5 is reachable.
        for digit in ['1', '2', '3'] {
            key(&mut app, KeyCode::Char(digit));
            assert_eq!(app.screen, Screen::Projects);
            let expected = digit.to_digit(10).unwrap() as i64;
            assert_eq!(app.store.project(project_id).unwrap().weight, expected);
        }
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
        assert_eq!(app.project_path(project_id), "/tmp/moved");
        // tasks reference the project by id, so renaming leaves them intact
        let stored = app.store.project(project_id).unwrap();
        assert_eq!(stored.name, "renamed");
        assert_eq!(
            app.store.default_repo(project_id).unwrap().path,
            "/tmp/moved"
        );
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

    /// `A` on the projects screen toggles archived (DESIGN.md §5): the
    /// project's tasks leave the queue and counts wholesale, states untouched,
    /// and toggle back exactly as they were.
    #[test]
    fn projects_screen_archive_toggles_and_the_cockpit_empties() {
        let mut app = app_with(&[TaskState::Ready, TaskState::NeedsInput]);
        let project_id = app.projects[0].id;
        assert_eq!(app.queue.rows.len(), 2);
        key(&mut app, KeyCode::Char('3'));

        key(&mut app, KeyCode::Char('A'));
        assert!(app.store.project(project_id).unwrap().archived);
        assert!(app.projects[0].archived);
        assert!(app.queue.rows.is_empty());
        assert_eq!(app.counts.ready, 0);
        assert_eq!(app.counts.needs_input, 0);
        // the tasks froze rather than transitioned
        assert_eq!(app.all[0].task.state, TaskState::NeedsInput);

        key(&mut app, KeyCode::Char('A'));
        assert!(!app.store.project(project_id).unwrap().archived);
        assert_eq!(app.queue.rows.len(), 2);
        assert_eq!(app.counts.needs_input, 1);
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
        // before the assertions below run (see the resume test above for the
        // same race).
        let (mut store, ctx, project_path) = scratch_env(
            "dispatch",
            Some(
                "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"sleep 1 && cat {prompt_file}\"\n",
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
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
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

    /// Dispatch requires `ready` or `stalled` (DESIGN.md §8); on anything else
    /// the key no-ops with a status message rather than erroring deep inside
    /// dispatch or silently doing nothing, mirroring how `s` reports a state
    /// with nowhere to go.
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
                .contains("only ready or stalled tasks can be dispatched"),
            "{:?}",
            app.status
        );
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
    /// empty modal. (A *missing* file is no longer a failure: the built-ins
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

    /// A listing showing the fixture's session still going, and one showing it
    /// finished — what decides the jump-in verb (task #332).
    const LIVE_LISTING: &str = r#"[{"sessionId": "ref-1", "state": "working"}]"#;
    const FINISHED_LISTING: &str = r#"[{"sessionId": "ref-1", "state": "done"}]"#;

    /// The zombie shape the pid rule exists for (task #376): an entry the
    /// agent's listing leaves at `blocked` long after the session died, with
    /// no pid to check. It read as live while not-`done` meant live, which
    /// sent `A` at `claude attach <uuid>` — "No job matching" — and made `a`
    /// refuse a session there was nothing to attach to.
    const ZOMBIE_LISTING: &str = r#"[{"sessionId": "ref-1", "state": "blocked"}]"#;

    /// The same entry with a supervisor pid that is still around: a session
    /// genuinely stuck mid-turn — on a permission prompt, say — which stays
    /// live, attachable, and closed to a headless send.
    fn blocked_live_listing() -> String {
        format!(
            r#"[{{"sessionId": "ref-1", "state": "blocked", "pid": {}}}]"#,
            std::process::id()
        )
    }

    struct JumpIn {
        store: Store,
        ctx: crate::dispatch::DispatchCtx,
        task_id: i64,
        project_path: std::path::PathBuf,
        listing: std::path::PathBuf,
    }

    /// Rewrite the canned listing, moving the session between live and
    /// finished under a task whose state stays where it is.
    fn write_listing(path: &std::path::Path, json: &str) {
        std::fs::write(path, json).unwrap();
    }

    /// A project with one dispatched task, its session's ref recorded, and a
    /// canned `sessions` listing the test can rewrite. `verbs` names the
    /// session verbs the stub agent defines, so a test can take one away. The
    /// stub lingers after printing its prompt, so a verb-less agent — whose
    /// liveness is the pid — keeps its task `running` through reconcile.
    ///
    /// The `message` verb lingers too: a send that exits non-zero inside its
    /// grace window is a send that did not happen (task #390), so the stub has
    /// to be a command that survives. It says what it is in a trailing comment,
    /// which the launch log records verbatim — that is what the assertions
    /// below read the rendered `{session}` out of.
    fn jump_in_env(verbs: &[&str], listing_json: &str) -> JumpIn {
        let (mut store, ctx, project_path) = scratch_env("jumpin", None);
        let listing = project_path.parent().unwrap().join("listing.json");
        write_listing(&listing, listing_json);
        let templates = [
            ("sessions", format!("cat '{}'", listing.display())),
            ("attach", "agent attach {session}".into()),
            ("resume", "agent resume {session}".into()),
            (
                "message",
                "sleep 30 # agent message {session} {prompt_file}".into(),
            ),
        ]
        .into_iter()
        .filter(|(verb, _)| verbs.contains(verb))
        .map(|(verb, template)| format!("{verb} = \"{template}\"\n"))
        .collect::<String>();
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}} && sleep 30\"\n{templates}"
            ),
        )
        .unwrap();
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
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        let session_id = store.sessions_for(task.id).unwrap()[0].id;
        store.set_session_ref(session_id, "ref-1").unwrap();
        JumpIn {
            store,
            ctx,
            task_id: task.id,
            project_path,
            listing,
        }
    }

    /// Every session verb, the ordinary configuration.
    fn all_verbs() -> &'static [&'static str] {
        &["sessions", "attach", "resume", "message"]
    }

    /// `A` on a running task whose session is still listed queues the agent's
    /// `attach` command — ref substituted, project path as cwd — for main() to
    /// run with the TUI suspended.
    #[test]
    fn attach_key_prepares_the_attach_command_for_a_running_task() {
        let env = jump_in_env(all_verbs(), LIVE_LISTING);
        let project_path = env.project_path.clone();
        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");
        assert_eq!(request.cwd, project_path.to_str().unwrap());

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// `A` on a review task whose session has finished uses `resume` — the
    /// point is reopening it, not attaching to a live one.
    #[test]
    fn attach_key_uses_resume_for_a_finished_review_session() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The bug this key had (task #332): a `--bg` session commonly outlives
    /// the `running` state, and `resume` refuses a session the supervisor
    /// still holds. A review task whose session is listed live attaches.
    #[test]
    fn attach_key_attaches_to_a_review_tasks_live_session() {
        let mut env = jump_in_env(all_verbs(), LIVE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A review task whose entry is a pid-less zombie resumes: `blocked` with
    /// nothing behind it is not a claim that anything is still running, and
    /// attaching to it fails at the agent (task #376).
    #[test]
    fn attach_key_resumes_a_review_tasks_zombie_session() {
        let mut env = jump_in_env(all_verbs(), ZOMBIE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The same entry with a live pid is the case the pid rule protects: a
    /// stalled-but-alive session is attached to, not resumed out from under.
    #[test]
    fn attach_key_attaches_to_a_blocked_session_with_a_live_pid() {
        let mut env = jump_in_env(all_verbs(), &blocked_live_listing());
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// And the mirror: a running task whose session has died since the last
    /// refresh resumes rather than attaching to nothing.
    #[test]
    fn attach_key_resumes_a_running_tasks_finished_session() {
        let env = jump_in_env(all_verbs(), LIVE_LISTING);
        let (project_path, listing, task_id) =
            (env.project_path.clone(), env.listing.clone(), env.task_id);

        let mut app = App::new(env.store, env.ctx).unwrap();
        // the session ends after the refresh that left the task running
        write_listing(&listing, FINISHED_LISTING);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// An agent that cannot report liveness falls back to task state, exactly
    /// as before liveness was consulted — the listing is not read, even
    /// though this one would have said the session had finished.
    #[test]
    fn attach_key_without_a_sessions_verb_follows_task_state() {
        let env = jump_in_env(&["attach", "resume"], FINISHED_LISTING);
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let mut app = App::new(env.store, env.ctx).unwrap();
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// An agent defining only one of the two verbs jumps in with that one —
    /// the built-in `codex` has no `attach` — rather than refusing a live
    /// session it has a way into.
    #[test]
    fn attach_key_falls_back_to_the_verb_the_agent_defines() {
        let env = jump_in_env(&["sessions", "resume"], LIVE_LISTING);
        let project_path = env.project_path.clone();
        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// With neither verb defined there is no way in, and the message names
    /// both rather than only the one the state would have picked.
    #[test]
    fn attach_key_without_either_verb_reports_both() {
        let env = jump_in_env(&["sessions"], LIVE_LISTING);
        let project_path = env.project_path.clone();
        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no attach or resume template"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The verb choice itself, over the grid the App tests can only sample:
    /// liveness wins where it is known, state stands in where it is not, and
    /// either way the chosen verb degrades to the one the agent defines.
    #[test]
    fn jump_verb_prefers_liveness_then_state_then_availability() {
        let (a, r) = (Some("attach {session}"), Some("resume {session}"));
        assert_eq!(jump_verb(Some(true), JumpVerb::Resume, a, r), a);
        assert_eq!(jump_verb(Some(false), JumpVerb::Attach, a, r), r);
        assert_eq!(jump_verb(None, JumpVerb::Attach, a, r), a);
        assert_eq!(jump_verb(None, JumpVerb::Resume, a, r), r);
        // only one verb defined: take it whichever way the choice went
        assert_eq!(jump_verb(Some(true), JumpVerb::Attach, None, r), r);
        assert_eq!(jump_verb(Some(false), JumpVerb::Resume, a, None), a);
        assert_eq!(jump_verb(Some(true), JumpVerb::Attach, None, None), None);
    }

    /// The state fallback, and the gate on which states offer a jump-in at all.
    #[test]
    fn state_jump_verb_covers_the_three_jumpable_states() {
        assert_eq!(state_jump_verb(TaskState::Running), Some(JumpVerb::Attach));
        assert_eq!(state_jump_verb(TaskState::Review), Some(JumpVerb::Resume));
        assert_eq!(state_jump_verb(TaskState::Stalled), Some(JumpVerb::Resume));
        assert_eq!(state_jump_verb(TaskState::Ready), None);
        assert_eq!(state_jump_verb(TaskState::Done), None);
    }

    /// Without a captured ref there is nothing to substitute into the verb;
    /// the key explains instead of queuing a broken command. The fixture's
    /// verb-less stub agent also exercises the pid-reconcile path: the dead
    /// session is finalised and the task lands in `stalled` (DESIGN.md §6/§8),
    /// whose jump-in is `resume`.
    #[test]
    fn attach_key_without_a_captured_ref_reports_and_does_nothing() {
        let (mut store, ctx, project_path) = scratch_env(
            "jumpin-noref",
            Some(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
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
                repo_id: None,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        // the stub exits immediately; wait for it so App::new's
        // reconcile-on-read reliably finds the pid dead
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut app = App::new(store, ctx).unwrap();
        // the dead session's task is stalled by reconcile-on-read, so it
        // belongs to the queue, not the running strip
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Stalled);
        assert!(app.running.is_empty(), "{:?}", app.running);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

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
    fn attach_key_on_a_ready_task_reports_the_states_that_work() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('A'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("jump-in"),
            "{:?}",
            app.status
        );
    }

    // --- quick message into a task's session ---

    /// The gate on which states take a quick message: the ones whose session
    /// is open and between turns. `running`/`refining` are mid-turn and
    /// `stalled` is dead, so all three belong to `A` or to redispatch.
    #[test]
    fn state_accepts_message_covers_the_three_messageable_states() {
        for state in [TaskState::NeedsInput, TaskState::Review, TaskState::Waiting] {
            assert!(state_accepts_message(state), "{state}");
        }
        for state in [
            TaskState::Running,
            TaskState::Refining,
            TaskState::Stalled,
            TaskState::Ready,
            TaskState::Done,
        ] {
            assert!(!state_accepts_message(state), "{state}");
        }
    }

    /// Type a message into the quick-message input and submit it.
    fn send_message(app: &mut App, text: &str) {
        key(app, KeyCode::Char('a'));
        assert!(
            matches!(
                app.mode,
                Mode::Prompt {
                    kind: PromptKind::SessionMessage,
                    ..
                }
            ),
            "a should open the message input: {:?}",
            app.status
        );
        type_str(app, text);
        key(app, KeyCode::Enter);
    }

    /// The launch log, which records every command Voro spawns before it runs.
    /// Absent until something is spawned, which is itself an assertion a test
    /// wants to make.
    fn launches(root: &std::path::Path) -> String {
        std::fs::read_to_string(root.join("sessions").join("launches.log")).unwrap_or_default()
    }

    /// Every message to a review task is a reject-with-feedback: the
    /// transition runs first, so the feedback is in the body and the event log
    /// before the send, and the task is back on its agent in `running`. The
    /// agent here reports no `sessions` listing, so liveness is unknowable and
    /// the send proceeds — a missing signal is never a refusal.
    #[test]
    fn message_on_a_review_task_rejects_with_feedback_and_sends() {
        let mut env = jump_in_env(&["attach", "resume", "message"], FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        assert!(
            app.store
                .events_for(task_id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "feedback"),
            "the rejection is logged"
        );
        let log = launches(&root);
        assert!(log.contains("agent message 'ref-1'"), "{log}");
        // fire-and-forget: the TUI never suspends for it
        assert!(app.pending_attach.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `needs-input` task transitions not at all — per DESIGN.md §6 the
    /// answer lives in the transcript, and the agent's own `voro resume` moves
    /// the task back to `running`. Its session row still follows the send, so
    /// the answer's process is what the reconciler reads.
    #[test]
    fn message_on_a_needs_input_task_sends_without_transitioning() {
        let mut env = jump_in_env(&["attach", "resume", "message"], FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Ask("which crate?".into()))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        send_message(&mut app, "voro-core");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::NeedsInput);
        assert!(!task.body.contains("voro-core"), "{}", task.body);
        assert!(launches(&root).contains("agent message 'ref-1'"));
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        assert!(crate::session_probe::pid_is_alive(session.pid.unwrap()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rewrite the stub agent's `message` verb — loaded fresh on every send, so
    /// a test can change what a send *does* after the environment is built.
    fn set_message_verb(env: &JumpIn, template: &str) {
        let config = std::fs::read_to_string(&env.ctx.agents_path).unwrap();
        let rewritten = config
            .lines()
            .map(|line| match line.starts_with("message = ") {
                true => format!("message = \"{template}\"\n"),
                false => format!("{line}\n"),
            })
            .collect::<String>();
        std::fs::write(&env.ctx.agents_path, rewritten).unwrap();
    }

    /// The defect this ordering exists for (task #390): the send is what the
    /// rejection hangs off, so a message the agent refuses — a supervisor-held
    /// session, a stale reference — leaves the task in `review` with its body
    /// untouched and the refusal on the status line. Recording feedback the
    /// agent never received, and returning the task to `running` on the
    /// strength of it, is the one outcome worse than not sending.
    #[test]
    fn a_refused_send_leaves_the_review_task_exactly_where_it_was() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        set_message_verb(
            &env,
            "printf 'Session is currently running as a background agent' >&2; \
             exit 1 # {session} {prompt_file}",
        );
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Review);
        assert!(!task.body.contains("Feedback"), "{}", task.body);
        assert!(
            !task.body.contains("the tests are missing"),
            "{}",
            task.body
        );
        assert!(
            !app.store
                .events_for(task_id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "feedback"),
            "no rejection is logged for a message that never landed"
        );
        // the agent's own account of the refusal, out of the log and onto the
        // status line
        let status = app.status.as_deref().unwrap_or("").to_string();
        assert!(status.contains("background agent"), "{status}");
        assert!(status.contains("unchanged"), "{status}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A verb that forks rather than resuming in place: the session row follows
    /// the fork, so the next message and the next jump-in address the
    /// conversation where it actually continued — and the rejection lands as
    /// usual behind the confirmed send.
    #[test]
    fn a_forking_send_moves_the_session_to_the_reference_it_opened() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        set_message_verb(
            &env,
            "sleep 30 # agent message {session} --session-id {new_session} {prompt_file}",
        );
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        let new_ref = session.session_ref.expect("a reference");
        assert_ne!(new_ref, "ref-1", "the row followed the fork");
        assert!(crate::session_probe::pid_is_alive(session.pid.unwrap()));
        assert!(
            launches(&root).contains(&format!("--session-id '{new_ref}'")),
            "the recorded reference is the one the command was given"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session still running is mid-turn, so the headless send is refused
    /// and the operator is pointed at the terminal instead. Nothing is sent
    /// and — the part that matters — nothing is transitioned.
    #[test]
    fn message_refuses_a_session_that_is_still_running() {
        let mut env = jump_in_env(all_verbs(), LIVE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "one more thing");

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Review);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("still running"),
            "{:?}",
            app.status
        );
        assert!(!launches(&root).contains("agent message"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The refusal's other half (task #376): a pid-less `blocked` zombie is
    /// not a session still running, so the message goes headlessly — and the
    /// send that lands is what the task then rides on. The reconcile that
    /// follows finds the same zombie in the listing but the send's own process
    /// on the row, so the task stays `running` rather than being stalled out
    /// from under the agent now answering (task #390).
    #[test]
    fn message_sends_into_a_zombie_session() {
        let mut env = jump_in_env(all_verbs(), ZOMBIE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        assert_eq!(task.state, TaskState::Running);
        assert!(launches(&root).contains("agent message 'ref-1'"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same entry with a live pid keeps the refusal: the session is stuck
    /// mid-turn with its supervisor still there, so the operator wants `A`.
    #[test]
    fn message_refuses_a_blocked_session_with_a_live_pid() {
        let mut env = jump_in_env(all_verbs(), &blocked_live_listing());
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "one more thing");

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Review);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("still running"),
            "{:?}",
            app.status
        );
        assert!(!launches(&root).contains("agent message"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reject edge's tail, corrected (task #390): a session the listing
    /// reports finished used to be stalled by the very next reconcile, seconds
    /// after the rejection was sent into it — the operator's feedback recorded,
    /// the task queued for redispatch, and the agent that received it ignored.
    /// The send's own process is now on the row, so the task rides `running`
    /// for as long as the turn takes.
    #[test]
    fn message_to_a_finished_session_keeps_the_task_running() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        assert!(launches(&root).contains("agent message 'ref-1'"));
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        assert!(session.ended_at.is_none());
        assert!(
            crate::session_probe::pid_is_alive(session.pid.unwrap()),
            "the row carries the send's own process, not the dispatch launcher"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An agent with no `message` verb degrades per-verb: the key explains and
    /// names the one that still works, and no input opens.
    #[test]
    fn message_without_the_verb_reports_and_points_at_the_jump_in() {
        let mut env = jump_in_env(&["sessions", "attach", "resume"], FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('a'));

        assert!(matches!(app.mode, Mode::Normal));
        let status = app.status.as_deref().unwrap_or("").to_string();
        assert!(status.contains("no message template"), "{status}");
        assert!(status.contains('A'), "{status}");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// Without a captured reference there is nothing to resume into, so the
    /// key explains rather than opening an input that could not be sent.
    #[test]
    fn message_without_a_captured_ref_reports_and_opens_nothing() {
        // No `sessions` verb, so dispatch captures no reference at all.
        let (mut store, ctx, project_path) = scratch_env(
            "message-noref",
            Some(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {prompt_file}\"\n\
                 message = \"agent message {session} {prompt_file}\"\n",
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
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        store.apply(task.id, Action::Complete(None)).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Char('a'));

        assert!(matches!(app.mode, Mode::Normal));
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

    /// A task nobody ever dispatched has no session to say anything into.
    #[test]
    fn message_on_a_task_with_no_session_reports() {
        let mut app = app_with(&[TaskState::Review]);
        key(&mut app, KeyCode::Char('a'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no recorded session"),
            "{:?}",
            app.status
        );
    }

    // --- last-session surfacing and the log key (tasks #73/#110) ---

    /// Refresh captures each task's newest session, so the detail views
    /// render it without querying the store mid-draw.
    #[test]
    fn refresh_captures_a_stalled_tasks_last_session() {
        let app = app_with(&[TaskState::Stalled]);
        let task_id = app.queue_task_ids()[0];
        let session = app.last_sessions.get(&task_id).expect("a captured session");
        assert_eq!(session.outcome, Some(voro_core::SessionOutcome::Failed));
        assert!(session.ended_at.is_some());
        assert_eq!(session.log_path.as_deref(), Some("/tmp/demo/s.log"));
    }

    /// `l` on a stalled task queues `$PAGER <log>` for main() to run with the
    /// TUI suspended, in the project's checkout.
    #[test]
    fn log_key_pages_a_stalled_tasks_session_log() {
        let mut app = app_with(&[TaskState::Stalled]);
        key(&mut app, KeyCode::Char('l'));

        let request = app.pending_attach.clone().expect("a pager request");
        assert_eq!(request.command, "${PAGER:-less} '/tmp/demo/s.log'");
        assert_eq!(request.cwd, "/tmp/demo");
    }

    /// The key is not gated on state (task #110): a task whose session is
    /// still open — here parked mid-flight into needs-input — pages the same
    /// way, answering "what is this session doing?".
    #[test]
    fn log_key_pages_an_open_sessions_log_in_any_state() {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "mid-flight".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store
            .record_dispatch(task.id, "claude", Some(1), Some("/tmp/demo/open.log"))
            .unwrap();
        store.apply(task.id, Action::Ask("A or B?".into())).unwrap();

        let mut app = App::new(store, dummy_ctx()).unwrap();
        assert_eq!(
            app.store.task(task.id).unwrap().state,
            TaskState::NeedsInput
        );
        key(&mut app, KeyCode::Char('l'));

        let request = app.pending_attach.clone().expect("a pager request");
        assert_eq!(request.command, "${PAGER:-less} '/tmp/demo/open.log'");
        assert_eq!(request.cwd, "/tmp/demo");
    }

    /// `l` on a task nothing ever dispatched explains itself instead of
    /// paging nothing.
    #[test]
    fn log_key_on_a_ready_task_reports_and_does_nothing() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('l'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("no session"),
            "{:?}",
            app.status
        );
    }

    /// A stalled session that recorded no log path refuses with an
    /// explanation rather than handing the pager an empty argument.
    #[test]
    fn log_key_without_a_recorded_log_path_reports() {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "died without a log".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        let (_, session) = store
            .record_dispatch(task.id, "claude", Some(1), None)
            .unwrap();
        store.reconcile_session(session.id, false, false).unwrap();

        let mut app = App::new(store, dummy_ctx()).unwrap();
        key(&mut app, KeyCode::Char('l'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("no log path"),
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
                "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
                 [viewer]\ncmd = \"true\"\n",
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

    /// `g` is statically the GitHub medium (DESIGN.md §8): on a review task
    /// whose checkout cannot take a pull request it refuses on the status line
    /// naming `o`, opens no confirmation, and does not fall back to the
    /// project's viewer — which is what the viewer action here would have done
    /// before the split.
    #[test]
    fn review_key_on_a_non_github_checkout_refuses_naming_the_viewer_key() {
        let dir = std::env::temp_dir().join(format!(
            "voro-review-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", dir.to_str().unwrap()).unwrap();
        store.set_viewer(project.id, Some("zed")).unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "reviewable".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        store.set_branch(task.id, Some("feat/thing")).unwrap();
        store.apply(task.id, Action::Start).unwrap();
        // PR-ready in every way but the checkout, so the refusal can only be
        // about the medium — the plan's own gaps are checked first.
        store
            .apply(task.id, Action::Complete(Some("did it".into())))
            .unwrap();

        let mut app = App::new(store, dummy_ctx()).unwrap();
        key(&mut app, KeyCode::Char('g'));

        let status = app.status.as_deref().unwrap_or("");
        assert!(matches!(app.mode, Mode::Normal), "{status:?}");
        assert!(status.contains("`o`"), "{status:?}");
        std::fs::remove_dir_all(&dir).ok();
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

    /// Confirming the create-PR modal shows the new PR without a second `g`:
    /// the browser is launched with the URL `create` just recorded (DESIGN.md
    /// §8). The launch is passed in so the chain is exercised without `gh`.
    #[test]
    fn a_created_pr_is_opened_in_the_browser() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        let url = "https://github.com/acme/widget/pull/7";
        let mut opened = None;
        app.report_created_pr(task_id, Ok(url.to_string()), |u| {
            opened = Some(u.to_string());
            Ok(format!("opening {u} in the browser"))
        });
        assert_eq!(opened.as_deref(), Some(url));
        let status = app.status.as_deref().unwrap_or("");
        assert!(
            status.contains(url) && status.contains("browser"),
            "{status}"
        );
    }

    /// A browser that will not launch does not turn a created PR into a
    /// failure: the URL is recorded, so it is reported with the open error
    /// alongside it (DESIGN.md §8).
    #[test]
    fn a_browser_failure_after_a_create_still_reports_the_pr() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        let url = "https://github.com/acme/widget/pull/7";
        app.report_created_pr(task_id, Ok(url.to_string()), |_| {
            Err("cannot run `gh` to open the PR: not found".to_string())
        });
        let status = app.status.as_deref().unwrap_or("");
        assert!(
            status.contains("PR created") && status.contains(url) && status.contains("not found"),
            "{status}"
        );
    }

    /// A failed create is reported as-is and never reaches the browser.
    #[test]
    fn a_failed_create_does_not_open_a_browser() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        let mut opened = false;
        app.report_created_pr(task_id, Err("`gh pr create` failed".to_string()), |_| {
            opened = true;
            Ok(String::new())
        });
        assert!(!opened, "the browser was launched for a failed create");
        assert_eq!(app.status.as_deref(), Some("`gh pr create` failed"));
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
                .contains("only ready or stalled tasks can be dispatched"),
            "{:?}",
            app.status
        );
    }

    // --- planning sessions (task #112) ---

    /// An app whose dispatch context reads a scratch `voro.toml`, so the
    /// planning keys resolve a known agent instead of the developer's real
    /// config and PATH.
    fn app_with_agents(agents_toml: &str) -> App {
        let mut app = app_with(&[TaskState::Ready]);
        let dir = std::env::temp_dir().join(format!(
            "voro-plan-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let agents_path = dir.join("voro.toml");
        std::fs::write(&agents_path, agents_toml).unwrap();
        app.dispatch_ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        app
    }

    /// `N` with a single project launches the planning session directly: the
    /// assembled command lands in `pending_plan` for main() to run with the
    /// terminal suspended, and no store write has happened.
    #[test]
    fn plan_key_queues_the_planning_session() {
        let mut app = app_with_agents(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        key(&mut app, KeyCode::Char('N'));

        assert!(matches!(app.mode, Mode::Normal));
        let launch = app.pending_plan.take().expect("a planning session queued");
        assert!(
            launch.command.starts_with("stub --interactive "),
            "{}",
            launch.command
        );
        assert_eq!(launch.cwd, "/tmp/demo");
    }

    /// `N` when the resolved agent defines no `plan` verb degrades to a status
    /// explaining what to configure — no session, no crash.
    #[test]
    fn plan_key_reports_a_missing_plan_verb() {
        let mut app = app_with_agents(
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n",
        );
        key(&mut app, KeyCode::Char('N'));

        assert!(app.pending_plan.is_none());
        let status = app.status.as_deref().unwrap_or("");
        assert!(status.contains("plan"), "{status}");
        assert!(status.contains("stub"), "{status}");
    }

    /// With several projects `N` opens the same project picker as `n`, marked
    /// with the planning flow; Enter launches the session for the picked
    /// project.
    #[test]
    fn plan_key_routes_through_the_project_picker() {
        let mut app = app_with_agents(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        app.store.create_project("second", "/tmp/second").unwrap();
        app.refresh().unwrap();

        key(&mut app, KeyCode::Char('N'));
        assert!(matches!(
            app.mode,
            Mode::PickProject {
                flow: CreateFlow::Plan,
                ..
            }
        ));

        key(&mut app, KeyCode::Enter);
        let launch = app.pending_plan.take().expect("a planning session queued");
        assert_eq!(launch.cwd, "/tmp/demo");
    }

    /// A review task with a tracked PR, selected in the cockpit — the only
    /// selection the stale-branch probe has anything to say about.
    fn app_with_review_pr() -> App {
        let mut app = app_with(&[TaskState::Review]);
        let id = app.selected_task_id().expect("the review task is selected");
        app.store
            .set_pr(id, Some("https://github.com/o/r/pull/1"))
            .unwrap();
        app.refresh().unwrap();
        app
    }

    /// Landing on a review task starts no probe on the tick it arrives
    /// (DESIGN.md §8): the selection has not rested yet, so scrolling past the
    /// row costs neither a `gh` call nor a thread.
    #[test]
    fn a_fresh_selection_starts_no_probe() {
        let mut app = app_with_review_pr();
        app.poll_conflict_probe();
        assert_eq!(app.probe.in_flight(), None);
        assert_eq!(app.conflict_selected, None);
    }

    /// A verdict landing while its task is still selected fills the marker.
    #[test]
    fn a_conflicting_verdict_marks_the_selected_task() {
        let mut app = app_with_review_pr();
        let id = app.selected_task_id().unwrap();
        app.probe
            .inject_result(id, voro_core::Mergeability::Conflicting);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, Some((id, true)));

        // A clean verdict is held too, so the row is not probed again.
        app.conflict_selected = None;
        app.probe
            .inject_result(id, voro_core::Mergeability::Mergeable);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, Some((id, false)));
    }

    /// A verdict that arrives after the selection has moved on is discarded,
    /// never shown against whatever is selected now.
    #[test]
    fn a_verdict_for_an_unselected_task_is_discarded() {
        let mut app = app_with_review_pr();
        let id = app.selected_task_id().unwrap();
        app.probe
            .inject_result(id + 1, voro_core::Mergeability::Conflicting);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, None);
        assert_eq!(app.probe.in_flight(), None);
    }

    /// A captured revision reaches the store whatever is selected by the time
    /// it lands (DESIGN.md §8) — the rejection that started it has already
    /// moved its task on to `running`.
    #[test]
    fn a_captured_revision_is_recorded_against_its_task() {
        let mut app = app_with(&[TaskState::Review, TaskState::Ready]);
        let id = app.selected_task_id().unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        app.capture.inject_result(id, Some(sha));
        app.move_selection(1);

        app.poll_reviewed_capture();
        assert_eq!(app.store.last_reviewed(id).unwrap(), Some(sha.to_string()));
    }

    /// Moving off the row drops its verdict, so nothing stale is rendered and
    /// re-selecting it probes afresh.
    #[test]
    fn moving_the_selection_drops_the_verdict() {
        let mut app = app_with(&[TaskState::Review, TaskState::Ready]);
        let id = app.selected_task_id().unwrap();
        app.conflict_selected = Some((id, true));
        app.move_selection(1);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, None);
    }

    #[test]
    fn a_refresh_loads_every_task_s_documents_and_where_they_resolve_to() {
        // The render path never queries the store, so both the links and the
        // resolved locations the detail panes show are loaded per refresh.
        let mut app = app_with(&[TaskState::Ready]);
        let task_id = app.all[0].task.id;
        let project_id = app.projects[0].id;
        let doc = app
            .store
            .create_doc(project_id, None, "docs/plan.md", Some("The Plan"))
            .unwrap();
        let url = app
            .store
            .create_doc(project_id, None, "https://example.com/rfc", None)
            .unwrap();
        app.store.set_task_docs(task_id, &[doc.id, url.id]).unwrap();
        app.refresh().unwrap();

        let linked = &app.docs[&task_id];
        assert_eq!(linked.len(), 2);
        assert_eq!(linked[0].label(), "The Plan");
        // A relative location is joined onto the project's checkout; a URL is
        // already where it is.
        assert_eq!(app.doc_locations[&doc.id], "/tmp/demo/docs/plan.md");
        assert_eq!(app.doc_locations[&url.id], "https://example.com/rfc");

        // Unlinking is reflected on the next refresh, and a task citing no
        // document has no entry at all rather than an empty one.
        app.store.set_task_docs(task_id, &[]).unwrap();
        app.refresh().unwrap();
        assert!(!app.docs.contains_key(&task_id));
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

        key(&mut app, KeyCode::Char('2'));
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
