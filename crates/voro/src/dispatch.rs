//! Dispatching a ready (or stalled) task to a headless agent session (DESIGN.md
//! §8): the I/O half of dispatch — resolving the agent, guarding that the
//! checkout is a git repository, writing the prompt, and spawning the process
//! detached — kept out of voro-core, which stays pure of process and filesystem
//! I/O. The atomic state-plus-session write is voro-core's `Store::record_dispatch`.
//!
//! For agents that define a `sessions` verb (task #75), dispatch additionally
//! captures the agent's own session reference after launch — by polling the
//! `sessions` listing for a session started in this project since the spawn,
//! falling back to the `backgrounded · <id>` line launchers print into the
//! log — and records it on the session row for later attach/resume.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use voro_core::{
    AgentsConfig, PROMPT_FILE_PLACEHOLDER, ReviewAction, Store, TASK_ID_PLACEHOLDER, TaskState,
    VIEWER_BASE_PLACEHOLDER, VIEWER_BRANCH_PLACEHOLDER, VIEWER_PATH_PLACEHOLDER,
    parse_sessions_json,
};

/// Prepended to every dispatched prompt so the agent learns the return-path
/// verbs (DESIGN.md §8). [`render_preamble`] fills the `{task_id}` and `{db}`
/// placeholders literally rather than relying on inherited `VORO_TASK_ID`/
/// `VORO_DB` env vars, which do not survive a launcher that hands the session
/// to a supervisor daemon (e.g. `claude --bg`).
const RETURN_PATH_PREAMBLE_TEMPLATE: &str = "\
<!-- Voro dispatch: how to report back -->
You are an agent dispatched by Voro on task {task_id}. Voro tracks this task in a
local SQLite database; report progress through these CLI verbs rather than
editing that database yourself:

    voro ask {task_id}{db} --question \"...\"     # blocked on a human decision (-> needs-input)
    voro resume {task_id}{db}                     # question answered in this session, carry on (-> running)
    voro done {task_id}{db} --summary \"...\"      # work finished, await review (-> review)
    voro propose <project> \"<title>\"{db} --body-file <path>   # file a follow-up task (-> proposed)

Run these commands exactly as shown: they name task {task_id} explicitly, so they
work from anywhere without any environment variable being set. After you `ask`,
the operator answers right here in this session — so when your question has been
answered, run `voro resume {task_id}` before continuing, to move the task back to
running (Voro records no answer text; the exchange is already in this transcript).
`propose` uses task {task_id} as the discovered-from source when `--from` is
omitted. Finish
with your work committed on a branch and a PR-ready `--summary` on `done` — what
changed, why, and how you verified it — since `voro pr` opens the pull request
straight from that summary. Never modify the database with raw SQL, which would
bypass the state machine and event log.{branch}

---

";

/// Shared by both branch blocks below so the early-registration instruction
/// cannot drift (task #96). `{name}` is the assigned branch or the `<name>` the
/// agent will choose; `{task_id}`/`{db}` keep the command copy-pasteable under
/// launch styles that drop the environment.
const BRANCH_REGISTER_SENTENCE: &str = "register it with `voro set {task_id}{db} --branch {name}` as you do, so Voro \
     tracks the real branch while the task runs";

/// Shared by both branch blocks so the stale-branch instruction cannot drift:
/// if the base moved while the task sat in review, the agent resolves the
/// conflict itself, inside its own worktree, touching neither the primary
/// checkout nor the remote. Fetching only updates remote-tracking refs, so it
/// leaves the trust model — dispatched agents cannot push — intact (DESIGN.md §8).
const BRANCH_REBASE_SENTENCE: &str = "If the branch conflicts with the project's base branch (typically because the \
     base moved while the task sat in review), run `git fetch origin <base>` from \
     inside the worktree and rebase or merge onto `origin/<base>`, resolving the \
     conflicts there — never modify the primary checkout, and never push.";

/// The `{branch}` block for a task that carries an intended git branch (task
/// #81): the agent is told the name, to do its work in a throwaway worktree on
/// it (never the primary checkout), to register it early via
/// [`BRANCH_REGISTER_SENTENCE`], to confirm it at completion, and to self-serve
/// a rebase onto a moved base via [`BRANCH_REBASE_SENTENCE`].
const ASSIGNED_BRANCH_TEMPLATE: &str = "\n\n\
This task is assigned the git branch `{name}`. You are spawned in the project
checkout — never modify it. Create a throwaway git worktree of the checkout on
this branch (e.g. `git worktree add <path> -b {name}`) and do all your work
inside that worktree — Voro runs no git, so the branch and its worktree are yours
to make — and {register}. Confirm the branch your work landed on with
`voro done {task_id}{db} --branch {name}`. {rebase}";

/// The `{branch}` block when no branch is assigned: the agent picks its own name
/// and must still register it early via [`BRANCH_REGISTER_SENTENCE`], so Voro
/// learns the branch while the task runs rather than only at `done`, and
/// self-serve a rebase onto a moved base via [`BRANCH_REBASE_SENTENCE`].
const UNASSIGNED_BRANCH_TEMPLATE: &str = "\n\n\
Pick a git branch for this work. You are spawned in the project checkout — never
modify it. Create a throwaway git worktree of the checkout on that branch (e.g.
`git worktree add <path> -b <branch>`) and do all your work inside that worktree
— Voro runs no git, so the branch and its worktree are yours to make — and
{register}; `<name>` is the name you choose. {rebase}";

/// Rendered per planning session (DESIGN.md §8) and written as the whole
/// prompt — unlike dispatch there is no task body to prepend it to, because
/// producing one is the session's job. `{project}` is the project's name,
/// `{project_arg}` the same name shell-quoted for the `voro add` line, and
/// `{db}` the same conditional `--db` flag the return-path preamble renders,
/// for the same reason: the command must name its database literally.
const PLANNING_PROMPT_TEMPLATE: &str = "\
<!-- Voro planning session: the deliverable is a task, not a PR -->
You are an agent launched by Voro to help the operator plan a new task for the
project `{project}`. This is an interactive planning conversation: ask the
operator what they want, and interview them until the task is well defined —
scope, approach where it is settled, and what done looks like. Do not modify
the project's files; the checkout is there to read, so the task can name real
files and code.

Write the task body as a self-contained dispatchable prompt: the agent that
picks it up later gets no other context, so name the relevant files, spell out
the decisions already made, and give concrete acceptance criteria.

When the operator confirms the draft, write the body to a file outside the
checkout and create the task with:

    voro add {project_arg} \"<title>\"{db} --body-file <path>

Add `--priority <0-3>` if the operator wants something other than the default
P2. The task is created in `proposed` and the operator triages it from the
queue, so do not pass a state. If the operator decides against creating a
task, end the session without creating one — that is a no-op, not a failure.
Never modify Voro's database with raw SQL, which would bypass the state
machine and event log.
";

/// How often the session-ref capture re-polls the agent's `sessions` command
/// while waiting for the freshly-launched session to appear in the listing.
const REF_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Clock slack when matching a listing entry's `startedAt` against the spawn
/// time: the agent stamps its session with its own clock, which may sit a
/// beat behind the timestamp taken here just before the spawn.
const SPAWN_CLOCK_SLACK_MS: i64 = 2000;

/// Fill [`RETURN_PATH_PREAMBLE_TEMPLATE`] for a concrete dispatch: the literal
/// task id in every verb, plus a `--db <path>` flag only when the dispatching
/// database is not the default one the verbs resolve to on their own.
fn render_preamble(task_id: i64, db_path: &Path, branch: Option<&str>) -> String {
    let db_flag = if db_path == Store::default_db_path() {
        String::new()
    } else {
        format!(" --db {}", shell_quote(db_path))
    };
    let register = BRANCH_REGISTER_SENTENCE.replace("{name}", branch.unwrap_or("<name>"));
    let branch_block = match branch {
        Some(name) => ASSIGNED_BRANCH_TEMPLATE
            .replace("{register}", &register)
            .replace("{rebase}", BRANCH_REBASE_SENTENCE)
            .replace("{name}", name),
        None => UNASSIGNED_BRANCH_TEMPLATE
            .replace("{register}", &register)
            .replace("{rebase}", BRANCH_REBASE_SENTENCE),
    };
    RETURN_PATH_PREAMBLE_TEMPLATE
        .replace("{branch}", &branch_block)
        .replace("{task_id}", &task_id.to_string())
        .replace("{db}", &db_flag)
}

/// Fill [`PLANNING_PROMPT_TEMPLATE`] for a concrete project: the `voro add`
/// line carries the shell-quoted project name and, exactly as
/// [`render_preamble`] does, a `--db` flag only when the database is not the
/// default one the verb resolves to unaided.
fn render_planning_prompt(project: &str, db_path: &Path) -> String {
    let db_flag = if db_path == Store::default_db_path() {
        String::new()
    } else {
        format!(" --db {}", shell_quote(db_path))
    };
    PLANNING_PROMPT_TEMPLATE
        .replace("{project_arg}", &shell_quote(Path::new(project)))
        .replace("{project}", project)
        .replace("{db}", &db_flag)
}

/// The assembled launch of a planning session: the agent's `plan` template
/// with the prompt file substituted, and the project checkout to run it in.
/// The TUI turns this into a foreground terminal round-trip, the same
/// suspend/restore as `$EDITOR` and attach/resume.
#[derive(Debug)]
pub struct PlanLaunch {
    pub command: String,
    pub cwd: String,
}

/// Assemble an interactive planning session for a project (DESIGN.md §8):
/// resolve the default agent, require its `plan` verb, write the planning
/// prompt outside the checkout, and substitute it into the template. No
/// session row is recorded and no task state changes — the session's
/// deliverable is a `proposed` task the agent creates through `voro add`, so
/// one that exits without creating anything has simply done nothing. There is
/// deliberately no dispatch-style guard: planning only reads the checkout and
/// writes nothing to it.
pub fn plan_session(
    store: &Store,
    ctx: &DispatchCtx,
    project_id: i64,
) -> Result<PlanLaunch, String> {
    let project = store.project(project_id).map_err(|e| e.to_string())?;
    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config.resolve(None).map_err(|e| e.to_string())?;
    let Some(plan_cmd) = agent.plan.as_deref() else {
        return Err(format!(
            "agent '{}' defines no plan template in {} — add plan = \"<interactive command> \
             {{prompt_file}}\" to its [agents.{}] table to plan tasks with it",
            agent.name,
            ctx.agents_path.display(),
            agent.name
        ));
    };
    std::fs::create_dir_all(&ctx.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.runtime_dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prompt_path = ctx
        .runtime_dir
        .join(format!("plan-{project_id}-{stamp}.prompt.md"));
    std::fs::write(
        &prompt_path,
        render_planning_prompt(&project.name, &ctx.db_path),
    )
    .map_err(|e| format!("cannot write prompt {}: {e}", prompt_path.display()))?;
    Ok(PlanLaunch {
        command: plan_cmd.replace(PROMPT_FILE_PLACEHOLDER, &shell_quote(&prompt_path)),
        cwd: project.path,
    })
}

/// Where dispatch finds its inputs and puts its artefacts. Built from the
/// database path in `main`; constructed directly in tests.
pub struct DispatchCtx {
    /// The active database, exported to the session as `VORO_DB` so the
    /// agent's return-path verbs write back to the same store.
    pub db_path: PathBuf,
    /// The config-file location dispatch reads (`voro.toml`).
    pub agents_path: PathBuf,
    /// Directory for prompt and log files — never inside a project checkout,
    /// so prompt and log files do not pollute the operator's working copy.
    pub runtime_dir: PathBuf,
    /// How long to keep polling for a session reference after spawning an
    /// agent that defines a `sessions` verb, before giving up (the ref stays
    /// NULL and the summary says so). Zero means a single attempt.
    pub ref_capture_timeout: Duration,
    /// The `VORO_TASK_ID` a dispatched session runs under, if any — the default
    /// discovered-from source for `propose`. Read from the environment only at
    /// the real `main` entry point so the CLI never reaches for ambient session
    /// state itself; tests leave it `None` and stay hermetic.
    pub session_task_id: Option<String>,
}

impl DispatchCtx {
    /// The real environment: the config file at its resolved default
    /// location, and a `sessions/` directory beside the database for prompts
    /// and logs.
    pub fn from_db_path(db_path: &Path) -> DispatchCtx {
        let runtime_dir = db_path
            .parent()
            .map(|d| d.join("sessions"))
            .unwrap_or_else(|| PathBuf::from("sessions"));
        DispatchCtx {
            db_path: db_path.to_path_buf(),
            agents_path: AgentsConfig::default_path(),
            runtime_dir,
            ref_capture_timeout: Duration::from_secs(5),
            session_task_id: None,
        }
    }

    /// The rolling log for subprocess launches that are *not* dispatches —
    /// viewer opens and attach/resume round-trips, which have no per-session
    /// log. They share one append-only file so a failure the TUI would paint
    /// over still leaves a breadcrumb. Single file, no rotation (DESIGN.md §8).
    pub fn launch_log_path(&self) -> PathBuf {
        self.runtime_dir.join("launches.log")
    }
}

/// Append a timestamped line to the launch log, best-effort — a launch must
/// not fail because its breadcrumb could not be written. Creates the parent
/// directory and opens in append mode so concurrent writers (a viewer's own
/// redirected stdout/stderr and this record line) interleave cleanly.
pub(crate) fn append_launch_log(path: &Path, line: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{ts}] {line}");
    }
}

/// Dispatch a ready task — or redispatch a stalled one — to a headless agent
/// session, returning a summary line. `agent_override` is the CLI `--agent`
/// flag, which outranks the task's own override (DESIGN.md §8). Every check
/// that can fail runs before the process is spawned, so a failed dispatch never
/// leaves an orphan session.
pub fn dispatch(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    spawn_session(store, ctx, task_id, agent_override)
}

/// Open a `review` (or `running`) task's diff in a viewer (DESIGN.md §11a): the
/// viewer medium of the per-project review action (§8). A dispatched agent works
/// in a throwaway worktree on the task's branch, so the diff lives there, not in
/// the primary checkout — the viewer is run in that worktree when the task has a
/// live one, falling back to `project.path` when it has no branch or no worktree
/// (§8). `viewer_override` names a `[viewers.<name>]` entry; `None` falls back to
/// the project's review action or the config default. The viewer template gets
/// `{path}` (the resolved dir), `{branch}` (the task's branch, empty when none),
/// and `{base}` (the checkout's default branch) so it can express a diff range.
/// Opening never touches task state, so there is no clean-tree guard (the diff
/// reviewed is often the uncommitted work itself).
pub fn open(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    viewer_override: Option<&str>,
) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    if !matches!(task.state, TaskState::Review | TaskState::Running) {
        return Err(format!(
            "only review or running tasks can be opened in a viewer; task {task_id} is {}",
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;

    // The project's review action may pin a named viewer even when open was
    // invoked directly rather than through the resolved `pr` action.
    let project_viewer = match &project.review_action {
        ReviewAction::Viewer(Some(name)) => Some(name.clone()),
        _ => None,
    };
    let viewer_name = viewer_override.map(str::to_string).or(project_viewer);

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let viewer = config
        .viewer_cmd(viewer_name.as_deref())
        .map_err(|e| e.to_string())?;

    // Run the viewer in the task's worktree when it has one — that is where the
    // dispatched agent's diff lives — otherwise the primary checkout.
    let viewer_dir = match task.branch.as_deref() {
        Some(branch) => crate::worktree::worktree_on_branch(&project.path, branch)?
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| project.path.clone()),
        None => project.path.clone(),
    };
    let base = default_base_branch(&project.path);
    let branch = task.branch.clone().unwrap_or_default();
    let command = viewer
        .replace(
            VIEWER_PATH_PLACEHOLDER,
            &shell_quote(Path::new(&viewer_dir)),
        )
        .replace(VIEWER_BRANCH_PLACEHOLDER, &branch)
        .replace(VIEWER_BASE_PLACEHOLDER, &base);

    // Redirect the detached viewer's output to the shared launch log so a
    // silent failure leaves a breadcrumb.
    std::fs::create_dir_all(&ctx.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.runtime_dir.display()))?;
    let launch_log = ctx.launch_log_path();
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&launch_log)
        .map_err(|e| format!("cannot open launch log {}: {e}", launch_log.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open launch log {}: {e}", launch_log.display()))?;
    append_launch_log(
        &launch_log,
        &format!("viewer: {command} (cwd {viewer_dir})"),
    );

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&viewer_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn viewer in {viewer_dir}: {e}"))?;
    let pid = i64::from(child.id());

    // Nothing waits on the viewer — reap it in a detached thread so an exited
    // child never lingers as a zombie, and record its exit status so a viewer
    // that failed after spawning is findable in the log.
    let reap_log = launch_log.clone();
    let reap_command = command.clone();
    std::thread::spawn(move || {
        let line = match child.wait() {
            Ok(status) => format!("viewer: {reap_command} exited with {status}"),
            Err(e) => format!("viewer: {reap_command} could not be waited on: {e}"),
        };
        append_launch_log(&reap_log, &line);
    });

    Ok(format!(
        "opened task {task_id} in {} (pid {pid}) — launch log {}",
        project.name,
        launch_log.display()
    ))
}

/// How the session reference on the freshly-recorded session row came to be,
/// for the summary line.
enum RefOutcome {
    /// Captured from the `sessions` listing or the log.
    Captured(String),
    /// The agent defines `sessions` but the reference never showed up.
    NotCaptured,
    /// The agent has no capture story; nothing was attempted.
    NotApplicable,
}

fn spawn_session(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    // Checked here so a human-only task is refused before anything spawns;
    // voro-core's record_dispatch is the backstop.
    if task.human {
        return Err(format!(
            "task {task_id} is human-only — no agent can execute it; work it by hand \
             (`voro start {task_id}`, then `voro done {task_id}`)"
        ));
    }
    if !matches!(task.state, TaskState::Ready | TaskState::Stalled) {
        return Err(format!(
            "only ready or stalled tasks can be dispatched; task {task_id} is {}",
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    // Refused here so an archived project is heard before anything spawns;
    // voro-core's record_dispatch is the backstop.
    if project.archived {
        return Err(format!(
            "project '{}' is archived — `voro project unarchive {}` first",
            project.name, project.name
        ));
    }

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config
        .resolve(agent_override.or(task.agent.as_deref()))
        .map_err(|e| e.to_string())?;

    guard_git_repo(&project.path)?;

    std::fs::create_dir_all(&ctx.runtime_dir)
        .map_err(|e| format!("cannot create {}: {e}", ctx.runtime_dir.display()))?;
    // One stamp names both artefacts, pairing each session's prompt with its
    // log and keeping earlier sessions' files intact across redispatch.
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let prompt_path = ctx
        .runtime_dir
        .join(format!("task-{task_id}-{stamp}.prompt.md"));
    let body = if task.body.is_empty() {
        format!("# {}\n", task.title)
    } else {
        format!("# {}\n\n{}\n", task.title, task.body.trim_end())
    };
    let prompt = format!(
        "{}{body}",
        render_preamble(task_id, &ctx.db_path, task.branch.as_deref())
    );
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("cannot write prompt {}: {e}", prompt_path.display()))?;

    let log_path = ctx.runtime_dir.join(format!("task-{task_id}-{stamp}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create log {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;

    let command = agent
        .dispatch
        .replace(PROMPT_FILE_PLACEHOLDER, &shell_quote(&prompt_path))
        .replace(TASK_ID_PLACEHOLDER, &task_id.to_string());
    let spawn_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&project.path)
        .env("VORO_TASK_ID", task_id.to_string())
        .env("VORO_DB", &ctx.db_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn agent '{}': {e}", agent.name))?;
    let pid = i64::from(child.id());

    let recorded = store.record_dispatch(
        task_id,
        &agent.name,
        Some(pid),
        Some(log_path.to_string_lossy().as_ref()),
    );
    let (task, session) = match recorded {
        Ok(pair) => pair,
        Err(e) => {
            // An agent whose session never got recorded must not keep working
            // unrecorded: kill its process group (pgid = pid, set at spawn)
            // and reap the shell before surfacing the error.
            let _ = Command::new("kill")
                .args(["-TERM", "--"])
                .arg(format!("-{pid}"))
                .status();
            let _ = child.wait();
            return Err(format!(
                "recording the session failed ({e}); the spawned agent (pid {pid}) was killed"
            ));
        }
    };

    // Dispatch returns as soon as the session is recorded, so nothing else
    // waits on the child. Left unreaped it becomes a zombie, and `kill -0` on a
    // zombie still succeeds — permanently defeating reconciliation's
    // pid-liveness check in a long-lived TUI. A detached reaper collects the
    // exit status without blocking dispatch.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let ref_outcome = match &agent.sessions {
        Some(sessions_cmd) => {
            match capture_session_ref(
                sessions_cmd,
                &project.path,
                spawn_ms,
                ctx.ref_capture_timeout,
                &log_path,
            ) {
                Some(session_ref) => {
                    store
                        .set_session_ref(session.id, &session_ref)
                        .map_err(|e| e.to_string())?;
                    RefOutcome::Captured(session_ref)
                }
                None => RefOutcome::NotCaptured,
            }
        }
        None => RefOutcome::NotApplicable,
    };
    let ref_note = match &ref_outcome {
        RefOutcome::Captured(session_ref) => format!(", ref {session_ref}"),
        RefOutcome::NotCaptured => ", session ref not captured".to_string(),
        RefOutcome::NotApplicable => String::new(),
    };

    Ok(format!(
        "dispatched task {} to {} (session {}, pid {}{}) — log {}",
        task.id,
        session.agent,
        session.id,
        pid,
        ref_note,
        log_path.display()
    ))
}

/// Capture the agent's reference for the session just spawned: poll the
/// `sessions` listing for a session started in this project at or after the
/// spawn, falling back to the `backgrounded · <id>` line launchers print into
/// the log. `None` once the timeout passes without either source producing one.
fn capture_session_ref(
    sessions_cmd: &str,
    project_path: &str,
    spawn_ms: i64,
    timeout: Duration,
    log_path: &Path,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(session_ref) = query_new_session(sessions_cmd, project_path, spawn_ms) {
            return Some(session_ref);
        }
        if let Some(session_ref) = log_backgrounded_ref(log_path) {
            return Some(session_ref);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(REF_POLL_INTERVAL);
    }
}

/// One poll of the `sessions` command: the newest listed session whose `cwd`
/// is this project and whose `startedAt` is at or after the spawn (with a
/// little slack for the agent's own clock). `None` on any failure — capture
/// is best-effort and the caller retries until its deadline.
fn query_new_session(sessions_cmd: &str, project_path: &str, spawn_ms: i64) -> Option<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(sessions_cmd)
        .current_dir(project_path)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let entries = parse_sessions_json(&String::from_utf8_lossy(&output.stdout)).ok()?;
    entries
        .into_iter()
        .filter(|e| e.cwd.as_deref() == Some(project_path))
        .filter(|e| {
            e.started_at_ms
                .is_some_and(|t| t >= spawn_ms - SPAWN_CLOCK_SLACK_MS)
        })
        .max_by_key(|e| e.started_at_ms)
        .map(|e| e.session_ref)
}

/// The secondary capture source: `claude --bg`-style launchers print
/// `backgrounded · <id>` (ANSI-coloured) on stdout, which dispatch already
/// redirects into the session log. The id is the line's last token once the
/// colour codes are stripped.
fn log_backgrounded_ref(log_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log_path).ok()?;
    let text = strip_ansi(&text);
    text.lines()
        .find(|line| line.contains("backgrounded"))?
        .split_whitespace()
        .last()
        .filter(|token| *token != "backgrounded" && *token != "·")
        .map(str::to_string)
}

/// Drop ANSI escape sequences (the CSI colour codes launchers wrap their
/// output in) so the log can be tokenised as plain text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out
}

/// Refuse to dispatch into a path that is not a git repository (DESIGN.md §8):
/// the dispatched agent does its work in a git worktree of the checkout, which a
/// non-repo cannot provide, and Voro's on-close cleanup resolves worktrees
/// through git. A path where `git status` cannot run or fails is refused, and
/// the error names the path. The working tree's cleanliness is not inspected:
/// `git worktree add` snapshots HEAD, so the operator's uncommitted changes
/// never enter the agent's diff.
fn guard_git_repo(path: &str) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("cannot run git in {path}: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let detail = detail.trim();
        return Err(if detail.is_empty() {
            format!("cannot verify {path} is a git repository: git status failed")
        } else {
            format!("cannot verify {path} is a git repository: {detail}")
        });
    }
    Ok(())
}

/// The checkout's default branch — what a viewer template's `{base}` diffs a
/// task branch against (DESIGN.md §8). Read from `refs/remotes/origin/HEAD`,
/// whose `--short` form is `origin/<branch>`; the remote prefix is dropped. A
/// checkout with no origin or no symbolic HEAD falls back to `main`.
fn default_base_branch(project_path: &str) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => {
            let head = String::from_utf8_lossy(&o.stdout);
            let head = head.trim();
            head.strip_prefix("origin/").unwrap_or(head).to_string()
        }
        _ => "main".to_string(),
    }
}

/// Single-quote a path for safe substitution into the `sh -c` command line.
pub(crate) fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use voro_core::{NewTask, Priority};

    /// A scratch database, a freshly-`git init`ed clean project, and an
    /// `voro.toml` whose one agent is a stub command that just reads the
    /// prompt. Returns the store, the dispatch context, and the project path.
    fn fixture(cmd: &str) -> (Store, DispatchCtx, PathBuf) {
        fixture_toml(&format!(
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"{cmd}\"\n"
        ))
    }

    /// Like [`fixture`], but with the whole `voro.toml` supplied, for tests
    /// exercising the session verbs.
    fn fixture_toml(agents_toml: &str) -> (Store, DispatchCtx, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "voro-dispatch-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-q"]);

        let db_path = root.join("voro.db");
        let agents_path = root.join("voro.toml");
        std::fs::write(&agents_path, agents_toml).unwrap();

        let store = Store::open(&db_path).unwrap();
        let ctx = DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
            ref_capture_timeout: Duration::ZERO,
            session_task_id: None,
        };
        (store, ctx, project)
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn ready_task(store: &mut Store, project_path: &Path) -> i64 {
        let p = store
            .create_project("proj", project_path.to_str().unwrap())
            .unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                title: "Do the thing".into(),
                body: "Detailed prompt.".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap()
            .id
    }

    #[test]
    fn dispatch_spawns_records_session_and_runs_the_task() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");

        let task = store.task(id).unwrap();
        assert_eq!(task.state, TaskState::Running);

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.agent, "stub");
        assert!(session.pid.is_some_and(|p| p > 0));
        let log = session.log_path.as_deref().unwrap();
        assert!(Path::new(log).exists(), "log file {log} should exist");

        // no sessions verb, so no capture was attempted and none is reported
        assert!(session.session_ref.is_none());
        assert!(!summary.contains("ref"), "{summary}");

        // the prompt carries the task title and body
        let prompt = std::fs::read_to_string(prompt_files(&ctx).pop().unwrap()).unwrap();
        assert!(prompt.contains("Do the thing"));
        assert!(prompt.contains("Detailed prompt."));
    }

    #[test]
    fn dispatched_prompt_injects_the_return_path_preamble() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();

        let prompt = std::fs::read_to_string(prompt_files(&ctx).pop().unwrap()).unwrap();
        // the return-path verbs name the literal task id, no env var
        assert!(prompt.contains(&format!("voro ask {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro resume {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro done {id}")), "{prompt}");
        assert!(prompt.contains("voro propose"), "{prompt}");
        assert!(!prompt.contains("VORO_TASK_ID"), "{prompt}");
        // even with no assigned branch, the agent is told to register the branch
        // it picks (`voro set` and `--branch <name>` asserted apart, since a
        // `--db` flag may sit between them under a non-default store)
        assert!(prompt.contains(&format!("voro set {id}")), "{prompt}");
        assert!(prompt.contains("--branch <name>"), "{prompt}");
        assert!(prompt.contains("Detailed prompt."), "{prompt}");
        // the rendered preamble is dropped in ahead of the task body
        assert!(
            prompt.starts_with(&render_preamble(id, &ctx.db_path, None)),
            "{prompt}"
        );
        assert!(
            prompt.find("# Do the thing").unwrap() > prompt.find("voro done").unwrap(),
            "the task body must follow the preamble"
        );
    }

    #[test]
    fn preamble_renders_a_db_flag_only_for_a_non_default_database() {
        // a scratch (non-default) db renders --db on every verb, shell-quoted
        let db = PathBuf::from("/tmp/scratch/voro.db");
        let rendered = render_preamble(62, &db, None);
        assert!(
            rendered.contains("voro ask 62 --db '/tmp/scratch/voro.db'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("voro done 62 --db '/tmp/scratch/voro.db'"),
            "{rendered}"
        );
        assert!(
            rendered.contains("voro propose <project> \"<title>\" --db '/tmp/scratch/voro.db'"),
            "{rendered}"
        );

        // the default db is what the verbs resolve to unaided, so no flag
        let default = render_preamble(62, &Store::default_db_path(), None);
        assert!(default.contains("voro ask 62 --question"), "{default}");
        assert!(!default.contains("--db"), "{default}");
    }

    #[test]
    fn preamble_tells_both_cases_to_register_the_branch_early() {
        // no assigned branch: the agent is told to register the name it picks,
        // but branch-assignment wording and a completion `voro done --branch`
        // are absent, since no name is known.
        let plain = render_preamble(62, &Store::default_db_path(), None);
        assert!(!plain.contains("git branch `"), "{plain}");
        assert!(plain.contains("voro set 62 --branch <name>"), "{plain}");
        assert!(!plain.contains("voro done 62 --branch"), "{plain}");
        assert!(plain.contains("Voro runs no git"), "{plain}");
        // and the mandatory worktree instruction: work in a throwaway worktree,
        // never in the primary checkout.
        assert!(plain.contains("git worktree add"), "{plain}");
        assert!(plain.contains("never\nmodify it"), "{plain}");
        // stale-branch self-serve: fetch the base and rebase onto it, no push.
        assert!(plain.contains("git fetch origin <base>"), "{plain}");
        assert!(plain.contains("origin/<base>"), "{plain}");
        assert!(plain.contains("never push"), "{plain}");

        // with a branch: the agent is told to use it, register it, and confirm
        // it at completion.
        let branched = render_preamble(62, &Store::default_db_path(), Some("feat/parser"));
        assert!(branched.contains("git branch `feat/parser`"), "{branched}");
        assert!(branched.contains("Voro runs no git"), "{branched}");
        // the assigned case carries the same stale-branch self-serve instruction.
        assert!(branched.contains("git fetch origin <base>"), "{branched}");
        assert!(branched.contains("origin/<base>"), "{branched}");
        assert!(branched.contains("never push"), "{branched}");
        // the worktree instruction carries the assigned branch name.
        assert!(
            branched.contains("git worktree add <path> -b feat/parser"),
            "{branched}"
        );
        assert!(branched.contains("never modify it"), "{branched}");
        assert!(
            branched.contains("voro set 62 --branch feat/parser"),
            "{branched}"
        );
        assert!(
            branched.contains("voro done 62 --branch feat/parser"),
            "{branched}"
        );
    }

    #[test]
    fn dispatched_prompt_injects_the_intended_branch() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        store.set_branch(id, Some("feat/parser")).unwrap();

        dispatch(&mut store, &ctx, id, None).unwrap();

        let prompt = std::fs::read_to_string(prompt_files(&ctx).pop().unwrap()).unwrap();
        assert!(prompt.contains("git branch `feat/parser`"), "{prompt}");
        // registered (`voro set`) and confirmed (`voro done`); id and `--branch`
        // asserted apart, since a `--db` flag may sit between them
        assert!(prompt.contains(&format!("voro set {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro done {id}")), "{prompt}");
        assert!(prompt.contains("--branch feat/parser"), "{prompt}");
    }

    #[test]
    fn dispatch_substitutes_the_task_id_into_the_command() {
        // the stub command writes {task_id} to a marker file, so a successful
        // run proves the placeholder reached the spawned command line
        let (mut store, ctx, project) = fixture("cat {prompt_file} && echo {task_id} > marker.txt");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();

        let marker = project.join("marker.txt");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap().trim(),
            id.to_string()
        );
    }

    #[test]
    fn dispatch_without_the_task_id_placeholder_is_unchanged() {
        // a template that never mentions {task_id} dispatches exactly as before
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
    }

    fn prompt_files(ctx: &DispatchCtx) -> Vec<PathBuf> {
        std::fs::read_dir(&ctx.runtime_dir)
            .unwrap()
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.to_string_lossy().ends_with(".prompt.md"))
            .collect()
    }

    #[test]
    fn redispatch_keeps_each_sessions_prompt_and_log() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();
        store.apply(id, voro_core::Action::Abort).unwrap();
        dispatch(&mut store, &ctx, id, None).unwrap();

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_ne!(
            sessions[0].log_path, sessions[1].log_path,
            "each session must own its log"
        );
        assert_eq!(prompt_files(&ctx).len(), 2);
    }

    #[test]
    fn a_dirty_checkout_dispatches() {
        // The dispatched agent works in a throwaway worktree that snapshots
        // HEAD, so uncommitted changes in the primary checkout do not block
        // dispatch: it proceeds and records the session.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        std::fs::write(project.join("scratch.txt"), "uncommitted").unwrap();

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
        assert_eq!(store.sessions_for(id).unwrap().len(), 1);
    }

    #[test]
    fn a_non_git_path_is_refused_naming_the_path() {
        // A path that is not a git repository is refused, with the path in the
        // error.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        std::fs::remove_dir_all(project.join(".git")).unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains(project.to_str().unwrap()), "{err}");
        // nothing was dispatched
        assert_eq!(store.task(id).unwrap().state, TaskState::Ready);
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    #[test]
    fn unknown_agent_override_is_refused_before_spawning() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        let err = dispatch(&mut store, &ctx, id, Some("gemini")).unwrap_err();
        assert!(err.contains("gemini"), "{err}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Ready);
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    #[test]
    fn only_ready_or_stalled_tasks_dispatch() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        store.apply(id, voro_core::Action::Park).unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("ready or stalled"), "{err}");
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    /// Redispatching a stalled task is the whole point of the state (DESIGN.md
    /// §6/§8): dispatch's precondition accepts `stalled → running` too.
    #[test]
    fn a_stalled_task_redispatches() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);

        dispatch(&mut store, &ctx, id, None).unwrap();
        let session = &store.sessions_for(id).unwrap()[0];
        store.reconcile_session(session.id, false, false).unwrap();
        assert_eq!(store.task(id).unwrap().state, TaskState::Stalled);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("dispatched task"), "{summary}");
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
        assert_eq!(store.sessions_for(id).unwrap().len(), 2);
    }

    // --- session-ref capture (task #75) ---

    /// A canned `sessions` listing whose one entry matches the dispatched
    /// project's cwd with a far-future start time, so the first capture poll
    /// finds it — the stub for `claude agents --json`.
    fn write_sessions_json(root: &Path, project: &Path, session_ref: &str) -> PathBuf {
        let listing = root.join("sessions.json");
        std::fs::write(
            &listing,
            format!(
                "[{{\"pid\": 1, \"id\": \"short123\", \"cwd\": \"{}\", \
                 \"startedAt\": 99999999999999, \"sessionId\": \"{session_ref}\", \
                 \"status\": \"idle\", \"state\": \"working\"}}]",
                project.display()
            ),
        )
        .unwrap();
        listing
    }

    #[test]
    fn dispatch_captures_the_session_ref_from_the_sessions_listing() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nsessions = \"cat sessions.json\"\n",
        );
        let root = project.parent().unwrap().to_path_buf();
        let listing = write_sessions_json(&root, &project, "3f6c-full-uuid");
        // the sessions command runs in the project path
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}}\"\nsessions = \"cat '{}'\"\n",
                listing.display()
            ),
        )
        .unwrap();
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("ref 3f6c-full-uuid"), "{summary}");

        let session = &store.sessions_for(id).unwrap()[0];
        assert_eq!(session.session_ref.as_deref(), Some("3f6c-full-uuid"));
    }

    #[test]
    fn capture_gives_up_gracefully_when_nothing_matches() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nsessions = \"echo []\"\n",
        );
        let id = ready_task(&mut store, &project);

        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("session ref not captured"), "{summary}");
        assert!(
            store.sessions_for(id).unwrap()[0].session_ref.is_none(),
            "the ref stays NULL when capture fails"
        );
        // the dispatch itself still succeeded
        assert_eq!(store.task(id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn capture_falls_back_to_the_backgrounded_log_line() {
        // The dispatch stub prints the ANSI-coloured `backgrounded · <id>`
        // line a real `claude --bg` launcher prints; the sessions listing
        // never matches (empty array), so capture must fall back to the log.
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = 'cat {prompt_file} >/dev/null && printf \"\\033[2mbackgrounded · \\033[1mdeadbeef\\033[0m\\n\"'\n\
             sessions = \"echo []\"\n",
        );
        let id = ready_task(&mut store, &project);

        // give the stub time to write the line before capture's single poll
        let ctx = DispatchCtx {
            ref_capture_timeout: Duration::from_secs(3),
            session_task_id: None,
            ..ctx
        };
        let summary = dispatch(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains("ref deadbeef"), "{summary}");
        assert_eq!(
            store.sessions_for(id).unwrap()[0].session_ref.as_deref(),
            Some("deadbeef")
        );
    }

    #[test]
    fn strip_ansi_and_log_parsing_lift_the_backgrounded_id() {
        assert_eq!(
            strip_ansi("\u{1b}[38;5;245mbackgrounded · \u{1b}[1mdeadbeef\u{1b}[0m"),
            "backgrounded · deadbeef"
        );
        let log = std::env::temp_dir().join(format!("voro-bg-line-{}.log", std::process::id()));
        std::fs::write(
            &log,
            "warming up\n\u{1b}[2mbackgrounded · \u{1b}[1mcafe1234\u{1b}[0m\nmanage with: claude attach\n",
        )
        .unwrap();
        assert_eq!(log_backgrounded_ref(&log).as_deref(), Some("cafe1234"));
        std::fs::remove_file(&log).unwrap();
    }

    /// A ready task moved into `review` through the transition machine, so
    /// `open`'s state guard is exercised against a genuine review row.
    fn review_task(store: &mut Store, project_path: &Path) -> i64 {
        let id = ready_task(store, project_path);
        store.apply(id, voro_core::Action::Start).unwrap();
        store.apply(id, voro_core::Action::Complete(None)).unwrap();
        id
    }

    #[test]
    fn open_runs_the_configured_viewer_in_the_project_path() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // a [viewer] whose command drops a marker at the substituted {path}
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"touch {path}/opened.marker\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        let summary = open(&mut store, &ctx, id, None).unwrap();
        assert!(summary.contains(&format!("opened task {id}")), "{summary}");

        // the viewer is spawned detached, so wait briefly for it to run
        let marker = project.join("opened.marker");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(marker.exists(), "the viewer should have run in the project");
    }

    /// Give the checkout one commit and a worktree on `branch` beside it, so
    /// `open` has a real worktree to resolve. Returns the worktree path.
    fn commit_and_worktree(project: &Path, branch: &str) -> PathBuf {
        git(project, &["config", "user.email", "t@example.com"]);
        git(project, &["config", "user.name", "Test"]);
        std::fs::write(project.join("README"), "hi\n").unwrap();
        git(project, &["add", "-A"]);
        git(project, &["commit", "-q", "-m", "init"]);
        let wt = project.parent().unwrap().join(format!("wt-{branch}"));
        git(
            project,
            &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
        );
        wt
    }

    #[test]
    fn open_runs_the_viewer_in_the_tasks_worktree_and_falls_back_to_the_checkout() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // a [viewer] whose command drops a marker at the substituted {path}
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"touch {path}/opened.marker\"\n",
        )
        .unwrap();
        let wt = commit_and_worktree(&project, "feat");

        // A task on `feat` has a live worktree, so the viewer opens there.
        let with_wt = review_task(&mut store, &project);
        store.set_branch(with_wt, Some("feat")).unwrap();
        open(&mut store, &ctx, with_wt, None).unwrap();
        let wt_marker = wt.join("opened.marker");
        for _ in 0..50 {
            if wt_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            wt_marker.exists(),
            "the viewer should have run in the worktree, not the checkout"
        );

        // A task on a branch with no worktree falls back to the checkout (a
        // second task in the same project, since the project name is unique).
        let project_id = store.task(with_wt).unwrap().project_id;
        let no_wt = store
            .create_task(NewTask {
                project_id,
                title: "No worktree".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap()
            .id;
        store.apply(no_wt, voro_core::Action::Start).unwrap();
        store
            .apply(no_wt, voro_core::Action::Complete(None))
            .unwrap();
        store.set_branch(no_wt, Some("ghost")).unwrap();
        open(&mut store, &ctx, no_wt, None).unwrap();
        let checkout_marker = project.join("opened.marker");
        for _ in 0..50 {
            if checkout_marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            checkout_marker.exists(),
            "a branch with no worktree must fall back to the checkout"
        );
    }

    #[test]
    fn open_substitutes_the_branch_and_base_placeholders() {
        // A viewer template using all three placeholders to spell a diff range;
        // {base} has no origin here, so it falls back to `main`.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"echo {base}...{branch} > {path}/range.txt\"\n",
        )
        .unwrap();
        let wt = commit_and_worktree(&project, "feat");
        let id = review_task(&mut store, &project);
        store.set_branch(id, Some("feat")).unwrap();

        open(&mut store, &ctx, id, None).unwrap();

        // {path} resolved to the worktree, {base}...{branch} to `main...feat`.
        let range = wt.join("range.txt");
        for _ in 0..50 {
            if range.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            std::fs::read_to_string(&range).unwrap().trim(),
            "main...feat"
        );
    }

    #[test]
    fn open_ignores_new_placeholders_a_template_does_not_use() {
        // A template that mentions neither {branch} nor {base} is substituted
        // exactly as before — only {path} changes, and it lands unquoted-content
        // in the checkout since the task carries no branch.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"echo plain > {path}/plain.txt\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project); // no branch set

        open(&mut store, &ctx, id, None).unwrap();

        let plain = project.join("plain.txt");
        for _ in 0..50 {
            if plain.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&plain).unwrap().trim(), "plain");
    }

    #[test]
    fn open_records_a_failing_viewers_exit_status_in_the_launch_log() {
        // a [viewer] whose command exits non-zero after spawning: the failure
        // is silent on the (null-less) detached child, so it must be findable
        // in the launch log, and the summary must say where that log is.
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"exit 3\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        let summary = open(&mut store, &ctx, id, None).unwrap();
        let launch_log = ctx.launch_log_path();
        assert!(
            summary.contains(&launch_log.display().to_string()),
            "{summary}"
        );

        // the reap thread records the exit status asynchronously
        let mut contents = String::new();
        for _ in 0..50 {
            contents = std::fs::read_to_string(&launch_log).unwrap_or_default();
            if contents.contains("exited with") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(contents.contains("viewer:"), "{contents}");
        assert!(contents.contains("exit status: 3"), "{contents}");
    }

    #[test]
    fn open_without_a_viewer_reports_what_to_configure() {
        // the fixture's voro.toml defines no viewer at all
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = review_task(&mut store, &project);

        let err = open(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("[viewers.<name>]"), "{err}");
        assert!(err.contains(ctx.agents_path.to_str().unwrap()), "{err}");
    }

    #[test]
    fn open_refuses_a_task_that_is_not_review_or_running() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project); // still ready

        let err = open(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("review or running"), "{err}");
    }

    /// The two named-viewer selection paths (DESIGN.md §8/§11a): an explicit
    /// override picks its `[viewers.<name>]` entry over the default, and with
    /// no override the project's `viewer:<name>` review action picks one.
    #[test]
    fn open_picks_the_named_viewer_from_override_or_project_action() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewer]\ncmd = \"touch {path}/default.marker\"\n\n\
             [viewers.special]\ncmd = \"touch {path}/special.marker\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        open(&mut store, &ctx, id, Some("special")).unwrap();
        let marker = project.join("special.marker");
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "the override must pick [viewers.special]");
        std::fs::remove_file(&marker).unwrap();

        let project_id = store.task(id).unwrap().project_id;
        store
            .set_review_action(
                project_id,
                &voro_core::ReviewAction::Viewer(Some("special".into())),
            )
            .unwrap();
        open(&mut store, &ctx, id, None).unwrap();
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            marker.exists(),
            "the project's viewer:special action must pick [viewers.special]"
        );
        assert!(
            !project.join("default.marker").exists(),
            "the default [viewer] must not have run"
        );
    }

    /// An unknown viewer name errors naming the known ones rather than
    /// silently falling back to the default.
    #[test]
    fn open_reports_an_unknown_viewer_name() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [viewers.zed]\ncmd = \"true\"\n",
        )
        .unwrap();
        let id = review_task(&mut store, &project);

        let err = open(&mut store, &ctx, id, Some("emacs")).unwrap_err();
        assert!(err.contains("emacs"), "{err}");
        assert!(err.contains("zed"), "{err}");
    }

    // --- planning sessions (task #112) ---

    #[test]
    fn plan_session_assembles_the_launch_and_writes_the_prompt() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\nplan = \"stub --interactive {prompt_file}\"\n",
        );
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();
        // a dirty checkout is fine for planning
        std::fs::write(project.join("scratch.txt"), "uncommitted").unwrap();

        let launch = plan_session(&store, &ctx, p.id).unwrap();
        assert_eq!(launch.cwd, project.to_str().unwrap());

        // the plan template ran through the same {prompt_file} substitution as
        // dispatch, pointing at a prompt written outside the checkout
        let prompt_path = prompt_files(&ctx).pop().unwrap();
        assert_eq!(
            launch.command,
            format!("stub --interactive {}", shell_quote(&prompt_path))
        );

        let prompt = std::fs::read_to_string(&prompt_path).unwrap();
        assert!(prompt.contains("plan a new task"), "{prompt}");
        assert!(prompt.contains("`proj`"), "{prompt}");
        assert!(prompt.contains("voro add 'proj' \"<title>\""), "{prompt}");
        assert!(prompt.contains("--body-file"), "{prompt}");
        assert!(prompt.contains("acceptance criteria"), "{prompt}");
        assert!(prompt.contains("`proposed`"), "{prompt}");
        // the fixture database is not the default, so every command carries it
        assert!(
            prompt.contains(&format!("--db {}", shell_quote(&ctx.db_path))),
            "{prompt}"
        );

        // no dispatch happened: no session row, no state change, no task at all
        assert!(store.tasks().unwrap().is_empty());
    }

    #[test]
    fn planning_prompt_renders_the_db_flag_only_for_a_non_default_database() {
        let rendered = render_planning_prompt("proj", &Store::default_db_path());
        assert!(
            rendered.contains("voro add 'proj' \"<title>\" --body-file"),
            "{rendered}"
        );
        assert!(!rendered.contains("--db"), "{rendered}");
    }

    #[test]
    fn plan_without_a_plan_verb_reports_what_to_configure() {
        // the fixture's stub agent defines only dispatch
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();

        let err = plan_session(&store, &ctx, p.id).unwrap_err();
        assert!(err.contains("stub"), "{err}");
        assert!(err.contains("plan"), "{err}");
        assert!(err.contains("{prompt_file}"), "{err}");
        assert!(err.contains(ctx.agents_path.to_str().unwrap()), "{err}");
    }

    #[test]
    fn task_override_selects_the_agent() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // add a second agent and set it as the task's override
        std::fs::write(
            &ctx.agents_path,
            "default_agent = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
             [agents.special]\ncmd = \"cat {prompt_file}\"\n",
        )
        .unwrap();
        let p = store
            .create_project("proj", project.to_str().unwrap())
            .unwrap();
        let id = store
            .create_task(NewTask {
                project_id: p.id,
                title: "T".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: Some("special".into()),
                human: false,
            })
            .unwrap()
            .id;

        dispatch(&mut store, &ctx, id, None).unwrap();
        assert_eq!(store.sessions_for(id).unwrap()[0].agent, "special");
    }
}
