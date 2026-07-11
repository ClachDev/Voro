//! The observation half of dispatch (DESIGN.md §8): catching a `running`
//! task whose backing process has exited without reporting, and finalising a
//! session stranded on a task that has already closed. `voro-core` owns the
//! reconciliation *decision* (`Store::reconcile_session`) given liveness as a
//! plain bool; this module supplies that bool — and a best-effort read of
//! whether the log looks like a usage cap — the inputs that need process or
//! filesystem I/O and so stay out of voro-core.
//!
//! Because the session lifecycle follows the task, a liveness probe is only
//! run for a `running` task. `needs-input`/`review` keep their session open on
//! purpose (it is reused when the answer or feedback continues the work), so
//! they are left untouched without any probe; a session still open on a closed
//! task is stale and is finalised. Reconciliation is no longer the thing that
//! eventually closes healthy sessions — the terminal transitions do that — so a
//! lingering `blocked`/`null` listing entry for a task that has moved on stops
//! mattering.
//!
//! Liveness comes from one of two sources, per agent (task #75). An agent
//! that defines a `sessions` verb is asked directly: its listing is queried
//! once per reconcile pass, and a session is live while its captured
//! reference appears there not-yet-`done`. This is the only correct source
//! for supervisor-owned launches (`claude --bg`), where the pid Voro spawned
//! is a launcher that exits at birth — pid-checking would declare every such
//! dispatch dead immediately, so those sessions are *never* pid-checked; if
//! their liveness can't be determined (no ref captured, listing failed) they
//! are left alone rather than guessed at. Agents without a `sessions` verb
//! keep the original pid-liveness check.
//!
//! There is no daemon watching for a session's process to exit — the
//! dispatching `voro` invocation may not outlive it, whether that was a
//! one-shot `voro dispatch` or a TUI session closed hours ago. A pragmatic
//! v1 instead reconciles on read: `App::refresh` and every CLI verb call
//! [`reconcile_live_sessions`] before consulting session or task state, so a
//! dead session is finalised the next time anything looks, without ever
//! needing a resident watcher.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};

use voro_core::{AgentSessionEntry, AgentsConfig, Result, Store, TaskState, parse_sessions_json};

/// How much of a session's log tail to scan for a usage-cap signature.
const LOG_TAIL_BYTES: u64 = 4096;

/// Phrases that plausibly mean "usage cap", checked case-insensitively
/// against the log tail. Deliberately narrow (DESIGN.md §8): this is not a
/// general log parser, and anything it misses is reported `failed` rather
/// than guessed at — a wrong `failed` costs a beat (redispatch still works
/// the same either way), a wrong `capped` would be a confident lie about the
/// cause.
const CAP_SIGNATURES: [&str; 3] = ["usage limit", "rate limit", "quota exceeded"];

/// Reconcile every session still marked live, per its task's state (DESIGN.md
/// §8). The session lifecycle now follows the task, so a liveness probe is only
/// meaningful for a `running` task — its process going away without a
/// return-path call is the one thing observation can catch. A `needs-input` or
/// `review` task keeps its session open on purpose (for answer/feedback
/// continuation), so it is left alone without any probe; a session still open
/// on a task that has already closed is stale and is finalised. The
/// probe-or-not decision is made here; [`Store::reconcile_session`] owns the
/// resulting write. Returns how many were finalised. Cheap to call on every
/// read — with no live sessions it costs one query; the agents config is only
/// consulted when a `running` session needs a listing-based probe.
pub fn reconcile_live_sessions(store: &mut Store, agents_path: &Path) -> Result<usize> {
    let live = store.live_sessions()?;
    if live.is_empty() {
        return Ok(0);
    }
    // A missing or invalid voro.toml degrades every session to the pid
    // check rather than failing the read that triggered reconciliation.
    let config = AgentsConfig::load(agents_path).ok();
    // One listing per agent per pass, however many of its sessions are live.
    let mut listings: HashMap<String, Option<Vec<AgentSessionEntry>>> = HashMap::new();

    let mut finalised = 0;
    for session in live {
        let task_state = store.task(session.task_id)?.state;
        if task_state != TaskState::Running {
            // needs-input / review keep their session open (reconcile_session
            // returns None); a session still open on a closed task is stale and
            // gets finalised. Neither needs a liveness probe, so none is run.
            if store.reconcile_session(session.id, false, false)?.is_some() {
                finalised += 1;
            }
            continue;
        }

        // A running task: probe liveness. `None` means it can't be determined
        // right now (no ref captured, listing failed, no pid) — in which case
        // the session is left alone rather than wrongly finalised.
        let sessions_cmd = config
            .as_ref()
            .and_then(|c| c.agent(&session.agent))
            .and_then(|a| a.sessions());
        let alive: Option<bool> = match sessions_cmd {
            Some(cmd) => match session.session_ref.as_deref() {
                // No ref was captured — this session can't be found in the
                // listing, and pid-checking a supervisor-owned launch would
                // wrongly kill it, so liveness is unknowable.
                None => None,
                Some(session_ref) => {
                    let listing = listings
                        .entry(session.agent.clone())
                        .or_insert_with(|| run_sessions_command(cmd));
                    // The listing itself failing leaves liveness unknowable.
                    listing.as_ref().map(|entries| {
                        entries
                            .iter()
                            .find(|e| e.matches_ref(session_ref))
                            .is_some_and(|e| !e.is_finished())
                    })
                }
            },
            // No pid recorded means liveness can't be checked.
            None => session.pid.map(pid_is_alive),
        };
        let Some(alive) = alive else { continue };
        if alive {
            continue;
        }
        let likely_capped = session
            .log_path
            .as_deref()
            .is_some_and(log_tail_looks_capped);
        if store
            .reconcile_session(session.id, false, likely_capped)?
            .is_some()
        {
            finalised += 1;
        }
    }
    Ok(finalised)
}

/// Run an agent's `sessions` command and parse its listing. `None` on any
/// failure — spawn error, non-zero exit, unparseable output — which the
/// caller treats as "liveness unknowable", never as "no sessions".
fn run_sessions_command(cmd: &str) -> Option<Vec<AgentSessionEntry>> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_sessions_json(&String::from_utf8_lossy(&output.stdout)).ok()
}

/// Whether a process with this pid still exists, via `kill -0` — checks
/// existence/permission without sending a real signal, the same mechanism
/// `dispatch.rs` uses to terminate a session's process group. A non-positive
/// pid is refused rather than probed, since 0 and negative pids address
/// process groups, not the single process meant here.
fn pid_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A trivial, best-effort usage-cap detector (DESIGN.md §8): scan the last
/// few KB of the log for a short list of phrases agents print when a usage
/// limit stops them mid-session. Anything this misses simply reports as
/// `failed` instead of `capped` — see [`CAP_SIGNATURES`].
fn log_tail_looks_capped(path: &str) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut tail = String::new();
    if file.read_to_string(&mut tail).is_err() {
        return false;
    }
    let tail = tail.to_lowercase();
    CAP_SIGNATURES.iter().any(|sig| tail.contains(sig))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use voro_core::{Action, NewTask, Priority, SessionOutcome, TaskState};

    /// An agents path that never exists. It loads the built-ins (a missing
    /// file is no longer an error), so a session under a verb-less agent name
    /// like `manual` still degrades to the pid check — the built-in `claude`
    /// and `codex` carry a `sessions` verb and take the listing path instead.
    fn no_config() -> PathBuf {
        PathBuf::from("/nonexistent/voro.toml")
    }

    /// A ready task started (`running`), ready to hang a session off of.
    fn running_task() -> (Store, i64) {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/proj").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                title: "task".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap();
        s.apply(t.id, Action::Start).unwrap();
        (s, t.id)
    }

    #[test]
    fn a_live_pid_is_left_alone() {
        let (mut s, task_id) = running_task();
        // this test process's own pid is guaranteed alive
        let session = s
            .create_session(task_id, "claude", Some(std::process::id() as i64), None)
            .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn a_dead_pid_finalises_the_session_and_leaves_the_task_running() {
        let (mut s, task_id) = running_task();
        // spawn and reap a child so its pid is guaranteed to no longer exist
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();

        // a verb-less agent so liveness falls to the pid check
        let session = s
            .create_session(task_id, "manual", Some(dead_pid), None)
            .unwrap();

        // the session is finalised, but the task is not auto-requeued
        // (DESIGN.md §8): it stays running, surfaced as an orphaned running row.
        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );
        assert!(!s.redispatch_flag(task_id).unwrap());
    }

    #[test]
    fn a_dead_pid_with_a_capped_looking_log_reports_capped() {
        let (mut s, task_id) = running_task();
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();

        let log = std::env::temp_dir().join(format!(
            "voro-reconcile-test-{}-{dead_pid}.log",
            std::process::id()
        ));
        std::fs::write(&log, "sorry, hit the 5-hour usage limit — try again later").unwrap();

        s.create_session(
            task_id,
            "manual",
            Some(dead_pid),
            Some(log.to_str().unwrap()),
        )
        .unwrap();
        reconcile_live_sessions(&mut s, &no_config()).unwrap();

        let sessions = s.sessions_for(task_id).unwrap();
        assert_eq!(sessions[0].outcome, Some(SessionOutcome::Capped));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn sessions_without_a_recorded_pid_are_left_alone() {
        let (mut s, task_id) = running_task();
        let session = s.create_session(task_id, "claude", None, None).unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
    }

    /// A `needs-input` or `review` task keeps its session open on purpose, so
    /// reconciliation leaves it alone even with a dead process — no probe, no
    /// finalise. Otherwise its session would be closed and the ref lost before
    /// the answer/feedback could continue the same agent session.
    #[test]
    fn a_needs_input_or_review_session_is_left_open() {
        for action in [Action::Ask("A or B?".into()), Action::Complete(None)] {
            let (mut s, task_id) = running_task();
            // a dead pid that a running task would be finalised on
            let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
            let dead_pid = child.id() as i64;
            child.wait().unwrap();
            let session = s
                .create_session(task_id, "claude", Some(dead_pid), None)
                .unwrap();
            s.apply(task_id, action.clone()).unwrap();

            assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 0);
            assert!(s.session(session.id).unwrap().ended_at.is_none());
        }
    }

    // --- sessions-verb liveness (task #75) ---

    /// An `voro.toml` whose `claude` agent lists sessions by catting a
    /// canned JSON file, plus that file's path for the test to fill in.
    fn sessions_fixture(name: &str, listing_json: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "voro-reconcile-verbs-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let listing = dir.join("sessions.json");
        std::fs::write(&listing, listing_json).unwrap();
        let agents_path = dir.join("voro.toml");
        std::fs::write(
            &agents_path,
            format!(
                "default_agent = \"claude\"\n\n[agents.claude]\n\
                 dispatch = \"cat {{prompt_file}}\"\nsessions = \"cat '{}'\"\n",
                listing.display()
            ),
        )
        .unwrap();
        (agents_path, dir)
    }

    /// A session whose recorded pid is dead — the trap case: with `--bg`-style
    /// launches the spawned pid always exits at birth. With the session still
    /// listed live by the agent, reconciliation must trust the listing and
    /// leave the task running, never the pid.
    #[test]
    fn a_listed_live_session_is_left_alone_despite_a_dead_pid() {
        let (agents_path, dir) = sessions_fixture(
            "alive",
            r#"[{"id": "dead1234", "sessionId": "full-uuid-1", "cwd": "/tmp/proj",
                "startedAt": 1, "status": "idle", "state": "working"}]"#,
        );
        let (mut s, task_id) = running_task();
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();
        let session = s
            .create_session(task_id, "claude", Some(dead_pid), None)
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The session left its agent's listing without any return-path verb: it
    /// shows up `state: done` (or drops out entirely). The reconciler finalises
    /// the session but must *not* auto-requeue the task — a vanished session is
    /// indistinguishable from a completion whose `voro done` has not landed yet
    /// (DESIGN.md §8), so the task is left running for the human to handle.
    #[test]
    fn a_finished_or_missing_listed_session_is_left_running() {
        for (name, listing) in [
            (
                "done",
                r#"[{"sessionId": "full-uuid-1", "cwd": "/tmp/proj",
                    "startedAt": 1, "state": "done"}]"#,
            ),
            ("gone", "[]"),
        ] {
            let (agents_path, dir) = sessions_fixture(name, listing);
            let (mut s, task_id) = running_task();
            let session = s
                .create_session(task_id, "claude", Some(std::process::id() as i64), None)
                .unwrap();
            s.set_session_ref(session.id, "full-uuid-1").unwrap();

            assert_eq!(
                reconcile_live_sessions(&mut s, &agents_path).unwrap(),
                1,
                "{name}"
            );
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Running, "{name}");
            assert_eq!(
                s.session(session.id).unwrap().outcome,
                Some(SessionOutcome::Failed),
                "{name}"
            );
            assert!(!s.redispatch_flag(task_id).unwrap(), "{name}");

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// With a `sessions` verb configured but no captured ref, liveness is
    /// unknowable: the session is left alone (pid-checking a supervisor-owned
    /// launch would wrongly flag it), matching the no-pid case above.
    #[test]
    fn a_refless_session_of_a_sessions_agent_is_left_alone() {
        let (agents_path, dir) = sessions_fixture("refless", "[]");
        let (mut s, task_id) = running_task();
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();
        let session = s
            .create_session(task_id, "claude", Some(dead_pid), None)
            .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failing listing command means liveness is unknowable this pass —
    /// leave the session alone rather than guessing either way.
    #[test]
    fn a_failing_sessions_command_leaves_the_session_alone() {
        let dir = std::env::temp_dir().join(format!(
            "voro-reconcile-verbs-failcmd-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let agents_path = dir.join("voro.toml");
        std::fs::write(
            &agents_path,
            "default_agent = \"claude\"\n\n[agents.claude]\n\
             dispatch = \"cat {prompt_file}\"\nsessions = \"false\"\n",
        )
        .unwrap();
        let (mut s, task_id) = running_task();
        let session = s.create_session(task_id, "claude", Some(1), None).unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
