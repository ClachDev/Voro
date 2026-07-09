//! Dispatching a ready task to a headless agent session (DESIGN.md §8), and
//! continuing an already-`running` one after a human answers a question
//! (DESIGN.md §6). Both are the I/O half of dispatch — resolving the agent,
//! guarding against a dirty checkout, writing the prompt, and spawning the
//! process detached — kept out of voro-core, which stays pure of process and
//! filesystem I/O. The atomic state-plus-session writes are voro-core's
//! `Store::record_dispatch` and `Store::record_continuation`.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use voro_core::{
    AgentsConfig, PROMPT_FILE_PLACEHOLDER, Session, Store, Task, TaskState, VIEWER_PATH_PLACEHOLDER,
};

/// Prepended verbatim to every dispatched prompt so the agent learns the
/// return-path verbs (DESIGN.md §8) with no per-project install and no reliance
/// on a skill being loaded — the dispatcher already owns the prompt file, so
/// injection is the most robust delivery. Deliberately says nothing about
/// `voro start`: dispatch has already transitioned the task `ready → running`
/// and exported `VORO_TASK_ID` on the agent's behalf. Kept as a single const so
/// the injected text cannot drift; the voro-cli SKILL.md is the richer,
/// dev-facing version covering manual runs.
const RETURN_PATH_PREAMBLE: &str = "\
<!-- Voro dispatch: how to report back -->
You are an agent dispatched by Voro on the task below. Voro tracks this task in a
local SQLite database; report progress through these CLI verbs rather than
editing that database yourself:

    voro ask \"$VORO_TASK_ID\" --question \"...\"     # blocked on a human decision (-> needs-input)
    voro done \"$VORO_TASK_ID\" --summary \"...\"      # work finished, await review (-> review)
    voro propose <project> \"<title>\" --body-file <path>   # file a follow-up task (-> proposed)

`VORO_TASK_ID` identifies this task and is already exported for you; `propose`
uses it as the discovered-from source when `--from` is omitted. `VORO_DB` points
these verbs at the database this session was dispatched from — without it they
would default to the real database at ~/.local/share/voro/voro.db — so run them
as shown and never modify the database with raw SQL, which would bypass the state
machine and event log.

---

";

/// Which of the two flows `spawn_session` is performing — they share every
/// mechanic (agent resolution, the dirty-tree guard, prompt/log files,
/// spawning and reaping the process) and differ only in which state the task
/// must already be in and which `Store` method records the result.
#[derive(Clone, Copy)]
enum SpawnKind {
    /// A fresh dispatch: the task must be `ready`; recording it performs the
    /// `ready → running` transition.
    Fresh,
    /// A continuation: the task must already be `running` — `answer` just put
    /// it there — so recording it only opens a session, never changes state.
    Continuation,
}

impl SpawnKind {
    fn required_state(self) -> TaskState {
        match self {
            SpawnKind::Fresh => TaskState::Ready,
            SpawnKind::Continuation => TaskState::Running,
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
    /// `agents.toml` location.
    pub agents_path: PathBuf,
    /// Directory for prompt and log files — never inside a project checkout,
    /// so writing the prompt does not itself dirty the tree.
    pub runtime_dir: PathBuf,
}

impl DispatchCtx {
    /// The real environment: `agents.toml` at its config default, and a
    /// `sessions/` directory beside the database for prompts and logs.
    pub fn from_db_path(db_path: &Path) -> DispatchCtx {
        let runtime_dir = db_path
            .parent()
            .map(|d| d.join("sessions"))
            .unwrap_or_else(|| PathBuf::from("sessions"));
        DispatchCtx {
            db_path: db_path.to_path_buf(),
            agents_path: AgentsConfig::default_path(),
            runtime_dir,
        }
    }
}

/// Dispatch a ready task to a headless agent session, returning a summary line.
/// `agent_override` is the CLI `--agent` picker flag, which outranks the task's
/// own override (DESIGN.md §8). Every check that can fail — task readiness,
/// agent resolution, the dirty-tree guard, writing the prompt — runs before the
/// process is spawned, so a failed dispatch never leaves an orphan session.
pub fn dispatch(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    spawn_session(store, ctx, task_id, agent_override, SpawnKind::Fresh)
}

/// Continue a task that `answer` just moved `needs-input → running` (DESIGN.md
/// §6): spawn a fresh session whose prompt is the task body — now carrying the
/// `## Answers` section the answer appended — so the work resumes with the
/// answer in hand rather than being fed to a live pipe, which by the time a
/// human answers has typically already exited. Shares every mechanic with
/// [`dispatch`] except the required starting state and how the result is
/// recorded: `record_continuation` asserts the task is already `running`
/// instead of performing the `ready → running` transition itself, so this can
/// never be used to smuggle a task into `running` outside the transition API.
pub fn continue_dispatch(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
) -> Result<String, String> {
    spawn_session(store, ctx, task_id, agent_override, SpawnKind::Continuation)
}

/// Open a `review` (or `running`) task's checkout in the configured viewer so
/// its diff can be seen (DESIGN.md §11a). The `[viewer]` command from
/// `agents.toml` is run detached in the project's path — a shell-out baseline
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
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .current_dir(&project.path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("cannot spawn viewer in {}: {e}", project.path))?;
    let pid = i64::from(child.id());

    // Nothing waits on the viewer — like dispatch, reap it in a detached thread
    // so an exited child never lingers as a zombie in a long-lived TUI session.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    Ok(format!(
        "opened task {task_id} in {} (pid {pid})",
        project.name
    ))
}

fn spawn_session(
    store: &mut Store,
    ctx: &DispatchCtx,
    task_id: i64,
    agent_override: Option<&str>,
    kind: SpawnKind,
) -> Result<String, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    let required = kind.required_state();
    if task.state != required {
        return Err(format!(
            "only {required} tasks can be {}; task {task_id} is {}",
            kind.verb_past(),
            task.state
        ));
    }
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;

    let config = AgentsConfig::load(&ctx.agents_path).map_err(|e| e.to_string())?;
    let agent = config
        .resolve(agent_override.or(task.agent.as_deref()))
        .map_err(|e| e.to_string())?;

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
    let prompt = format!("{RETURN_PATH_PREAMBLE}{body}");
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("cannot write prompt {}: {e}", prompt_path.display()))?;

    let log_path = ctx.runtime_dir.join(format!("task-{task_id}-{stamp}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create log {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;

    let command = agent
        .cmd
        .replace(PROMPT_FILE_PLACEHOLDER, &shell_quote(&prompt_path));
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

    Ok(format!(
        "{} task {} {} {} (session {}, pid {}) — log {}",
        kind.verb_past(),
        task.id,
        kind.preposition(),
        session.agent,
        session.id,
        pid,
        log_path.display()
    ))
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
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use voro_core::{NewTask, Priority};

    /// A scratch database, a freshly-`git init`ed clean project, and an
    /// `agents.toml` whose one agent is a stub command that just reads the
    /// prompt. Returns the store, the dispatch context, and the project path.
    fn fixture(cmd: &str) -> (Store, DispatchCtx, PathBuf) {
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
        let agents_path = root.join("agents.toml");
        std::fs::write(
            &agents_path,
            format!("default = \"stub\"\n\n[agents.stub]\ncmd = \"{cmd}\"\n"),
        )
        .unwrap();

        let store = Store::open(&db_path).unwrap();
        let ctx = DispatchCtx {
            db_path,
            agents_path,
            runtime_dir: root.join("sessions"),
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
        // all three return-path verbs, the task-id variable, and the task body
        assert!(prompt.contains("voro ask"), "{prompt}");
        assert!(prompt.contains("voro done"), "{prompt}");
        assert!(prompt.contains("voro propose"), "{prompt}");
        assert!(prompt.contains("VORO_TASK_ID"), "{prompt}");
        assert!(prompt.contains("Detailed prompt."), "{prompt}");
        // the preamble is one const, dropped in ahead of the task body
        assert!(prompt.starts_with(RETURN_PATH_PREAMBLE), "{prompt}");
        assert!(
            prompt.find("# Do the thing").unwrap() > prompt.find("voro done").unwrap(),
            "the task body must follow the preamble"
        );
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
    fn only_ready_tasks_dispatch() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        let id = ready_task(&mut store, &project);
        store.apply(id, voro_core::Action::Park).unwrap();

        let err = dispatch(&mut store, &ctx, id, None).unwrap_err();
        assert!(err.contains("ready"), "{err}");
        assert!(store.sessions_for(id).unwrap().is_empty());
    }

    /// A ready task moved into `review` through the transition machine, so
    /// `open`'s state guard is exercised against a genuine review row.
    fn review_task(store: &mut Store, project_path: &Path) -> i64 {
        let id = ready_task(store, project_path);
        store.apply(id, voro_core::Action::Start).unwrap();
        store.apply(id, voro_core::Action::Complete).unwrap();
        id
    }

    #[test]
    fn open_runs_the_configured_viewer_in_the_project_path() {
        let (mut store, ctx, project) = fixture("cat {prompt_file}");
        // a [viewer] whose command drops a marker at the substituted {path}
        std::fs::write(
            &ctx.agents_path,
            "default = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
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
    fn open_without_a_viewer_reports_what_to_configure() {
        // the fixture's agents.toml has no [viewer] table
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
            "default = \"stub\"\n\n[agents.stub]\ncmd = \"cat {prompt_file}\"\n\n\
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
            })
            .unwrap()
            .id;

        dispatch(&mut store, &ctx, id, None).unwrap();
        assert_eq!(store.sessions_for(id).unwrap()[0].agent, "special");
    }
}
