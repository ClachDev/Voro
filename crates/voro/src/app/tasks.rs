//! What the selected task can do from the cockpit and the task browser
//! (DESIGN.md §9): create, refine, dispatch, prioritise, link and open its
//! pull request, and open its diff in a viewer.

use super::BrowserRow;
use super::CockpitRow;
use super::Screen;
use crate::dispatch::Filing;
use voro_core::{
    Action, CompletionReport, Event, LivenessSource, PrRef, Priority, Project, RefineOutcome,
    ScoreBreakdown, TaskState, projects_for_new_task,
};

use super::sessions::state_accepts_message;
use super::{
    ALL_PROJECTS_ARCHIVED_HINT, App, CreateFlow, EditorRequest, Mode, NO_PROJECTS_HINT, PromptKind,
    RefineFlow,
};

impl App {
    /// Toggle the selected task's deep flag (DESIGN.md §8): `!` moves it
    /// between the agent's workhorse model and its strongest one. Routes
    /// through `voro-core` so the change is logged, and reports the store's
    /// refusal — of a human task — on the status line.
    pub(super) fn toggle_deep(&mut self, task_id: i64) {
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
    pub(super) fn hand_off_selected(&mut self) {
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

    /// The checkout a task's work lives in (DESIGN.md §8): its resolved repo's
    /// path. Every TUI action that needs a working directory for a task —
    /// paging its log, attaching to its session, asking whether it can take a
    /// pull request — comes here rather than reading a project path.
    pub(super) fn task_checkout(&self, task_id: i64) -> voro_core::Result<String> {
        let task = self.store.task(task_id)?;
        Ok(self.store.repo_for_task(&task)?.path)
    }

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

    /// The same verb, withheld where the `[incomplete report]` marker stands in
    /// its place (DESIGN.md §8). Only `pr` is withheld: a pull request is built
    /// from the summary, so a half-written report cannot become one and the
    /// marker is the truer line to show. Every other verb — `open` above all,
    /// which is what a checkout with no remote advertises — the missing summary
    /// does not block, so the recommendation stands and the marker sits beside
    /// it rather than in place of it.
    pub fn advertised_action(&self, task: &voro_core::Task) -> Option<voro_core::NextAction> {
        let verb = self.next_action(task)?;
        (verb != voro_core::NextAction::Pr || !self.incomplete_report.contains(&task.id))
            .then_some(verb)
    }

    /// The repo a task names, as (name, path), or `None` when it runs in its
    /// project's default — the detail pane renders the line only when it says
    /// something the project row does not.
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
    /// hint (DESIGN.md §9). The two states the key itself allows, and a branch
    /// to diff: opening builds its diff from the task's branch, so a task
    /// without one — an investigation whose whole product is its summary, or a
    /// dispatch that has yet to name a branch — has nothing to look at.
    pub fn selected_has_a_diff(&self) -> bool {
        self.selected_task().is_some_and(|t| {
            matches!(t.state, TaskState::Review | TaskState::Running) && t.branch.is_some()
        })
    }

    /// Whether the selection is the task `g` has a PR to show or create — what
    /// gates that hint (DESIGN.md §9). The key stays bound in every state, for
    /// jumping to a tracked PR and linking one; only the advertisement is
    /// narrowed to the moment the PR is the review, which needs a PR to jump to
    /// or a branch to open one from — `plan_pr` refuses without a branch.
    pub fn selected_is_in_review(&self) -> bool {
        self.selected_task().is_some_and(|t| {
            t.state == TaskState::Review && (t.branch.is_some() || t.pr_url.is_some())
        })
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

    /// The projects the create flows offer, in the order they offer them
    /// (DESIGN.md §9) — unarchived only, weightiest first. Both the picker's
    /// key handler and its draw arm index this, so `sel` means the same thing
    /// to each.
    pub fn creatable_projects(&self) -> Vec<&Project> {
        projects_for_new_task(&self.projects)
    }

    /// Begin creating a task in one of the three flows (DESIGN.md §9): straight
    /// into it when exactly one project can take work, via the project picker
    /// when several can, and a pointer to the projects screen when none can —
    /// because none is registered, or because every one of them is archived.
    pub(super) fn new_task(&mut self, flow: CreateFlow) {
        let offered: Vec<i64> = self.creatable_projects().iter().map(|p| p.id).collect();
        match offered.len() {
            0 if self.projects.is_empty() => self.status = Some(NO_PROJECTS_HINT.into()),
            0 => self.status = Some(ALL_PROJECTS_ARCHIVED_HINT.into()),
            1 => self.start_create(offered[0], flow),
            _ => self.mode = Mode::PickProject { sel: 0, flow },
        }
    }

    /// Launch the chosen create flow on a project: open the one-line modal the
    /// background agent expands, queue the `$EDITOR` form, or assemble a
    /// planning session (DESIGN.md §8) for main() to run in the foreground. An
    /// agent without a `plan` verb — or any other assembly failure — reports
    /// what to configure through the status line, the same "no-op with an
    /// explanation" style as the dispatch keys.
    pub(super) fn start_create(&mut self, project_id: i64, flow: CreateFlow) {
        match flow {
            CreateFlow::Quick(filing) => {
                self.mode = Mode::QuickCreate {
                    project_id,
                    filing,
                    buffer: String::new(),
                };
            }
            CreateFlow::Editor(filing) => {
                self.pending_editor = Some(EditorRequest::Create { project_id, filing });
            }
            CreateFlow::Plan(filing) => {
                match crate::dispatch::plan_session(
                    &self.store,
                    &self.dispatch_ctx,
                    crate::dispatch::PlanTarget::Create { project_id, filing },
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
    pub(super) fn refine_selected(&mut self, flow: RefineFlow) {
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
    pub(super) fn refine_with_note(&mut self, task_id: i64, note: &str) {
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
        // The interactive round is the own-pid case (DESIGN.md §8): a
        // foreground `plan` child Voro spawned, so the pid recorded here is the
        // round itself and reconciliation must read it rather than a listing
        // this session never appears in.
        if let Err(e) = self.store.record_refine_launch(
            refine.task_id,
            "",
            &refine.agent,
            Some(pid),
            LivenessSource::Pid,
            None,
        ) {
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
    pub(super) fn cancel_refine_selected(&mut self) {
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
    pub(super) fn dispatchable_selected_task(&mut self) -> Option<(i64, Option<String>)> {
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
    pub(super) fn dispatch_task(&mut self, task_id: i64, agent_override: Option<String>) {
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
    pub(super) fn open_selected_in_viewer(&mut self) {
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
    /// A checkout with nowhere to push refuses before the modal, naming `o` —
    /// the local diff is the only thing left to look at — while a remote `gh`
    /// cannot address as GitHub is refused by the create it starts.
    pub(super) fn open_selected_pr(&mut self) {
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
        // A create already running for this task owns the branch until it
        // lands; a second one would open a second pull request.
        if self.pr_create.in_flight(id) {
            self.status = Some(format!("already creating the PR for #{id} — waiting on it"));
            return;
        }
        // Both preconditions are network-free (DESIGN.md §8), so the modal is
        // on screen the instant the key is pressed: the store says whether the
        // task is PR-ready, and the memoised `git remote` reading says whether
        // its checkout could take a pull request at all.
        let planned = crate::pr::plan(&self.store, id)
            .and_then(|plan| self.pr_checkout_takes_prs(id).map(|()| plan));
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
    /// TUI's idiom so the refusal points at `o` (DESIGN.md §8). Answered from
    /// the memoised `git remote` reading the row's own `next:` verb is derived
    /// through, so the key and the advertisement cannot disagree and neither
    /// costs a round-trip. The sharper question — whether `gh` can address the
    /// remote as a GitHub repository — is put by the background create instead,
    /// so a non-GitHub remote reaches the same dead end a moment later.
    fn pr_checkout_takes_prs(&self, task_id: i64) -> Result<(), String> {
        if !self.local_review.contains(&task_id) {
            return Ok(());
        }
        let checkout = self.task_checkout(task_id).map_err(|e| e.to_string())?;
        Err(format!(
            "{checkout} has no remote to open a pull request on — \
             use `o` to see this task's diff in a viewer"
        ))
    }

    /// Start a confirmed create off the event loop (DESIGN.md §8). The store
    /// half runs here — a few SQLite reads, and a gap it names is worth naming
    /// before a thread is spawned — and the push and `gh pr create` go to the
    /// background, where the operator is not waiting on them. The status line
    /// says what is running, since the queue redraws with the task looking
    /// exactly as it did.
    pub(super) fn start_pr_create(&mut self, task_id: i64) {
        let input = match crate::pr::create_input(&self.store, task_id) {
            Ok(input) => input,
            Err(e) => {
                self.status = Some(e);
                return;
            }
        };
        self.status = Some(match self.pr_create.start(task_id, input, "`o`") {
            true => format!("creating the PR for #{task_id}…"),
            false => format!("already creating the PR for #{task_id} — waiting on it"),
        });
    }

    /// Report a create-PR attempt, chaining a success straight into the browser
    /// (DESIGN.md §8): creating a PR is all but always followed by looking at
    /// it, so `g` does both. The create is the durable half — its URL is already
    /// recorded on the task — so a browser that will not launch is reported
    /// beside the URL rather than as a failed create.
    pub(super) fn report_created_pr(
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

    /// Record, open, and report every create that has landed (DESIGN.md §8).
    /// Each answer belongs to the task it was started for rather than to the
    /// selection, and a tracked PR is a fact about the branch rather than about
    /// a state, so nothing here is discarded or re-gated on what the task has
    /// become in the meantime. A success is chained straight into the browser,
    /// which is the promise the confirmation modal was already making; a
    /// failure is the one thing the operator pressed a key to learn, so it
    /// reaches the status line rather than being swallowed the way a lost
    /// revision is.
    pub fn poll_pr_create(&mut self) {
        self.drain_pr_creates(crate::pr::open_url);
    }

    /// The half above with the browser launch passed in, for the same reason
    /// `report_created_pr` takes one: the record-and-open chain is testable
    /// without `gh`, and a test can never launch a real browser.
    fn drain_pr_creates(&mut self, mut open: impl FnMut(&str) -> Result<String, String>) {
        let landed = self.pr_create.take_results();
        if landed.is_empty() {
            return;
        }
        for (task_id, created) in landed {
            let created =
                created.and_then(|url| crate::pr::record_created(&mut self.store, task_id, &url));
            self.report_created_pr(task_id, created, &mut open);
        }
        // The rows now carry a tracked PR: their `next:` verb and their marker
        // both change, and neither would until the next unrelated refresh.
        let result = self.refresh();
        self.report(result);
    }

    /// Quick propose (DESIGN.md §6/§8): spawn the headless agent that expands
    /// the typed line into a task and files it with `voro add`. Nothing waits on
    /// it and no task exists yet, so there is nothing to refresh onto — the
    /// proposal shows up in the untriaged count and the queue on a later
    /// refresh, exactly as one an agent filed with `voro propose` does. An empty
    /// line asked for nothing, so it spawns nothing.
    pub(super) fn quick_propose(&mut self, project_id: i64, filing: Filing, intent: &str) {
        if intent.trim().is_empty() {
            self.status = Some("cancelled".into());
            return;
        }
        self.status = Some(
            match crate::dispatch::propose(
                &self.store,
                &self.dispatch_ctx,
                project_id,
                intent,
                filing,
            ) {
                Ok(summary) => summary,
                Err(e) => e,
            },
        );
    }

    /// Validate and track a PR reference on a task, then refresh. An unparseable
    /// reference keeps the prompt open with the typed text intact and the parse
    /// error on the status line, so a typo can be fixed without retyping.
    pub(super) fn link_pr(&mut self, task_id: i64, raw: &str) {
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
    pub(super) fn prompt_seed(&mut self, task_id: i64, kind: PromptKind) -> String {
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

    /// Link or unlink one document, whichever the current state calls for, and
    /// refresh so the detail panes behind the picker show the new list.
    pub(super) fn toggle_doc_link(&mut self, task_id: i64, doc: &voro_core::Doc) {
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

    /// Set the priority of whichever task the selection resolves to — the bare
    /// digit the cockpit and the task browser share with the detail popup
    /// (DESIGN.md §9). A digit with nothing to act on says why rather than
    /// passing unremarked: a collapsed digest names no single task, and an empty
    /// queue names none at all.
    pub(super) fn set_selected_priority(&mut self, digit: char) {
        let Ok(priority) = Priority::from_int((digit as u8 - b'0') as i64) else {
            return;
        };
        match self.selected_task_id() {
            Some(id) => self.set_priority(id, priority),
            None => self.status = Some(self.no_priority_target().into()),
        }
    }

    /// Why a bare digit found no task to prioritise.
    fn no_priority_target(&self) -> &'static str {
        if self.screen == Screen::Cockpit
            && let Some(CockpitRow::Queue(i)) = self.cockpit_rows.get(self.cockpit_sel)
            && self.digest(*i).is_some()
        {
            return "select a proposal inside the digest (⏎ expands) to set its priority";
        }
        if self.screen == Screen::Tasks
            && let Some(BrowserRow::Group(_)) = self.browser_rows.get(self.tasks_sel)
        {
            return "select a task inside the fold (⏎ expands) to set its priority";
        }
        "nothing selected"
    }

    /// Re-prioritise a task in place, the review-time fast path that
    /// skips the edit form. Routes through `voro-core` so the change is logged,
    /// then refreshes to re-score and re-sort. The status line names the task,
    /// since the digit now fires on a row picked out of a queue rather than only
    /// on the one task the popup framed.
    pub(super) fn set_priority(&mut self, task_id: i64, priority: Priority) {
        let before = self.store.task(task_id).ok().map(|t| t.priority);
        match self.store.set_priority(task_id, priority) {
            Ok(task) => {
                let from = before.map_or_else(String::new, |old| format!("{old} -> "));
                self.status = Some(format!(
                    "#{} {} priority {from}{priority}",
                    task.id, task.title
                ));
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
            milestone: form.milestone,
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
pub(crate) mod tests {
    use super::*;
    use crate::app::ViewerFormState;
    use crate::app::tests::{alt_key, app_with, ctrl_key, dummy_ctx, key, scratch_env};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use voro_core::{NewTask, Store};

    /// Select a queued proposal: the cockpit collapses them into a per-project
    /// digest (DESIGN.md §7), so Enter folds it open before a constituent row
    /// can be selected.
    pub(crate) fn select_proposal(app: &mut App) -> i64 {
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

    /// The event history the `h` toggle draws comes straight from the store,
    /// oldest first.
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

    /// `!` is the same toggle wherever a task is selected: the
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

    // --- bare digits set the selected row's number (DESIGN.md §9) ---

    /// The daily act, one keystroke on the row already selected: `0`–`3` on the
    /// cockpit re-prioritises the task under the cursor through the store.
    #[test]
    fn digit_on_the_cockpit_sets_the_selected_tasks_priority() {
        let mut app = app_with(&[TaskState::Ready]);
        let id = app.queue_task_ids()[0];
        let title = app.store.task(id).unwrap().title;
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P1);

        key(&mut app, KeyCode::Char('0'));
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P0);
        assert_eq!(
            app.screen,
            Screen::Cockpit,
            "the digit is not a screen jump"
        );
        let status = app.status.clone().unwrap_or_default();
        for needle in [format!("#{id}"), title, "P1 -> P0".into()] {
            assert!(
                status.contains(&needle),
                "the status line should name {needle:?}: {status}"
            );
        }

        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P3);
    }

    /// A digest names no single task, so the digit says which selection it
    /// wants rather than passing unremarked; folded open, the proposal beneath
    /// it takes the priority.
    #[test]
    fn digit_on_a_digest_reports_and_reaches_the_proposal_once_expanded() {
        let mut app = app_with(&[TaskState::Proposed]);
        let id = app.all[0].task.id;

        key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P1);
        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.contains("digest"),
            "the digest row should say why the digit did nothing: {status}"
        );

        assert_eq!(select_proposal(&mut app), id);
        key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P2);
    }

    /// Weight runs to 5 and priority stops at P3, so the operator arriving from
    /// the projects screen meets an explanation rather than silence.
    #[test]
    fn digits_beyond_p3_report_the_priority_range() {
        let mut app = app_with(&[TaskState::Ready]);
        let id = app.queue_task_ids()[0];
        for digit in ['4', '5'] {
            key(&mut app, KeyCode::Char(digit));
            assert_eq!(app.store.task(id).unwrap().priority, Priority::P1);
            assert_eq!(app.status.as_deref(), Some("priority is P0–P3"));
            assert_eq!(app.screen, Screen::Cockpit);
        }
    }

    /// An empty queue has nothing to prioritise, and says so.
    #[test]
    fn digit_with_nothing_selected_reports_it() {
        let mut app = app_with(&[]);
        key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.status.as_deref(), Some("nothing selected"));
    }

    /// The same binding on the task browser, where naming the task matters most
    /// — the operator is looking at a list of them.
    #[test]
    fn digit_on_the_tasks_screen_sets_priority_and_names_the_task() {
        let mut app = app_with(&[TaskState::Ready]);
        let id = app.all[0].task.id;
        let title = app.all[0].task.title.clone();
        alt_key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.screen, Screen::Tasks);

        key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P3);
        let status = app.status.clone().unwrap_or_default();
        assert!(
            status.contains(&format!("#{id}")) && status.contains(&title),
            "the status line should name the task: {status}"
        );
        assert!(status.contains("P3"), "{status}");
    }

    /// The popup's own `0`–`3` still sets the viewed task's priority
    /// in place, through the same call the two screens now share.
    #[test]
    fn digit_in_the_detail_popup_sets_priority_without_closing_it() {
        let mut app = app_with(&[TaskState::Ready]);
        let id = app.all[0].task.id;
        alt_key(&mut app, KeyCode::Char('2'));
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Detail { .. }));

        key(&mut app, KeyCode::Char('0'));
        assert_eq!(app.store.task(id).unwrap().priority, Priority::P0);
        assert!(matches!(app.mode, Mode::Detail { .. }));
        assert!(
            app.status
                .as_deref()
                .unwrap_or_default()
                .contains(&format!("#{id}")),
            "{:?}",
            app.status
        );
    }

    // --- dispatch keybindings (DESIGN.md §8/§9) ---

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
                milestone: false,
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

    // --- open-in-viewer keybinding (DESIGN.md §11a) ---

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
                milestone: false,
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

    /// A review task PR-ready in every way but its checkout: a branch, a
    /// completion summary, and a directory nowhere near a forge. `with_repo`
    /// decides whether that directory is a git repository at all, which is the
    /// whole of what the press-time gate reads (DESIGN.md §8).
    fn pr_ready_app(with_repo: bool) -> (App, i64, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("voro-review-key-")
            .tempdir()
            .unwrap()
            .keep();
        if with_repo {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["init", "-q"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git init failed");
        }

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
                milestone: false,
            })
            .unwrap();
        store.set_branch(task.id, Some("feat/thing")).unwrap();
        store.apply(task.id, Action::Start).unwrap();
        store
            .apply(task.id, Action::Complete(Some("did it".into())))
            .unwrap();

        let app = App::new(store, dummy_ctx()).unwrap();
        (app, task.id, dir)
    }

    /// `g` opens the confirmation without asking the network anything
    /// (DESIGN.md §8): even on a checkout that is not a GitHub repository at
    /// all, the modal appears — the refusal belongs to the create itself,
    /// below.
    #[test]
    fn review_key_opens_the_confirmation_without_a_round_trip() {
        let (mut app, task_id, dir) = pr_ready_app(false);
        key(&mut app, KeyCode::Char('g'));

        match app.mode {
            Mode::ConfirmPr {
                task_id: id,
                ref branch,
                ..
            } => {
                assert_eq!(id, task_id);
                assert_eq!(branch, "feat/thing");
            }
            _ => panic!("expected the confirmation modal, got {:?}", app.status),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `g` is statically the GitHub medium (DESIGN.md §8): a checkout with no
    /// remote to push to has no forge to open a pull request on, which `git
    /// remote` answers without the network — so that refusal still comes before
    /// the modal, still names `o`, and still does not fall back to the
    /// project's viewer.
    #[test]
    fn review_key_on_a_remoteless_checkout_refuses_naming_the_viewer_key() {
        let (mut app, _, dir) = pr_ready_app(true);
        key(&mut app, KeyCode::Char('g'));

        let status = app.status.as_deref().unwrap_or("");
        assert!(matches!(app.mode, Mode::Normal), "{status:?}");
        assert!(status.contains("`o`"), "{status:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The sharper refusal — a checkout `gh` cannot address as a GitHub
    /// repository — survives the move off the event loop unchanged: it is the
    /// create that asks now, and its error is still the one that points at `o`.
    #[test]
    fn a_create_on_a_non_github_checkout_still_names_the_viewer_key() {
        let (mut app, task_id, dir) = pr_ready_app(false);
        let error = crate::pr::create(&mut app.store, task_id, "`o`")
            .expect_err("a bare directory is no GitHub repository");
        assert!(error.contains("`o`"), "{error:?}");
        assert!(app.store.task(task_id).unwrap().pr_url.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A create that lands is recorded, opened, and named (DESIGN.md §8): the
    /// URL goes onto the task, the browser is launched on it, and the status
    /// line says so. The launch is passed in so the chain runs without `gh`.
    #[test]
    fn a_landed_create_is_recorded_and_opened() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        let url = "https://github.com/acme/widget/pull/7";
        app.pr_create.inject_result(task_id, Ok(url.to_string()));

        let mut opened = None;
        app.drain_pr_creates(|u| {
            opened = Some(u.to_string());
            Ok(format!("opening {u} in the browser"))
        });

        assert_eq!(
            app.store.task(task_id).unwrap().pr_url.as_deref(),
            Some(url)
        );
        assert_eq!(opened.as_deref(), Some(url));
        let status = app.status.as_deref().unwrap_or("");
        assert!(status.contains(url), "{status:?}");
    }

    /// A create that failed is the one answer the operator pressed a key to
    /// see, so it reaches the status line rather than being swallowed the way a
    /// lost reviewed revision is — and nothing is recorded or opened.
    #[test]
    fn a_failed_create_lands_in_the_status_line() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        app.pr_create
            .inject_result(task_id, Err("`git push origin feat/thing` failed".into()));

        let mut opened = false;
        app.drain_pr_creates(|_| {
            opened = true;
            Ok(String::new())
        });

        assert!(!opened, "the browser was launched for a failed create");
        assert_eq!(
            app.status.as_deref(),
            Some("`git push origin feat/thing` failed")
        );
        assert!(app.store.task(task_id).unwrap().pr_url.is_none());
    }

    /// Two creates for one branch would be two pull requests, so `g` on a task
    /// whose create is still running opens no modal and says what it is waiting
    /// on (DESIGN.md §8).
    #[test]
    fn the_github_key_refuses_while_a_create_is_in_flight() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        app.pr_create
            .inject_result(task_id, Ok("https://github.com/acme/widget/pull/7".into()));

        key(&mut app, KeyCode::Char('g'));

        let status = app.status.as_deref().unwrap_or("");
        assert!(matches!(app.mode, Mode::Normal), "{status:?}");
        assert!(status.contains("already creating"), "{status:?}");
    }

    /// Confirming ends the operator's wait whatever the create then does: the
    /// modal closes on the keypress (DESIGN.md §8). Here the task has no branch
    /// to push, so the store half refuses before any thread is started, and the
    /// gap is named rather than a create being reported as running.
    #[test]
    fn confirming_a_create_closes_the_modal_at_once() {
        let mut app = app_with(&[TaskState::Review]);
        let task_id = app.selected_task_id().unwrap();
        app.mode = Mode::ConfirmPr {
            task_id,
            branch: "feat/thing".into(),
            title: "reviewable".into(),
        };

        key(&mut app, KeyCode::Enter);

        let status = app.status.as_deref().unwrap_or("");
        assert!(matches!(app.mode, Mode::Normal), "{status:?}");
        assert!(status.contains("branch"), "{status:?}");
        assert!(app.pr_create.take_results().is_empty());
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

    // --- planning sessions ---

    /// An app whose dispatch context reads a scratch `voro.toml`, so the
    /// planning keys resolve a known agent instead of the developer's real
    /// config and PATH.
    fn app_with_agents(agents_toml: &str) -> App {
        let mut app = app_with(&[TaskState::Ready]);
        let dir = tempfile::Builder::new()
            .prefix("voro-plan-key-")
            .tempdir()
            .unwrap()
            .keep();
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

    // --- quick propose ---

    /// An agents config whose `dispatch` verb only copies the prompt file, so a
    /// spawned expansion is observable through the prompt it wrote without
    /// anything real being launched. The project's checkout is re-pointed at the
    /// scratch directory, since a spawn needs a cwd that exists.
    fn app_with_stub_dispatch() -> App {
        let mut app = app_with_agents(
            "default_agent = \"stub\"\n\n[agents.stub]\ndispatch = \"cat {prompt_file}\"\n",
        );
        let checkout = app.dispatch_ctx.runtime_dir.with_file_name("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        let project_id = app.projects[0].id;
        app.store
            .set_default_repo_path(project_id, checkout.to_str().unwrap())
            .unwrap();
        app.refresh().unwrap();
        app
    }

    /// Every prompt the app's dispatch context has written, newest last.
    fn written_prompts(app: &App) -> Vec<String> {
        let mut paths: Vec<_> = std::fs::read_dir(&app.dispatch_ctx.runtime_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| Some(e.ok()?.path()))
                    .filter(|p| p.to_string_lossy().ends_with(".prompt.md"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect()
    }

    /// `n` is the background quick propose (DESIGN.md §8): with one project it
    /// opens the one-line modal on it directly, suspending nothing — no
    /// `$EDITOR` request, no foreground planning session.
    #[test]
    fn quick_create_key_opens_the_modal_on_the_only_project() {
        let mut app = app_with_stub_dispatch();
        let project_id = app.projects[0].id;

        key(&mut app, KeyCode::Char('n'));
        match &app.mode {
            Mode::QuickCreate {
                project_id: id,
                filing: Filing::Task,
                buffer,
            } => {
                assert_eq!(*id, project_id);
                assert!(buffer.is_empty(), "buffer was {buffer:?}");
            }
            _ => panic!("n should open the quick-create modal"),
        }
        assert!(app.pending_editor.is_none());
        assert!(app.pending_plan.is_none());
    }

    /// The Milestones tab's `n` is the same quick propose, told to file with
    /// `--milestone`; it writes no row itself (DESIGN.md §9).
    #[test]
    fn n_on_the_milestones_tab_launches_the_quick_propose_and_writes_no_row() {
        let mut app = app_with_stub_dispatch();
        let before = app.store.tasks().unwrap().len();
        app.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT));
        assert_eq!(app.screen, Screen::Milestones);
        key(&mut app, KeyCode::Char('n'));
        assert!(matches!(
            app.mode,
            Mode::QuickCreate {
                filing: Filing::Milestone,
                ..
            }
        ));
        for c in "carpet crossing".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);

        let status = app.status.as_deref().unwrap_or("");
        assert!(status.contains("proposing milestone in demo"), "{status}");
        let prompts = written_prompts(&app);
        assert_eq!(prompts.len(), 1, "one expansion, one prompt");
        assert!(prompts[0].contains("--milestone"), "{}", prompts[0]);
        assert_eq!(app.store.tasks().unwrap().len(), before);
    }

    /// ⏎ on a typed line spawns the expansion and returns to the queue: the
    /// prompt the agent gets carries the line as typed and names the project it
    /// files into, and the status line says the proposal is on its way.
    #[test]
    fn quick_create_submit_spawns_the_expansion_and_reports_it() {
        let mut app = app_with_stub_dispatch();

        key(&mut app, KeyCode::Char('n'));
        for c in "cache the score decomposition".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter);

        assert!(matches!(app.mode, Mode::Normal));
        let status = app.status.as_deref().unwrap_or("");
        assert!(status.contains("proposing task in demo"), "{status}");

        let prompts = written_prompts(&app);
        assert_eq!(prompts.len(), 1, "one expansion, one prompt");
        let prompt = &prompts[0];
        assert!(
            prompt.contains("cache the score decomposition"),
            "the typed line seeds the prompt: {prompt}"
        );
        assert!(prompt.contains("voro add 'demo'"), "{prompt}");
    }

    /// Backspace edits the line, esc abandons it: neither spawns anything, and
    /// a blank submit is a cancel rather than a proposal with no intent.
    #[test]
    fn quick_create_cancels_on_esc_and_on_a_blank_line() {
        let mut app = app_with_stub_dispatch();

        key(&mut app, KeyCode::Char('n'));
        key(&mut app, KeyCode::Char('x'));
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.status.as_deref(), Some("cancelled"));
        assert!(written_prompts(&app).is_empty());

        key(&mut app, KeyCode::Char('n'));
        for c in "something".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(written_prompts(&app).is_empty());
    }

    /// With several projects `n` routes through the same picker the other create
    /// flows use, carrying the quick flow; ⏎ there opens the modal on the picked
    /// project rather than proposing into it blind.
    #[test]
    fn quick_create_key_routes_through_the_project_picker() {
        let mut app = app_with_stub_dispatch();
        let second = app.store.create_project("second", "/tmp/second").unwrap();
        app.refresh().unwrap();

        key(&mut app, KeyCode::Char('n'));
        assert!(matches!(
            app.mode,
            Mode::PickProject {
                flow: CreateFlow::Quick(Filing::Task),
                ..
            }
        ));

        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Enter);
        match &app.mode {
            Mode::QuickCreate { project_id, .. } => assert_eq!(*project_id, second.id),
            _ => panic!("⏎ in the picker should open the quick-create modal"),
        }
        assert!(written_prompts(&app).is_empty(), "nothing spawns yet");
    }

    /// An archived project cannot take a new task at all (DESIGN.md §5), so it
    /// is not a candidate: with one live project beside three archived ones
    /// there is nothing to pick between, and `n` opens the create flow on the
    /// live one rather than a picker whose other rows can only fail.
    #[test]
    fn create_skips_the_picker_when_only_one_project_is_unarchived() {
        let mut app = app_with_stub_dispatch();
        let live = app.projects[0].id;
        for name in ["retired-a", "retired-b", "retired-c"] {
            let p = app
                .store
                .create_project(name, &format!("/tmp/{name}"))
                .unwrap();
            app.store.set_archived(p.id, true).unwrap();
        }
        app.refresh().unwrap();
        assert_eq!(app.projects.len(), 4);

        key(&mut app, KeyCode::Char('n'));
        match &app.mode {
            Mode::QuickCreate { project_id, .. } => assert_eq!(*project_id, live),
            _ => panic!("n should open the quick-create modal"),
        }
    }

    /// With every project archived there is no project to create in, and no
    /// picker to open over none. It refuses the way the neighbouring keys do —
    /// a no-op with an explanation, pointing at the screen that unarchives.
    #[test]
    fn create_with_every_project_archived_explains_itself() {
        let mut app = app_with_stub_dispatch();
        let only = app.projects[0].id;
        app.store.set_archived(only, true).unwrap();
        app.refresh().unwrap();

        for press in ['n', 'N'] {
            app.status = None;
            key(&mut app, KeyCode::Char(press));
            assert!(matches!(app.mode, Mode::Normal), "no picker opens");
            assert_eq!(app.status.as_deref(), Some(ALL_PROJECTS_ARCHIVED_HINT));
            assert!(app.pending_editor.is_none());
            assert!(app.pending_plan.is_none());
        }
    }

    /// The picker offers what can take work in the order the operator ranked it
    /// — weight descending, name ascending inside a weight (DESIGN.md §9) — so
    /// ⏎ on a row starts the create flow on the project *that* order puts
    /// there, not the one alphabetical order would have.
    #[test]
    fn picker_rows_follow_weight_then_name() {
        let mut app = app_with_stub_dispatch();
        let demo = app.projects[0].id;
        app.store.set_weight(demo, 1).unwrap();
        let heavy = app.store.create_project("zeta", "/tmp/zeta").unwrap();
        app.store.set_weight(heavy.id, 4).unwrap();
        let parked = app.store.create_project("alpha", "/tmp/alpha").unwrap();
        app.store.set_weight(parked.id, 0).unwrap();
        let hidden = app.store.create_project("beta", "/tmp/beta").unwrap();
        app.store.set_weight(hidden.id, 5).unwrap();
        app.store.set_archived(hidden.id, true).unwrap();
        app.refresh().unwrap();

        // zeta (4), demo (1), alpha (0) — beta is archived and absent despite
        // outweighing all three.
        for (row, expected) in [(0, heavy.id), (1, demo), (2, parked.id)] {
            key(&mut app, KeyCode::Char('n'));
            assert!(matches!(app.mode, Mode::PickProject { sel: 0, .. }));
            for _ in 0..row {
                key(&mut app, KeyCode::Char('j'));
            }
            key(&mut app, KeyCode::Enter);
            match &app.mode {
                Mode::QuickCreate { project_id, .. } => assert_eq!(*project_id, expected, "{row}"),
                _ => panic!("row {row} should open the modal"),
            }
            key(&mut app, KeyCode::Esc);
        }
    }

    /// `ctrl-n` keeps the manual `$EDITOR` form, the only path that sets state,
    /// priority and dependencies at creation time (DESIGN.md §8).
    #[test]
    fn ctrl_n_still_queues_the_editor_form() {
        let mut app = app_with_stub_dispatch();
        let project_id = app.projects[0].id;

        ctrl_key(&mut app, KeyCode::Char('n'));
        assert!(matches!(app.mode, Mode::Normal));
        match app.pending_editor {
            Some(EditorRequest::Create {
                project_id: id,
                filing: Filing::Task,
            }) => assert_eq!(id, project_id),
            _ => panic!("ctrl-n should queue the manual create form"),
        }
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
                flow: CreateFlow::Plan(Filing::Task),
                ..
            }
        ));

        key(&mut app, KeyCode::Enter);
        let launch = app.pending_plan.take().expect("a planning session queued");
        assert_eq!(launch.cwd, "/tmp/demo");
    }

    /// `o` with no viewer set up anywhere raises the add-viewer form rather
    /// than only complaining, and says why on the status line; every
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
}
