//! The observation half of dispatch (DESIGN.md §8): catch a `running` task
//! whose backing process has exited without reporting — landing it in
//! `stalled` — and finalise a session stranded on a task that has already
//! closed. `voro-core` owns the reconciliation decision
//! (`Store::reconcile_session`) given liveness as a bool; this module supplies
//! that bool, plus a best-effort read of whether the log looks usage-capped,
//! the inputs that need process or filesystem I/O.
//!
//! A liveness probe runs for a `running` task and for a `refining` one — the
//! two states with work actually under way (DESIGN.md §6/§9).
//! `needs-input`/`review` keep their session open on purpose (reused when the
//! answer or feedback continues the work), and a session still open on a closed
//! task is stale and finalised — neither needs a probe.
//!
//! Liveness has two sources per agent (task #75). An agent defining a
//! `sessions` verb is queried through [`crate::session_probe`], its listing
//! taken once per pass and cached here across the sessions sharing an agent.
//! This is the only correct source for supervisor-owned launches (`claude
//! --bg`), whose spawned pid is a launcher that exits at birth: the pid the
//! session row holds would declare every such dispatch dead, so it is never
//! consulted for them, and undeterminable liveness (no ref, listing failed) is
//! left alone rather than guessed. The pid a listing *entry* carries is a
//! different pid — the supervisor's — and it is authoritative where present,
//! which is what stops a listing that keeps dead sessions at `blocked` forever
//! from reading them all as live. Agents without a `sessions` verb keep the
//! spawned-pid check.
//!
//! A refine round is the exception that check exists to avoid guessing about,
//! and it is not a guess: refine spawns its agent as a direct `sh -c` child
//! whose pid Voro holds, captures no session ref, and hands the process to no
//! supervisor — so the pid *is* the round, and a refining session is pid-checked
//! whatever verbs its agent defines.
//!
//! There is no daemon watching for process exit. Reconciliation runs on read:
//! `App::refresh` and every CLI verb call [`reconcile_live_sessions`] before
//! consulting session or task state, so a dead session is finalised the next
//! time anything looks.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use voro_core::{AgentSessionEntry, AgentsConfig, Result, Store, TaskState};

use crate::session_probe::{listing_says_live, pid_is_alive, run_sessions_command};

/// How much of a session's log tail to scan for a usage-cap signature.
const LOG_TAIL_BYTES: u64 = 4096;

/// Phrases that plausibly mean "usage cap", checked case-insensitively
/// against the log tail. Deliberately narrow (DESIGN.md §8): not a general log
/// parser — anything it misses is reported `failed` rather than misattributed
/// as `capped`.
const CAP_SIGNATURES: [&str; 3] = ["usage limit", "rate limit", "quota exceeded"];

/// Reconcile every session still marked live, per its task's state (DESIGN.md
/// §8). The probe-or-not decision is made here; [`Store::reconcile_session`]
/// owns the resulting write. Returns how many were finalised. Cheap to call on
/// every read — with no live sessions it costs one query, and the agents config
/// is only consulted when a `running` session needs a listing-based probe.
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
        if !matches!(task_state, TaskState::Running | TaskState::Refining) {
            // needs-input / review keep their session open (reconcile_session
            // returns None); a session on a closed task is stale and finalised.
            if store.reconcile_session(session.id, false, false)?.is_some() {
                finalised += 1;
            }
            continue;
        }

        // Work under way: probe liveness. `None` (no ref, listing failed, no
        // pid) leaves the session alone rather than wrongly finalising it. A
        // refine round is always its own pid — no supervisor holds it — so the
        // listing path, which exists for launches whose pid lies, is skipped.
        let sessions_cmd = match task_state {
            TaskState::Refining => None,
            _ => config
                .as_ref()
                .and_then(|c| c.agent(&session.agent))
                .and_then(|a| a.sessions()),
        };
        let alive: Option<bool> = match sessions_cmd {
            Some(cmd) => match session.session_ref.as_deref() {
                // No ref: not findable in the listing, and pid-checking a
                // supervisor-owned launch would wrongly kill it.
                None => None,
                Some(session_ref) => {
                    let listing = listings
                        .entry(session.agent.clone())
                        .or_insert_with(|| run_sessions_command(cmd, None));
                    listing
                        .as_ref()
                        .map(|entries| listing_says_live(entries, session_ref))
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

/// Best-effort usage-cap detector (DESIGN.md §8): scan the log tail for the
/// phrases in [`CAP_SIGNATURES`].
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
    use std::process::{Command, Stdio};
    use voro_core::{Action, NewTask, Priority, SessionOutcome, TaskState};

    /// An agents path that never exists — loads the built-ins, so a verb-less
    /// agent name like `manual` degrades to the pid check while the built-in
    /// `claude`/`codex` (which carry a `sessions` verb) take the listing path.
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
                repo_id: None,
                title: "task".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
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
    fn a_dead_pid_finalises_the_session_and_stalls_the_task() {
        let (mut s, task_id) = running_task();
        // spawn and reap a child so its pid is guaranteed to no longer exist
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();

        // a verb-less agent so liveness falls to the pid check
        let session = s
            .create_session(task_id, "manual", Some(dead_pid), None)
            .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Stalled);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );
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

    /// `needs-input`/`review` keep their session open, so reconciliation leaves
    /// it alone even with a dead process — otherwise the ref would be lost
    /// before the answer/feedback could continue the same agent session.
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

    /// A refine round whose agent has gone (DESIGN.md §6): the proposal is back
    /// in the triage queue within one pass, marked as a failed round, and the
    /// session is closed. This is the pid probe doing for a rewrite exactly what
    /// it does for a dispatch.
    #[test]
    fn a_dead_refine_agent_returns_the_proposal() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/proj").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "sloppy".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Proposed,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();
        // `claude` defines a `sessions` verb, so a *dispatch* of its with no
        // captured ref would be left alone. A refine round is a plain child
        // whose pid Voro holds, so it is pid-checked regardless.
        let (_, session) = s
            .record_refine_launch(t.id, "thin body", "claude", Some(dead_pid), None)
            .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 1);
        assert_eq!(s.task(t.id).unwrap().state, TaskState::Proposed);
        assert!(s.refine_failed_flag(t.id).unwrap());
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );
    }

    /// The live half: a round still running is left alone, so a rewrite in
    /// progress is never yanked back into the queue mid-flight.
    #[test]
    fn a_live_refine_agent_is_left_alone() {
        let mut s = Store::open_in_memory().unwrap();
        let p = s.create_project("proj", "/tmp/proj").unwrap();
        let t = s
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "sloppy".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Proposed,
                agent: None,
                human: false,
                deep: false,
            })
            .unwrap();
        s.record_refine_launch(
            t.id,
            "thin body",
            "claude",
            Some(std::process::id() as i64),
            None,
        )
        .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 0);
        assert_eq!(s.task(t.id).unwrap().state, TaskState::Refining);
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

    /// The trap case: a `--bg`-style launch's spawned pid exits at birth, so
    /// with the session still listed live the reconciler must trust the listing
    /// and leave the task running, never the dead pid.
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

    /// A session that left the listing without a return-path verb (shows
    /// `state: done`, or drops out) is finalised and the task stalled — an
    /// attention state, so even a rare misfire surfaces rather than requeues.
    #[test]
    fn a_finished_or_missing_listed_session_stalls_the_task() {
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
            assert_eq!(s.task(task_id).unwrap().state, TaskState::Stalled, "{name}");
            assert_eq!(
                s.session(session.id).unwrap().outcome,
                Some(SessionOutcome::Failed),
                "{name}"
            );

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The zombie case (DESIGN.md §8): an agent's listing keeps entries for
    /// long-dead sessions at `blocked` and never says `done`, so a `running`
    /// task whose session is one of them must still be finalised — otherwise
    /// the task rides the strip as live forever.
    #[test]
    fn a_pidless_blocked_zombie_stalls_the_task() {
        let (agents_path, dir) = sessions_fixture(
            "zombie",
            r#"[{"sessionId": "full-uuid-1", "cwd": "/tmp/proj",
                "startedAt": 1, "state": "blocked"}]"#,
        );
        let (mut s, task_id) = running_task();
        let session = s
            .create_session(task_id, "claude", Some(std::process::id() as i64), None)
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Stalled);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same entry with its supervisor pid still alive is a session stuck
    /// mid-turn, not a zombie: left alone, so a permission prompt waiting on
    /// the operator does not stall the task under them.
    #[test]
    fn a_blocked_entry_with_a_live_pid_is_left_alone() {
        let (agents_path, dir) = sessions_fixture(
            "blocked-live",
            &format!(
                r#"[{{"sessionId": "full-uuid-1", "cwd": "/tmp/proj",
                    "startedAt": 1, "state": "blocked", "pid": {}}}]"#,
                std::process::id()
            ),
        );
        let (mut s, task_id) = running_task();
        let session = s.create_session(task_id, "claude", None, None).unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
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
