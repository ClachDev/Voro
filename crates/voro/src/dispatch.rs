//! Dispatching a ready task to a headless agent session (DESIGN.md §8). This
//! is the I/O half of dispatch — resolving the agent, guarding against a dirty
//! checkout, writing the prompt, and spawning the process detached — kept out
//! of voro-core, which stays pure of process and filesystem I/O. The atomic
//! `ready → running`-plus-session write is voro-core's `Store::record_dispatch`.

use std::fs::File;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use voro_core::{AgentsConfig, PROMPT_FILE_PLACEHOLDER, Store, TaskState};

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
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    if task.state != TaskState::Ready {
        return Err(format!(
            "only ready tasks can be dispatched; task {task_id} is {}",
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
    let prompt_path = ctx.runtime_dir.join(format!("task-{task_id}.prompt.md"));
    let prompt = if task.body.is_empty() {
        format!("# {}\n", task.title)
    } else {
        format!("# {}\n\n{}\n", task.title, task.body.trim_end())
    };
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("cannot write prompt {}: {e}", prompt_path.display()))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let log_path = ctx.runtime_dir.join(format!("task-{task_id}-{stamp}.log"));
    let log = File::create(&log_path)
        .map_err(|e| format!("cannot create log {}: {e}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("cannot open log {}: {e}", log_path.display()))?;

    let command = agent
        .cmd
        .replace(PROMPT_FILE_PLACEHOLDER, &shell_quote(&prompt_path));
    let child = Command::new("sh")
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

    let (task, session) = store
        .record_dispatch(task_id, &agent.name, Some(pid), log_path.to_str())
        .map_err(|e| e.to_string())?;

    Ok(format!(
        "dispatched task {} to {} (session {}, pid {}) — log {}",
        task.id,
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
        return Err(format!(
            "{path} is not a git repository, so its cleanliness cannot be verified"
        ));
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
        let prompt =
            std::fs::read_to_string(ctx.runtime_dir.join(format!("task-{id}.prompt.md"))).unwrap();
        assert!(prompt.contains("Do the thing"));
        assert!(prompt.contains("Detailed prompt."));
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
