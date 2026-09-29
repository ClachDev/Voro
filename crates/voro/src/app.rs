use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod cockpit;
mod config;
pub(crate) mod milestones;
mod modes;
mod probes;
mod projects;
mod rows;
mod sessions;
mod tasks;
mod tree;

pub use config::{DefaultKind, ViewerFormState, ViewerOption, viewer_label};
pub use modes::{Mode, PromptKind};
pub use rows::{
    BrowserRow, CockpitRow, ConfigAgentRow, ConfigRow, ConfigSettingRow, ConfigViewerRow,
    SettingKind, TaskRow,
};
pub(crate) use tree::blocks_edges;

pub use crate::dispatch::Filing;
use crate::ui::Hit;
use probes::CapTarget;
use voro_core::{
    Action, AgentsConfig, DepKind, DepRef, Project, Queue, QueueRow, RunningRow, StateCounts,
    Store, Task, TaskState, Triage, WipGate, scheduler,
};

/// Lines `PgDn`/`PgUp` move the focus card in one press. A fixed step, since
/// the key handler runs without the pane's geometry.
const DETAIL_PAGE_STEP: i64 = 10;

/// `n`'s refusal with no project registered, in the keys README.md teaches.
/// The gate (DESIGN.md §9) keeps the operator on the two screens `n` is not
/// bound on, so this is a defensive path rather than one they can reach.
pub const NO_PROJECTS_HINT: &str = "no projects yet — press tab to Projects, then a to add one";

/// `n`'s refusal when every registered project is archived. An archived project
/// refuses new work (DESIGN.md §5), so there is nothing to pick; unarchiving is
/// the projects screen's job, which is where this points.
pub const ALL_PROJECTS_ARCHIVED_HINT: &str =
    "every project is archived — press tab to Projects, then A to unarchive one";

/// How a gated screen jump refuses (DESIGN.md §9). It names `alt-3` rather than
/// `tab` because the jump is the shortest route from either screen the gate
/// leaves reachable, and it lands on Projects from both.
pub const NO_PROJECTS_JUMP_HINT: &str = "no projects yet — press alt-3, then a to add one";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Cockpit,
    Tasks,
    Projects,
    Config,
    Milestones,
}

#[cfg(test)]
impl Screen {
    /// Every screen, in Tab-ring and alt-digit order (DESIGN.md §9).
    pub const ALL: [Screen; 5] = [
        Screen::Cockpit,
        Screen::Tasks,
        Screen::Projects,
        Screen::Config,
        Screen::Milestones,
    ];
}

/// Which create flow the project picker feeds (DESIGN.md §8/§9). Three paths
/// reach a task, and the case convention orders them: `n` collects one line in
/// a modal and hands it to a background agent that writes the task and files it,
/// `N` opens the interactive planning session, and `ctrl-n` — the rare path, and
/// the only one that sets state, priority and dependencies at creation time —
/// opens the manual `$EDITOR` form. Each carries what it files: the Milestones
/// tab's three keys file a milestone, every other screen's a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateFlow {
    Quick(Filing),
    Editor(Filing),
    Plan(Filing),
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
    Create { project_id: i64, filing: Filing },
    Edit { task_id: i64 },
}

/// A request for main() to suspend the terminal and run an agent's
/// `attach`/`resume` command in the foreground — a full-screen
/// interactive program that owns the terminal until the user detaches.
#[derive(Debug, Clone)]
pub struct AttachRequest {
    /// The verb template with `{session}` already substituted.
    pub command: String,
    /// The project checkout to run it in.
    pub cwd: String,
}

/// The transition menu for a task: the machine's legal actions, or on a
/// milestone the two verdicts the TUI offers it (DESIGN.md §6).
fn transition_actions(task: &Task) -> Vec<Action> {
    if task.milestone {
        task.milestone_actions()
    } else {
        Store::legal_actions(task.state, task.human)
    }
}

/// [`action_label`] for a task's menu: on a milestone `Complete` is done,
/// which [`Store::close_milestone`] reaches in one step.
pub fn transition_label(action: &Action, milestone: bool) -> &'static str {
    match action {
        Action::Complete(_) if milestone => "done → done",
        _ => action_label(action),
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
    /// The next-action queue (DESIGN.md §7), ranked by score and carrying the
    /// dispatch gate's state when it is suppressing rows.
    pub queue: Queue,
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
    /// detail views render as `blocked by #N` / `blocks #N`.
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
    /// Each task's newest session, keyed by task id: what the
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
    /// The background threads pushing branches and opening pull requests for
    /// confirmed creates (DESIGN.md §8), drained by `poll_pr_create`.
    pr_create: crate::probe::PrCreate,
    /// Which in-flight sessions can be read for a usage cap: task id, the
    /// session reference to read, and the agent's `logs` command. Resolved on
    /// refresh, where the agents config is already loaded, so the tick that
    /// starts a probe does no I/O of its own to decide what to probe.
    cap_targets: Vec<CapTarget>,
    /// Which in-flight tasks are sitting on a usage cap right now, and when
    /// each window reopens if the agent said (DESIGN.md §8). Purely a reading
    /// of current session output — no column, no event, no state change — so it
    /// clears itself once the operator continues the session and fresh output
    /// displaces the cap message.
    pub caps: std::collections::HashMap<i64, voro_core::CapReading>,
    /// The background threads taking those readings, drained by
    /// `poll_cap_probes`.
    cap_probe: crate::probe::CapProbe,
    /// What each agent's *account* says about when its window reopens
    /// (DESIGN.md §8), for the agents with a badged session on the strip. Held
    /// per agent rather than per task because a cap belongs to the account: one
    /// reading answers for every session running under it, and an agent Voro
    /// cannot ask simply has none.
    account_caps: std::collections::HashMap<String, voro_core::AccountCap>,
    /// The background threads taking those readings, drained beside the
    /// per-session ones.
    account_probe: crate::probe::AccountCapProbe,
    /// The local wall clock as minutes past midnight, for deciding whether a
    /// badged reset time has gone by. Refreshed on a slow cadence rather than
    /// per frame: reading it costs a subprocess, and a badge that flips from
    /// "waiting" to "window open" within half a minute is timely enough.
    pub now_minutes: Option<u16>,
    /// The same moment as seconds since the Unix epoch, which is what an
    /// agent's own reset instant is compared against. Costs no subprocess, and
    /// is refreshed beside `now_minutes` so the two halves of a badge cannot
    /// disagree about when now is.
    pub now_epoch: Option<i64>,
    /// When `now_minutes` was last read.
    clock_read_at: Option<std::time::Instant>,

    /// Every milestone with its members (DESIGN.md §3), closed ones included,
    /// in the Milestones tab's order: ready, parked, then closed.
    pub milestones: Vec<voro_core::MilestoneMembers>,
    /// Each task's nearest milestones by id, for the cockpit column and the
    /// detail panes. Loaded per refresh from one read of the graph.
    pub milestone_of: std::collections::HashMap<i64, Vec<i64>>,
    pub milestones_sel: usize,
    /// Whether the task browser groups by milestone (`M`), and which folds are
    /// open; `None` is the unattached fold. Both held across refreshes.
    pub browse_by_milestone: bool,
    pub open_groups: std::collections::HashSet<Option<i64>>,
    /// Whether the task browser shows the blocker tree (`t`), and which of
    /// its folds are open, by the task heading each. Both held across
    /// refreshes; the tree and milestone grouping exclude each other.
    pub browse_tree: bool,
    pub open_folds: std::collections::HashSet<i64>,
    /// The browser's blocker tree with every fold open, rebuilt per refresh,
    /// and the counts of the tasks it leaves out: those with no edges, and
    /// the closed tasks heading a tree.
    pub tree_rows: Vec<voro_core::TreeRow>,
    pub tree_no_edges: usize,
    pub tree_closed: usize,
    /// Each task's index in `all`, by id.
    pub all_index: std::collections::HashMap<i64, usize>,
    /// The task browser's rows, which `tasks_sel` counts in.
    pub browser_rows: Vec<BrowserRow>,

    pub cockpit_rows: Vec<CockpitRow>,
    pub cockpit_sel: usize,
    pub tasks_sel: usize,
    pub projects_sel: usize,

    /// The Config screen's view of `voro.toml` (DESIGN.md §5), reloaded every
    /// refresh so an edit — from either this screen or a dispatch — is reflected
    /// immediately. Agents are read-only; the settings and the viewers are what
    /// `config_sel` runs over, through the flat `config_rows`, and ⏎ edits
    /// whichever it lands on — a built-in viewer row is selectable but refuses.
    pub config_agents: Vec<ConfigAgentRow>,
    pub config_settings: Vec<ConfigSettingRow>,
    pub config_viewers: Vec<ConfigViewerRow>,
    /// The settings and the viewers as one selectable list, which is the space
    /// `config_sel` counts in.
    pub config_rows: Vec<ConfigRow>,
    /// The legacy anonymous `[viewer]` table's command, shown read-only.
    pub config_anon_viewer: Option<String>,
    /// A `voro.toml` that failed to parse, surfaced on the screen rather than
    /// silently rendering an empty config.
    pub config_error: Option<String>,
    /// What the loaded `voro.toml` carries that does nothing, one sentence
    /// each — shown on the Config screen and once at startup.
    pub config_warnings: Vec<String>,
    pub config_sel: usize,
    /// Vertical scroll offset of the Config screen's agents pane (DESIGN.md §9),
    /// driven by `J`/`K` and `PgDn`/`PgUp`. The pane carries no selection of its
    /// own — `j`/`k` belong to the viewers list below it — so the scroll is the
    /// only way past the fold on a terminal too short for every agent.
    pub config_agents_scroll: u16,
    /// The largest useful `config_agents_scroll` for the pane as last rendered,
    /// recorded by `draw_config` for the same reason as `detail_max_scroll`.
    pub config_agents_max_scroll: std::cell::Cell<u16>,

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
pub(crate) fn browse_order(state: TaskState) -> u8 {
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
                tied_cut: None,
            },
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
            pr_create: crate::probe::PrCreate::default(),
            cap_targets: Vec::new(),
            caps: std::collections::HashMap::new(),
            cap_probe: crate::probe::CapProbe::default(),
            account_caps: std::collections::HashMap::new(),
            account_probe: crate::probe::AccountCapProbe::default(),
            now_minutes: None,
            now_epoch: None,
            clock_read_at: None,
            milestones: Vec::new(),
            milestone_of: std::collections::HashMap::new(),
            milestones_sel: 0,
            browse_by_milestone: false,
            open_groups: std::collections::HashSet::new(),
            browse_tree: false,
            open_folds: std::collections::HashSet::new(),
            tree_rows: Vec::new(),
            tree_no_edges: 0,
            tree_closed: 0,
            all_index: std::collections::HashMap::new(),
            browser_rows: Vec::new(),
            cockpit_rows: Vec::new(),
            cockpit_sel: 0,
            tasks_sel: 0,
            projects_sel: 0,
            config_agents: Vec::new(),
            config_settings: Vec::new(),
            config_viewers: Vec::new(),
            config_rows: Vec::new(),
            config_anon_viewer: None,
            config_error: None,
            config_warnings: Vec::new(),
            config_sel: 0,
            config_agents_scroll: 0,
            config_agents_max_scroll: std::cell::Cell::new(0),
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
        } else if let Some(warning) = app.config_warnings.first() {
            app.status = Some(warning.clone());
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

    /// The database this run opened (DESIGN.md §5), for the footer to name it
    /// when it is not the operator's own store (§9).
    pub fn db_path(&self) -> &std::path::Path {
        &self.dispatch_ctx.db_path
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
        crate::reconcile::reconcile_live_sessions(&mut self.store, &self.dispatch_ctx)?;

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
        // Only the rows that actually advertise `pr` ask the forge question, so
        // a review task with nothing to push — it advertises `accept`
        // (DESIGN.md §6) — costs no `git remote`.
        self.local_review = all
            .iter()
            .filter(|r| r.task.next_action() == Some(voro_core::NextAction::Pr))
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
        self.all_index = all
            .iter()
            .enumerate()
            .map(|(i, r)| (r.task.id, i))
            .collect();
        self.all = all;
        let tree = self.build_tree();
        self.tree_rows = tree.rows;
        self.tree_no_edges = tree.no_edges;
        self.tree_closed = tree.closed_trees;
        self.load_milestones()?;
        self.running = self.store.running_rows()?;
        self.counts = self.store.state_counts()?;

        // The queue is gated on how much is already in flight (DESIGN.md §7).
        // A `voro.toml` that will not parse falls back to the default cap here
        // rather than emptying the cockpit — the Config screen is where the
        // error is surfaced.
        let config = AgentsConfig::load(&self.dispatch_ctx.agents_path);
        let gate = WipGate {
            running: self.counts.running,
            max_running: config
                .as_ref()
                .map_or(scheduler::DEFAULT_MAX_RUNNING, |c| c.max_running()),
        };
        self.queue = scheduler::queue(&candidates, gate);
        self.cap_targets = self.resolve_cap_targets(config.as_ref().ok());

        self.cockpit_rows = self.build_cockpit_rows();

        self.load_config_view(config);

        self.cockpit_sel = self
            .cockpit_sel
            .min(self.cockpit_rows.len().saturating_sub(1));
        self.tasks_sel = self
            .tasks_sel
            .min(self.browser_rows.len().saturating_sub(1));
        self.milestones_sel = self
            .milestones_sel
            .min(self.milestones.len().saturating_sub(1));
        self.projects_sel = self.projects_sel.min(self.projects.len().saturating_sub(1));
        self.config_sel = self
            .config_sel
            .min(self.config_rows.len().saturating_sub(1));
        Ok(())
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
            Screen::Tasks => {
                let i = self.browser_rows.get(self.tasks_sel)?.task_index()?;
                Some(self.all.get(i)?.task.id)
            }
            Screen::Milestones => Some(self.milestones.get(self.milestones_sel)?.milestone.id),
            Screen::Projects | Screen::Config => None,
        }
    }

    pub fn move_selection(&mut self, delta: i64) {
        let (sel, len) = match self.screen {
            Screen::Cockpit => (&mut self.cockpit_sel, self.cockpit_rows.len()),
            Screen::Tasks => (&mut self.tasks_sel, self.browser_rows.len()),
            Screen::Projects => (&mut self.projects_sel, self.projects.len()),
            Screen::Config => (&mut self.config_sel, self.config_rows.len()),
            Screen::Milestones => (&mut self.milestones_sel, self.milestones.len()),
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
            Screen::Tasks => (&mut self.tasks_sel, self.browser_rows.len()),
            Screen::Projects => (&mut self.projects_sel, self.projects.len()),
            Screen::Config => (&mut self.config_sel, self.config_rows.len()),
            Screen::Milestones => (&mut self.milestones_sel, self.milestones.len()),
        };
        if index >= len {
            return;
        }
        *sel = index;
        self.detail_scroll = 0;
    }

    /// Tab cycles cockpit → tasks → projects → config → milestones → cockpit;
    /// `alt-1` to `alt-5` jump directly (DESIGN.md §9). Until a project is
    /// registered the ring is the shorter Projects ↔ Config, the cockpit, the
    /// browser and the milestones having nothing to show and nothing to do
    /// until one exists; a screen off that ring enters it at Projects, where
    /// the first step is.
    pub fn toggle_screen(&mut self) {
        if self.projects.is_empty() {
            self.screen = match self.screen {
                Screen::Projects => Screen::Config,
                _ => Screen::Projects,
            };
            return;
        }
        self.screen = match self.screen {
            Screen::Cockpit => Screen::Tasks,
            Screen::Tasks => Screen::Projects,
            Screen::Projects => Screen::Config,
            Screen::Config => Screen::Milestones,
            Screen::Milestones => Screen::Cockpit,
        };
    }

    /// Take a direct screen jump, or refuse it while the target is gated behind
    /// having a project (DESIGN.md §9), in Voro's usual shape for a refusal: no
    /// move, and a status line naming the key to press instead.
    fn jump_to_screen(&mut self, screen: Screen) {
        if self.projects.is_empty()
            && matches!(screen, Screen::Cockpit | Screen::Tasks | Screen::Milestones)
        {
            self.status = Some(NO_PROJECTS_JUMP_HINT.into());
            return;
        }
        self.screen = screen;
    }

    /// The primary action of the current selection. On the Tasks screen every
    /// row opens its detail view. On the cockpit — where the detail pane
    /// already shows the body — a needs-input task resumes directly (the
    /// operator has answered the question in the agent's own session, DESIGN.md
    /// §6/§8) and any other task opens its transition menu.
    fn activate_selection(&mut self) {
        if self.screen == Screen::Tasks {
            if let Some(BrowserRow::Group(group)) = self.browser_rows.get(self.tasks_sel) {
                self.toggle_group(*group);
            } else if let Some(task_id) = self.selected_task_id() {
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
                let actions = transition_actions(task);
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
            // Every row with an editor behind it; a built-in viewer has none —
            // it is overridden by `a`, not edited.
            Screen::Config => match self.selected_config_row()? {
                ConfigRow::Setting(_) => Some("⏎ edit"),
                ConfigRow::Viewer(i) => self
                    .config_viewers
                    .get(i)
                    .filter(|v| v.editable)
                    .map(|_| "⏎ edit"),
            },
            Screen::Tasks => match self.browser_rows.get(self.tasks_sel)? {
                BrowserRow::Group(group) if self.open_groups.contains(group) => Some("⏎ collapse"),
                BrowserRow::Group(_) => Some("⏎ expand"),
                BrowserRow::Task(_) | BrowserRow::Node { .. } => Some("⏎ view"),
            },
            Screen::Milestones => {
                self.milestones
                    .get(self.milestones_sel)
                    .map(|m| match m.milestone.state {
                        TaskState::Proposed => "⏎ triage",
                        _ => "⏎ browse",
                    })
            }
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
    /// that same session — Voro only moves the state (DESIGN.md §6/§8). A
    /// transition that *closes* the session stops it too, so the agent's own
    /// listing loses the entry along with the row (§8); that is fire-and-forget
    /// and never reported, since a stop the operator did not ask for has nothing
    /// useful to say on the status line.
    fn apply_and_refresh(&mut self, task_id: i64, action: Action) {
        let rejected = matches!(action, Action::RejectWork(_));
        let result = match self.store.apply_closing(task_id, action) {
            Ok((task, stopped)) => {
                if let Some(session) = stopped {
                    crate::dispatch::stop_closed_session(&self.dispatch_ctx, &session);
                }
                Ok(task)
            }
            Err(e) => Err(e),
        };
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
            Mode::EditMaxRunning { buffer } => self.key_max_running(key, buffer),
            Mode::QuickCreate {
                project_id,
                filing,
                buffer,
            } => self.key_quick_create(key, project_id, filing, buffer),
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
            Mode::MilestonePicker {
                task_id,
                milestones,
                sel,
                back,
            } => self.key_milestone_picker(key, task_id, milestones, sel, back),
            Mode::ViewerPicker {
                project_id,
                options,
                current,
                sel,
            } => self.key_viewer_picker(key, project_id, options, current, sel),
            Mode::ViewerForm(form) => self.key_viewer_form(key, form),
            // `tab` turns the page; every other key dismisses the map, and
            // `on_key` has already restored `Mode::Normal` for it.
            Mode::KeyMap { page } => {
                if key.code == KeyCode::Tab {
                    self.mode = Mode::KeyMap { page: page + 1 };
                }
            }
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
            Hit::ConfigRow(i) if self.screen == Screen::Config => self.select_index(i),
            Hit::MilestoneRow(i) if self.screen == Screen::Milestones => self.select_index(i),
            Hit::PickerOption(i) => self.click_picker_option(i),
            _ => {}
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
                self.mode = Mode::KeyMap { page: 0 };
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
        // Direct screen jumps carry the modifier (DESIGN.md §9), which leaves
        // the bare digits to the numbers on the selected row. The arm sits ahead
        // of the two screens that handle their own keys below, so the jumps
        // reach every screen — and so this is the only place the gate's refusal
        // can fire, `tab` merely cycling a shorter ring.
        if key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char('1') => {
                    self.jump_to_screen(Screen::Cockpit);
                    return;
                }
                KeyCode::Char('2') => {
                    self.jump_to_screen(Screen::Tasks);
                    return;
                }
                KeyCode::Char('3') => {
                    self.jump_to_screen(Screen::Projects);
                    return;
                }
                KeyCode::Char('4') => {
                    self.jump_to_screen(Screen::Config);
                    return;
                }
                KeyCode::Char('5') => {
                    self.jump_to_screen(Screen::Milestones);
                    return;
                }
                _ => {}
            }
        }
        // The projects screen's digits are the selected project's weight, so its
        // handler gets first refusal before the priority arm below.
        if self.screen == Screen::Projects {
            self.key_projects(key);
            return;
        }
        // The Config screen has its own letter actions that would collide with
        // the global ones (`a`, `d`, `e`), so it too intercepts before the match
        // below; it binds no digits at all.
        if self.screen == Screen::Config {
            self.key_config(key);
            return;
        }
        // The Milestones tab binds its own few keys and no others, so its
        // list and its keys can move to another screen as a unit.
        if self.screen == Screen::Milestones {
            self.key_milestones(key);
            return;
        }
        // A bare digit sets the number on the selected row, which on the cockpit
        // and the task browser is the task's priority — the binding the detail
        // popup has always had (DESIGN.md §9). `4`/`5` are the digits the
        // projects screen's weights reach and priority does not.
        if !key.modifiers.contains(KeyModifiers::ALT) {
            match key.code {
                KeyCode::Char(c @ '0'..='3') => {
                    self.set_selected_priority(c);
                    return;
                }
                KeyCode::Char('4' | '5') => {
                    self.status = Some("priority is P0–P3".into());
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let result = self.refresh();
                self.report(result);
            }
            KeyCode::Char('r') => self.refine_selected(RefineFlow::Note),
            KeyCode::Char('R') => self.refine_selected(RefineFlow::Interactive),
            KeyCode::Enter => self.activate_selection(),
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.new_task(CreateFlow::Editor(Filing::Task))
            }
            KeyCode::Char('n') => self.new_task(CreateFlow::Quick(Filing::Task)),
            KeyCode::Char('N') => self.new_task(CreateFlow::Plan(Filing::Task)),
            KeyCode::Char('e') => {
                if let Some(id) = self.selected_task_id() {
                    self.pending_editor = Some(EditorRequest::Edit { task_id: id });
                }
            }
            KeyCode::Char('s') => {
                if let Some(task) = self.selected_task() {
                    let actions = transition_actions(task);
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
            // Scroll the focus card body: `j`/`k` already move the row
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
            KeyCode::Char('m') => {
                if let Some(id) = self.selected_task_id() {
                    self.open_milestone_picker(id, None);
                }
            }
            // `M` is not a variant of `m`: grouping the browser and attaching
            // a task merely share a letter (DESIGN.md §9).
            KeyCode::Char('M') if self.screen == Screen::Tasks => self.toggle_browse_by_milestone(),
            KeyCode::Char('t') if self.screen == Screen::Tasks => self.toggle_browse_tree(),
            KeyCode::Char(' ') if self.screen == Screen::Tasks => self.toggle_selected_fold(),
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
            KeyCode::Char('u') => self.nudge_capped(),
            KeyCode::Char('l') => self.view_session_log(),
            KeyCode::Char('w') => self.hand_off_selected(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tasks::tests::select_proposal;
    use voro_core::{LivenessSource, NewTask, Priority};

    pub(super) fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    pub(super) fn ctrl_key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::CONTROL));
    }

    pub(super) fn alt_key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::ALT));
    }

    /// A `DispatchCtx` that is never actually used to spawn anything in these
    /// tests — the transitions they drive (`resume`, reject) only move state.
    pub(super) fn dummy_ctx() -> crate::dispatch::DispatchCtx {
        crate::dispatch::DispatchCtx::without_config(std::path::Path::new("/nonexistent/voro.db"))
    }

    /// A store with one project and one task per requested state, reached
    /// through the real transition machine.
    pub(super) fn app_with(states: &[TaskState]) -> App {
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
                    milestone: false,
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
                        .record_dispatch(task.id, "claude", None, LivenessSource::Pid, None)
                        .unwrap();
                    store.apply(task.id, Action::Complete(None)).unwrap();
                    store.apply(task.id, Action::HandOff).unwrap();
                }
                // A dispatch that died: reconcile records the outcome and
                // stalls the task (DESIGN.md §8).
                TaskState::Stalled => {
                    let (_, session) = store
                        .record_dispatch(
                            task.id,
                            "claude",
                            Some(1),
                            LivenessSource::Pid,
                            Some("/tmp/demo/s.log"),
                        )
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
                            LivenessSource::Pid,
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

    /// Register a project in an app that had none and reload, so a test can
    /// watch the gate lift on the very app it watched hold.
    fn register_a_project(app: &mut App) {
        app.store.create_project("voro", "/tmp/voro").unwrap();
        app.refresh().unwrap();
    }

    /// The gate (DESIGN.md §9): until a project exists `tab` cycles the two
    /// screens that work without one, never landing on the cockpit or the
    /// browser.
    #[test]
    fn tab_cycles_only_projects_and_config_until_a_project_exists() {
        let mut app = empty_app();
        assert_eq!(app.screen, Screen::Projects);
        for _ in 0..6 {
            key(&mut app, KeyCode::Tab);
            assert!(
                matches!(app.screen, Screen::Projects | Screen::Config),
                "tab reached {:?} with no projects",
                app.screen
            );
        }

        register_a_project(&mut app);
        app.screen = Screen::Projects;
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Config);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Milestones);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Cockpit);
    }

    /// The gate's only refusal: the alt-digit jumps to the cockpit, the
    /// browser and the milestones no-op and say where to go instead, while
    /// `alt-3` and `alt-4` keep working. All five jump again once a project is
    /// registered.
    #[test]
    fn the_gated_screen_jumps_refuse_until_a_project_exists() {
        let mut app = empty_app();
        for (press, blocked) in [
            ('1', Screen::Cockpit),
            ('2', Screen::Tasks),
            ('5', Screen::Milestones),
        ] {
            for from in [Screen::Projects, Screen::Config] {
                app.screen = from;
                app.status = None;
                alt_key(&mut app, KeyCode::Char(press));
                assert_eq!(app.screen, from, "alt-{press} left {from:?}");
                assert_eq!(app.status.as_deref(), Some(NO_PROJECTS_JUMP_HINT));
                assert_ne!(app.screen, blocked);
            }
        }

        app.screen = Screen::Config;
        alt_key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        alt_key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.screen, Screen::Config);

        register_a_project(&mut app);
        for (press, screen) in [
            ('1', Screen::Cockpit),
            ('2', Screen::Tasks),
            ('3', Screen::Projects),
            ('4', Screen::Config),
            ('5', Screen::Milestones),
        ] {
            app.status = None;
            alt_key(&mut app, KeyCode::Char(press));
            assert_eq!(app.screen, screen);
            assert_eq!(app.status, None);
        }
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
    /// `a`, not a screen number. The gate (DESIGN.md §9) keeps the operator off
    /// the two screens `n` is bound on until a project exists, so this covers a
    /// defensive path rather than one the TUI can reach; it is kept because the
    /// refusal guards a real invariant for a single line.
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
    pub(super) fn scratch_env(
        name: &str,
        agents_toml: Option<&str>,
    ) -> (Store, crate::dispatch::DispatchCtx, std::path::PathBuf) {
        use std::process::{Command, Stdio};

        let root = tempfile::Builder::new()
            .prefix(&format!("voro-app-{name}-"))
            .tempdir()
            .unwrap()
            .keep();
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

        let root = tempfile::Builder::new()
            .prefix("voro-app-resume-")
            .tempdir()
            .unwrap()
            .keep();
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
                milestone: false,
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

    // --- projects screen (task, DESIGN.md §9) ---

    /// Tab cycles cockpit → tasks → projects → config → milestones → cockpit,
    /// and the alt-digits jump to a screen directly — from every screen,
    /// including the three that handle their own keys (DESIGN.md §9).
    #[test]
    fn tab_and_alt_digits_move_between_the_five_screens() {
        let mut app = app_with(&[]);
        assert_eq!(app.screen, Screen::Cockpit);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Tasks);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Projects);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Config);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Milestones);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Cockpit);

        alt_key(&mut app, KeyCode::Char('5'));
        assert_eq!(app.screen, Screen::Milestones);
        alt_key(&mut app, KeyCode::Char('2'));

        assert_eq!(app.screen, Screen::Tasks);
        alt_key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Cockpit);
        alt_key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.screen, Screen::Config);
        // The config screen's letter keys are its own, and the jump out of it is
        // the shared alt binding.
        alt_key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        // The projects screen's own digits are weights, but the modifier gets
        // past them.
        alt_key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.screen, Screen::Tasks);
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.screen, Screen::Projects);
        alt_key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Cockpit);
    }

    /// Type each character of `s` as a `Char` key press.
    pub(super) fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    // --- last-session surfacing and the log key ---

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
}
