//! Dispatching a ready task to a headless agent session (DESIGN.md §8), and
//! continuing an already-`running` one after a human answers a question
//! (DESIGN.md §6). Both are the I/O half of dispatch — resolving the agent,
//! guarding against a dirty checkout, writing the prompt, and spawning the
//! process detached — kept out of voro-core, which stays pure of process and
//! filesystem I/O. The atomic state-plus-session writes are voro-core's
//! `Store::record_dispatch` and `Store::record_continuation`.
//!
//! For agents that define a `sessions` verb (task #75), dispatch additionally
//! captures the agent's own session reference after launch — by polling the
//! `sessions` listing for a session started in this project since the spawn,
//! falling back to the `backgrounded · <id>` line launchers print into the
//! log — and records it on the session row for later attach/resume/continue.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use voro_core::{
    AgentsConfig, PROMPT_FILE_PLACEHOLDER, SESSION_PLACEHOLDER, Session, Store,
    TASK_ID_PLACEHOLDER, Task, TaskState, VIEWER_PATH_PLACEHOLDER, parse_sessions_json,
};

/// Rendered per dispatch and prepended verbatim to every prompt so the agent
/// learns the return-path verbs (DESIGN.md §8) with no per-project install and no
/// reliance on a skill being loaded — the dispatcher already owns the prompt
/// file, so injection is the most robust delivery. The `{task_id}` and `{db}`
/// placeholders are filled by [`render_preamble`] so every verb names its task
/// and database literally, rather than relying on inherited `VORO_TASK_ID`/
/// `VORO_DB` env vars — those do not survive a launcher that hands the real
/// session to a supervisor daemon (e.g. `claude --bg`), the exact launch style
/// the starter config ships. Deliberately says nothing about `voro start`:
/// dispatch has already transitioned the task `ready → running`. Kept as a
/// single template so the injected text cannot drift; the voro-cli SKILL.md is
/// the richer, dev-facing version covering manual runs.
const RETURN_PATH_PREAMBLE_TEMPLATE: &str = "\
<!-- Voro dispatch: how to report back -->
You are an agent dispatched by Voro on task {task_id}. Voro tracks this task in a
local SQLite database; report progress through these CLI verbs rather than
editing that database yourself:

    voro ask {task_id}{db} --question \"...\"     # blocked on a human decision (-> needs-input)
    voro done {task_id}{db} --summary \"...\"      # work finished, await review (-> review)
    voro propose <project> \"<title>\"{db} --body-file <path>   # file a follow-up task (-> proposed)

Run these commands exactly as shown: they name task {task_id} explicitly, so they
work from anywhere without any environment variable being set. `propose` uses
task {task_id} as the discovered-from source when `--from` is omitted. Finish
with your work committed on a branch and a PR-ready `--summary` on `done` — what
changed, why, and how you verified it — since `voro pr` opens the pull request
straight from that summary. Never modify the database with raw SQL, which would
bypass the state machine and event log.{branch}

---

";

/// The one sentence shared by both branch blocks below, so the early-registration
/// instruction cannot drift between them (task #96): the agent registers the
/// branch it will work on with Voro the moment it exists, letting reconcile,
/// attach, `voro pr`, and the UI reflect the real branch while the task is still
/// `running` — and capturing it even if the agent never reaches a clean `done`.
/// `{name}` is the concrete branch (assigned case) or the `<name>` the agent will
/// choose; `{task_id}`/`{db}` keep the command copy-pasteable under launch styles
/// that drop the environment (same rationale as the return-path verbs).
const BRANCH_REGISTER_SENTENCE: &str = "register it with `voro set {task_id}{db} --branch {name}` as you do, so Voro \
     tracks the real branch while the task runs";

/// The `{branch}` block [`render_preamble`] substitutes when the task carries an
/// intended git branch (task #81): the agent is told the name, to create or check
/// it out itself (Voro runs no git), to register it early via
/// [`BRANCH_REGISTER_SENTENCE`], and to confirm the branch its work landed on at
/// completion. `{name}` is the branch name and `{task_id}`/`{db}` keep the verbs
/// copy-pasteable.
const ASSIGNED_BRANCH_TEMPLATE: &str = "\n\n\
This task is assigned the git branch `{name}`. Create or check it out yourself
before making changes — Voro runs no git — and {register}. Confirm the branch your
work landed on with `voro done {task_id}{db} --branch {name}`.";

/// The `{branch}` block when no branch is assigned: the agent picks its own name
/// and must still register it early via [`BRANCH_REGISTER_SENTENCE`], so Voro
/// learns the branch while the task runs rather than only at `done`.
const UNASSIGNED_BRANCH_TEMPLATE: &str = "\n\n\
Pick a git branch for this work, create or check it out — Voro runs no git — and
{register}; `<name>` is the name you choose.";

/// How often the session-ref capture re-polls the agent's `sessions` command
/// while waiting for the freshly-launched session to appear in the listing.
const REF_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Clock slack when matching a listing entry's `startedAt` against the spawn
/// time: the agent stamps its session with its own clock, which may sit a
/// beat behind the timestamp taken here just before the spawn.
const SPAWN_CLOCK_SLACK_MS: i64 = 2000;

/// Fill [`RETURN_PATH_PREAMBLE_TEMPLATE`] for a concrete dispatch: substitute the
/// literal task id into every verb, and add a `--db <path>` flag to each verb
/// only when the dispatching database is not the default one the verbs resolve to
/// on their own. Putting both values in the visible command is what makes the
/// return path robust under launch styles that drop the spawned process's
/// environment.
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
            .replace("{name}", name),
        None => UNASSIGNED_BRANCH_TEMPLATE.replace("{register}", &register),
    };
    RETURN_PATH_PREAMBLE_TEMPLATE
        .replace("{branch}", &branch_block)
        .replace("{task_id}", &task_id.to_string())
        .replace("{db}", &db_flag)
}

/// Which of the two flows `spawn_session` is performing — they share every
/// mechanic (agent resolution, the dirty-tree guard, prompt/log files,
/// spawning and reaping the process) and differ only in which state the task
/// must already be in and which `Store` method records the result.
#[derive(Clone, Copy)]
enum SpawnKind {
    /// A fresh dispatch: the task must be `ready` — or `stalled`, redispatch
    /// being the whole point of that state (DESIGN.md §6/§8); recording it
    /// performs the transition to `running`.
    Fresh,
    /// A continuation: the task must already be `running` — `answer` just put
    /// it there — so recording it only opens a session, never changes state.
    Continuation,
}

impl SpawnKind {
    fn accepts_state(self, state: TaskState) -> bool {
        match self {
            SpawnKind::Fresh => matches!(state, TaskState::Ready | TaskState::Stalled),
            SpawnKind::Continuation => state == TaskState::Running,
        }
    }

    /// The states `accepts_state` allows, for the rejection message.
    fn accepted_states(self) -> &'static str {
        match self {
            SpawnKind::Fresh => "ready or stalled",
            SpawnKind::Continuation => "running",
        }
    }

    /// Past tense, for both the rejection message and the success summary.
    fn verb_past(self) -> &'static str {
        match self {
            SpawnKind::Fresh => "dispatched",
            SpawnKind::Continuation => "continued",
        }
    }

    fn preposition(self) -> &'static str {
        match self {
            SpawnKind::Fresh => "to",
            SpawnKind::Continuation => "on",
        }
    }

    fn record(
        self,
        store: &mut Store,
        task_id: i64,
        agent: &str,
        pid: Option<i64>,
        log_path: Option<&str>,
    ) -> voro_core::Result<(Task, Session)> {
        match self {
            SpawnKind::Fresh => store.record_dispatch(task_id, agent, pid, log_path),
            SpawnKind::Continuation => store.record_continuation(task_id, agent, pid, log_path),
        }
    }
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
    /// so writing the prompt does not itself dirty the tree.
    pub runtime_dir: PathBuf,
    /// How long to keep polling for a session reference after spawning an
    /// agent that defines a `sessions` verb, before giving up (the ref stays
    /// NULL and the summary says so). Zero means a single attempt.
    pub ref_capture_timeout: Duration,
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
        }
    }

    /// The rolling log for subprocess launches that are *not* dispatches —
    /// viewer opens and attach/resume round-trips. Dispatch and continuation
    /// each own a per-session log (`log_path`); these one-off launches share a
    /// single append-only file beside those session logs so a failure that
    /// would otherwise be painted over by the TUI leaves a breadcrumb. Single
    /// file, no rotation — the same single-operator argument as session logs
    /// (DESIGN.md §8).
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
/// picker flag, which outranks the task's own override (DESIGN.md §8). Every
/// check that can fail — task state, agent resolution, the dirty-tree guard,
/// writing the prompt — runs before the process is spawned, so a failed
/// dispatch never leaves an orphan session.
pub fn dispatch(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    spawn_session(store, ctx, task_id, agent_override, SpawnKind::Fresh, None)
}

/// Continue a task that `answer` just moved `needs-input → running` (DESIGN.md
/// §6). When the agent defines a `continue` verb and the task's last session
/// has a captured reference, the answer is fed to *that session* headless —
/// `new_input` (the answer text) becomes the prompt file substituted into the
/// template alongside `{session}`. Otherwise this falls back to what it always
/// did: spawn a fresh session whose prompt is the task body — now carrying the
/// `## Answers` section the answer appended — so the work resumes with the
/// answer in hand. Shares every mechanic with [`dispatch`] except the required
/// starting state and how the result is recorded: `record_continuation`
/// asserts the task is already `running` instead of performing the `ready →
/// running` transition itself, so this can never be used to smuggle a task
/// into `running` outside the transition API.
pub fn continue_dispatch(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
    new_input: Option<&str>,
) -> Result<String, String> {
    spawn_session(
        store,
        ctx,
        task_id,
        agent_override,
        SpawnKind::Continuation,
        new_input,
    )
}

/// Open a `review` (or `running`) task's checkout in the configured viewer so
/// its diff can be seen (DESIGN.md §11a). The `[viewer]` command from
/// `voro.toml` is run detached in the project's path — a shell-out baseline
/// that reuses the command-template model rather than hard-coding an editor.
/// With no viewer configured, the caller gets back what to add rather than a
/// silent no-op; opening never touches task state, so no clean-tree guard and
/// no `Store` mutation are involved (the diff being reviewed is often the
/// uncommitted work itself).
pub fn open(store: &mut Store, ctx: &DispatchCtx, task_id: i64) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    if !matches!(task.state, TaskState::Review | TaskState::Running) {
        return Err(format!(
            "only review or running tasks can be opened in a viewer; task {task_id} is {}",
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let viewer = config.viewer().ok_or_else(|| {
        format!(
            "no viewer configured; add a [viewer] table to {} with a cmd such as \
             'zed {{path}}' or 'git difftool -d' to see a task's diff",
            ctx.agents_path.display()
        )
    })?;

    let command = viewer.replace(
        VIEWER_PATH_PLACEHOLDER,
        &shell_quote(Path::new(&project.path)),
    );

    // A detached viewer's output was previously discarded to `/dev/null`, so a
    // failing viewer command was completely silent. Redirect it to the shared
    // launch log instead, and record the exit status there when the reap thread
    // collects it, so a silent failure has a breadcrumb.
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
        &format!("viewer: {command} (cwd {})", project.path),
    );

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&project.path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn viewer in {}: {e}", project.path))?;
    let pid = i64::from(child.id());

    // Nothing waits on the viewer — like dispatch, reap it in a detached thread
    // so an exited child never lingers as a zombie in a long-lived TUI session.
    // The reap records the exit status so a viewer that failed after spawning is
    // findable in the log, not just one that failed to spawn.
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
    /// Captured from the `sessions` listing or the log, or inherited from the
    /// prior session on a `continue`-verb continuation.
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
    kind: SpawnKind,
    new_input: Option<&str>,
) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    // Checked here so a human-only task is refused before anything spawns;
    // voro-core's record_dispatch/record_continuation are the backstop.
    if task.human {
        return Err(format!(
            "task {task_id} is human-only — no agent can execute it; work it by hand \
             (`voro start {task_id}`, then `voro done {task_id}`)"
        ));
    }
    if !kind.accepts_state(task.state) {
        return Err(format!(
            "only {} tasks can be {}; task {task_id} is {}",
            kind.accepted_states(),
            kind.verb_past(),
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config
        .resolve(agent_override.or(task.agent.as_deref()))
        .map_err(|e| e.to_string())?;

    // A continuation reuses the prior session when the agent knows how to
    // (`continue` verb) and the session can be addressed (captured ref);
    // otherwise it degrades to a fresh spawn of the dispatch template.
    let continue_ref = match kind {
        SpawnKind::Continuation if agent.continue_cmd.is_some() => store
            .sessions_for(task_id)
            .map_err(|e| e.to_string())?
            .first()
            .and_then(|s| s.session_ref.clone()),
        _ => None,
    };

    guard_clean_tree(&project.path)?;

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
    // A continued session already carries the preamble and the task from its
    // first prompt, so it gets only the new input (the answer); everything
    // else gets the full preamble + body.
    let prompt = match (&continue_ref, new_input) {
        (Some(_), Some(input)) => format!("{input}\n"),
        (Some(_), None) => body,
        (None, _) => format!(
            "{}{body}",
            render_preamble(task_id, &ctx.db_path, task.branch.as_deref())
        ),
    };
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("cannot write prompt {}: {e}", prompt_path.display()))?;

    let log_path = ctx.runtime_dir.join(format!("task-{task_id}-{stamp}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create log {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;

    let command = match &continue_ref {
        Some(session_ref) => agent
            .continue_cmd
            .as_deref()
            .expect("continue_ref is only set when the verb exists")
            .replace(SESSION_PLACEHOLDER, &shell_quote(Path::new(session_ref)))
            .replace(PROMPT_FILE_PLACEHOLDER, &shell_quote(&prompt_path)),
        None => agent
            .dispatch
            .replace(PROMPT_FILE_PLACEHOLDER, &shell_quote(&prompt_path))
            .replace(TASK_ID_PLACEHOLDER, &task_id.to_string()),
    };
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

    let recorded = kind.record(
        store,
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

    // Nothing in this process waits on the child otherwise, since dispatch
    // must return as soon as the session is recorded. Left unreaped, an
    // exited child sits as a zombie for as long as this process runs — and
    // `kill -0` on a zombie still succeeds, which would permanently defeat
    // reconciliation's pid-liveness check in a long-lived TUI session. A
    // detached reaper thread collects the exit status the moment it's
    // available without blocking dispatch.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    let ref_outcome = match (continue_ref, &agent.sessions) {
        // A continue-verb continuation addressed the prior session, so the
        // new row keeps addressing it.
        (Some(session_ref), _) => {
            let result = store.set_session_ref(session.id, &session_ref);
            result.map_err(|e| e.to_string())?;
            RefOutcome::Captured(session_ref)
        }
        (None, Some(sessions_cmd)) => {
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
        (None, None) => RefOutcome::NotApplicable,
    };
    let ref_note = match &ref_outcome {
        RefOutcome::Captured(session_ref) => format!(", ref {session_ref}"),
        RefOutcome::NotCaptured => ", session ref not captured".to_string(),
        RefOutcome::NotApplicable => String::new(),
    };

    Ok(format!(
        "{} task {} {} {} (session {}, pid {}{}) — log {}",
        kind.verb_past(),
        task.id,
        kind.preposition(),
        session.agent,
        session.id,
        pid,
        ref_note,
        log_path.display()
    ))
}

/// Capture the agent's reference for the session just spawned: poll the
/// agent's `sessions` listing for a session started in this project at or
/// after the spawn, falling back to the `backgrounded · <id>` line launchers
/// print (which lands in the log, since stdout is redirected there). `None`
/// once the timeout passes without either source producing a reference.
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

/// Refuse to dispatch into a checkout with uncommitted changes (the v1 guard,
/// DESIGN.md §8 and §11): the agent's work must land on a clean base so its
/// diff is reviewable. A path that is not a git repository can't be verified
/// as clean, so it is refused for the same reason. The error names the path.
fn guard_clean_tree(path: &str) -> Result<(), String> {
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
            format!("cannot verify {path} is clean: git status failed")
        } else {
            format!("cannot verify {path} is clean: {detail}")
        });
    }
    if !output.stdout.is_empty() {
        return Err(format!(
            "{path} has uncommitted changes; commit or stash before dispatching"
        ));
    }
    Ok(())
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
        // all three return-path verbs name the literal task id, no env var
        assert!(prompt.contains(&format!("voro ask {id}")), "{prompt}");
        assert!(prompt.contains(&format!("voro done {id}")), "{prompt}");
        assert!(prompt.contains("voro propose"), "{prompt}");
        assert!(!prompt.contains("VORO_TASK_ID"), "{prompt}");
        // even with no assigned branch, the agent is told to register the branch
        // it picks the moment it exists — `voro set` naming the literal task id,
        // with the `<name>` placeholder it will fill in (a `--db` flag may sit
        // between the two under a non-default store)
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
        // no assigned branch: the agent picks a name, but is still told to
        // register it early with `voro set`, naming the literal task id. The
        // branch-*assignment* wording (naming a specific branch) is absent, and
        // there is no completion `voro done --branch`, since no name is known.
        let plain = render_preamble(62, &Store::default_db_path(), None);
        assert!(!plain.contains("git branch `"), "{plain}");
        assert!(plain.contains("voro set 62 --branch <name>"), "{plain}");
        assert!(!plain.contains("voro done 62 --branch"), "{plain}");
        assert!(plain.contains("Voro runs no git"), "{plain}");

        // with a branch: the agent is told to use it, to register it early with
        // `voro set`, and how to confirm it at completion — every verb naming the
        // literal task id and the concrete branch.
        let branched = render_preamble(62, &Store::default_db_path(), Some("feat/parser"));
        assert!(branched.contains("git branch `feat/parser`"), "{branched}");
        assert!(branched.contains("Voro runs no git"), "{branched}");
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
        // the branch is both registered early (`voro set`) and confirmed at
        // completion (`voro done`); each names the literal task id and branch,
        // though a `--db` flag may sit between id and `--branch` under a
        // non-default store, so the two are asserted apart
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
    fn dirty_tree_is_refused_naming_the_path() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        std::fs::write(project.join("scratch.txt"), "uncommitted").unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("uncommitted"), "{err}");
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

    // --- continue-verb continuation (task #75) ---

    /// A task dispatched and left `running`, its session's ref set as if
    /// capture had recorded it.
    fn dispatched_with_ref(store: &mut Store, ctx: &DispatchCtx, project: &Path) -> i64 {
        let id = ready_task(store, project);
        dispatch(store, ctx, id, None).unwrap();
        let session_id = store.sessions_for(id).unwrap()[0].id;
        store.set_session_ref(session_id, "ref-123").unwrap();
        id
    }

    #[test]
    fn continuation_reuses_the_session_via_the_continue_verb() {
        let (mut store, ctx, project) = fixture_toml("placeholder — rewritten below");
        let root = project.parent().unwrap().to_path_buf();
        let ref_out = root.join("cont-ref.txt");
        let prompt_out = root.join("cont-prompt.txt");
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}}\"\n\
                 continue = \"cp {{prompt_file}} '{}' && echo {{session}} > '{}'\"\n",
                prompt_out.display(),
                ref_out.display()
            ),
        )
        .unwrap();
        let id = dispatched_with_ref(&mut store, &ctx, &project);

        let summary =
            continue_dispatch(&mut store, &ctx, id, None, Some("Schema B, then.")).unwrap();
        assert!(summary.contains("continued task"), "{summary}");
        assert!(summary.contains("ref ref-123"), "{summary}");

        // the continue template ran with the prior session's ref substituted
        for _ in 0..50 {
            if ref_out.exists() && prompt_out.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(std::fs::read_to_string(&ref_out).unwrap().trim(), "ref-123");
        // the prompt fed to the continuation is the answer, not the re-sent
        // task body — the session already has the task
        let prompt = std::fs::read_to_string(&prompt_out).unwrap();
        assert_eq!(prompt.trim(), "Schema B, then.");
        assert!(!prompt.contains("voro done"), "no preamble on a continue");

        // the continuation's session row inherits the ref it addressed
        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_ref.as_deref(), Some("ref-123"));
    }

    #[test]
    fn continuation_without_a_ref_falls_back_to_a_fresh_spawn() {
        let (mut store, ctx, project) = fixture_toml(
            "default_agent = \"stub\"\n\n[agents.stub]\n\
             dispatch = \"cat {prompt_file}\"\n\
             continue = \"true {session} {prompt_file}\"\n",
        );
        let id = ready_task(&mut store, &project);
        dispatch(&mut store, &ctx, id, None).unwrap();
        // no set_session_ref: capture never happened for this agent

        continue_dispatch(&mut store, &ctx, id, None, Some("B")).unwrap();

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions[0].session_ref.is_none());
        // the fresh spawn re-sends the full prompt: preamble plus task body
        let mut prompts = prompt_files(&ctx);
        prompts.sort();
        let continuation_prompt = std::fs::read_to_string(prompts.last().unwrap()).unwrap();
        assert!(continuation_prompt.contains("voro done"), "preamble");
        assert!(continuation_prompt.contains("Do the thing"), "task body");
    }

    #[test]
    fn continuation_without_a_continue_verb_ignores_prior_refs() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = dispatched_with_ref(&mut store, &ctx, &project);

        continue_dispatch(&mut store, &ctx, id, None, Some("B")).unwrap();

        let sessions = store.sessions_for(id).unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(
            sessions[0].session_ref.is_none(),
            "a fresh spawn does not inherit the old session's ref"
        );
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

        let summary = open(&mut store, &ctx, id).unwrap();
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

        let summary = open(&mut store, &ctx, id).unwrap();
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
        // the fixture's voro.toml has no [viewer] table
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = review_task(&mut store, &project);

        let err = open(&mut store, &ctx, id).unwrap_err();
        assert!(err.contains("[viewer]"), "{err}");
        assert!(err.contains(ctx.agents_path.to_str().unwrap()), "{err}");
    }

    #[test]
    fn open_refuses_a_task_that_is_not_review_or_running() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project); // still ready

        let err = open(&mut store, &ctx, id).unwrap_err();
        assert!(err.contains("review or running"), "{err}");
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
