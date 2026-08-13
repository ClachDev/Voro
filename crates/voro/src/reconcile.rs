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
//! Liveness has two sources (task #75), and which of them owns a session is
//! recorded on its row at launch by the code that spawned the process
//! (`sessions.liveness_source`, DESIGN.md §8) rather than inferred here.
//! A listing-authoritative session is queried through [`crate::session_probe`],
//! its listing taken once per pass and cached here across the sessions sharing
//! an agent. This is the only correct source for supervisor-owned launches
//! (`claude --bg`), whose spawned pid is a launcher that exits at birth: the pid
//! the session row holds can never declare such a session dead, and
//! undeterminable liveness (no ref, listing failed) is
//! left alone rather than guessed. The pid a listing *entry* carries is a
//! different pid — the supervisor's — and it is authoritative where present,
//! which is what stops a listing that keeps dead sessions at `blocked` forever
//! from reading them all as live. A pid-authoritative session — a foreground
//! child Voro owns, or any launch by an agent with no `sessions` verb — keeps
//! the spawned-pid check.
//!
//! The row's own pid is still read in one direction, whichever source owns the
//! session: a pid that is *alive* proves the session is (task #390). A quick
//! message replaces that pid with the process carrying its turn, and where the
//! agent had to fork to be joined at all, that turn is a `-p` run the agent's
//! listing never shows — so the listing would report the session gone while the
//! message it was just sent is still being worked on. A dead pid still proves
//! nothing under a supervisor-owned launch and falls back to the listing.
//!
//! A refine round is read by the flavour it was launched as, which is the point
//! of recording the source: the headless flavour renders the agent's own
//! `dispatch` template, so under a `--bg` launcher its recorded pid lies exactly
//! as a dispatch's does, and it is listing-authoritative whether or not its ref
//! capture happened to succeed. The interactive flavour is the genuinely-own-pid
//! case — a foreground `plan` child, no ref by construction — and says so on its
//! row, so a Voro that dies mid-conversation still leaves a round another window
//! can finish.
//!
//! Whether a *dead* session died capped is read from the same per-agent verb
//! set, through an optional `logs` (task #415). Voro's own launch log — the
//! only channel there used to be — cannot answer for a supervisor-owned launch:
//! the launcher exits at birth having written nothing but the backgrounding
//! banner, so the scan could essentially never report `capped` for a `--bg`
//! dispatch, whatever killed it. An agent that can print a session's recent
//! output is asked for it instead, and the log tail stays the fallback for
//! agents that cannot. The *live* half of the same reading — a session held at
//! a cap without dying, which is what a cap actually does to a `--bg` dispatch
//! — is not taken here at all: it is slow enough to need its own off-loop
//! runner ([`crate::probe::CapProbe`]), and it changes nothing in the database.
//!
//! There is no daemon watching for process exit. Reconciliation runs on read:
//! `App::refresh` and every CLI verb call [`reconcile_live_sessions`] before
//! consulting session or task state, so a dead session is finalised the next
//! time anything looks.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use voro_core::{
    AgentSessionEntry, AgentsConfig, LivenessSource, Result, Store, TaskState, read_cap,
};

use crate::session_probe::{
    listing_says_live, pid_is_alive, read_session_cap, run_sessions_command,
};

/// How much of a session's log tail to scan for a usage-cap signature.
const LOG_TAIL_BYTES: u64 = 4096;

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
    // A missing or invalid voro.toml costs the listing-authoritative sessions
    // their probe — they are left alone — rather than failing the read that
    // triggered reconciliation.
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

        // Work under way: probe liveness through the source the launch
        // recorded. `None` (no ref, listing failed, no pid) leaves the session
        // alone rather than wrongly finalising it.
        let alive: Option<bool> = match session.liveness_source {
            // The recorded pid is the work itself.
            LivenessSource::Pid => session.pid.map(pid_is_alive),
            // The work went to a supervisor, so only the agent's listing knows.
            // Without a ref to look up — or a listing to look it up in — that
            // is unanswerable, and pid-checking the launcher Voro spawned would
            // declare a working agent dead.
            LivenessSource::Listing => {
                let sessions_cmd = config
                    .as_ref()
                    .and_then(|c| c.agent(&session.agent))
                    .and_then(|a| a.sessions());
                sessions_cmd
                    .zip(session.session_ref.as_deref())
                    .and_then(|(cmd, session_ref)| {
                        let listing = listings
                            .entry(session.agent.clone())
                            .or_insert_with(|| run_sessions_command(cmd, None));
                        listing
                            .as_ref()
                            .map(|entries| listing_says_live(entries, session_ref))
                    })
            }
        };
        // A recorded process that is still there proves the session is live
        // whatever the listing says: a quick message forks a `-p` turn that
        // never appears in `claude agents`, so listing-absence alone must not
        // finalise the session under it (DESIGN.md §8). The check is
        // directional — a dead pid proves nothing, since a dispatch's pid is a
        // launcher that exits at birth, and falls back to the listing verdict.
        let alive = match alive {
            Some(false) if session.pid.is_some_and(pid_is_alive) => Some(true),
            verdict => verdict,
        };
        let Some(alive) = alive else { continue };
        if alive {
            continue;
        }
        // Classify the death from the session's *own* output where the agent
        // can produce it (DESIGN.md §8). Voro's launch log is the wrong place
        // to ask for a supervisor-owned launch: the launcher exits at birth
        // having written only the backgrounding banner, so scanning it could
        // essentially never report `capped` for a `--bg` dispatch however the
        // session actually died. It stays the fallback for agents defining no
        // `logs` verb, where it is the only text there is.
        let logs_cmd = config
            .as_ref()
            .and_then(|c| c.agent(&session.agent))
            .and_then(|a| a.logs());
        let likely_capped = match logs_cmd.zip(session.session_ref.as_deref()) {
            Some((cmd, session_ref)) => read_session_cap(cmd, session_ref).is_some(),
            None => session
                .log_path
                .as_deref()
                .is_some_and(log_tail_looks_capped),
        };
        if store
            .reconcile_session(session.id, false, likely_capped)?
            .is_some()
        {
            finalised += 1;
        }
    }
    Ok(finalised)
}

/// Best-effort usage-cap detector for an agent with no `logs` verb (DESIGN.md
/// §8): read the launch log's tail and put it through the same classifier the
/// session-output path uses, so both channels agree on what a cap looks like.
fn log_tail_looks_capped(path: &str) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut tail = Vec::new();
    if file.read_to_end(&mut tail).is_err() {
        return false;
    }
    read_cap(&String::from_utf8_lossy(&tail)).is_some()
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

    /// A pid guaranteed to name no process: spawned and reaped.
    fn dead_pid() -> i64 {
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let pid = child.id() as i64;
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_live_pid_is_left_alone() {
        let (mut s, task_id) = running_task();
        // this test process's own pid is guaranteed alive
        let session = s
            .create_session(
                task_id,
                "claude",
                Some(std::process::id() as i64),
                LivenessSource::Pid,
                None,
            )
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
            .create_session(task_id, "manual", Some(dead_pid), LivenessSource::Pid, None)
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
            LivenessSource::Pid,
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
        let session = s
            .create_session(task_id, "claude", None, LivenessSource::Pid, None)
            .unwrap();

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
                .create_session(task_id, "claude", Some(dead_pid), LivenessSource::Pid, None)
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
        let dead_pid = dead_pid();
        // An interactive round records itself pid-authoritative — a foreground
        // child whose pid Voro holds — so it is pid-checked whatever verbs its
        // agent defines.
        let (_, session) = s
            .record_refine_launch(
                t.id,
                "thin body",
                "claude",
                Some(dead_pid),
                LivenessSource::Pid,
                None,
            )
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
            LivenessSource::Pid,
            None,
        )
        .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 0);
        assert_eq!(s.task(t.id).unwrap().state, TaskState::Refining);
    }

    /// A proposal under refinement, ready to hang a round's session off of,
    /// launched as the flavour `source` names (DESIGN.md §8).
    fn refining_task(agent: &str, pid: Option<i64>, source: LivenessSource) -> (Store, i64, i64) {
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
        let (_, session) = s
            .record_refine_launch(t.id, "thin body", agent, pid, source, None)
            .unwrap();
        (s, t.id, session.id)
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
            .create_session(
                task_id,
                "claude",
                Some(dead_pid),
                LivenessSource::Listing,
                None,
            )
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
            // The launcher pid of a `--bg` dispatch, dead as it always is by
            // now, so the listing is what decides (a *live* pid would override
            // it — see `a_live_pid_outlives_its_absence_from_the_listing`).
            let session = s
                .create_session(
                    task_id,
                    "claude",
                    Some(dead_pid()),
                    LivenessSource::Listing,
                    None,
                )
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
            .create_session(
                task_id,
                "claude",
                Some(dead_pid()),
                LivenessSource::Listing,
                None,
            )
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
        let session = s
            .create_session(task_id, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The quick-message case (task #390): a forked `-p` turn does not appear
    /// in the agent's listing at all, so the ref the session now carries reads
    /// as gone — while the process answering the message is right there on the
    /// row. A live pid outranks the listing, or the send the operator just made
    /// would stall the task under the agent working on it.
    #[test]
    fn a_live_pid_outlives_its_absence_from_the_listing() {
        let (agents_path, dir) = sessions_fixture("message-fork", "[]");
        let (mut s, task_id) = running_task();
        let session = s
            .create_session(task_id, "claude", None, LivenessSource::Listing, None)
            .unwrap();
        s.record_session_send(session.id, Some("forked-uuid"), std::process::id() as i64)
            .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A listing-authoritative session with no captured ref has nothing to look
    /// up: liveness is unknowable and the session is left alone (pid-checking a
    /// supervisor-owned launch would wrongly flag it), matching the no-pid case
    /// above.
    #[test]
    fn a_refless_listing_session_is_left_alone() {
        let (agents_path, dir) = sessions_fixture("refless", "[]");
        let (mut s, task_id) = running_task();
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();
        let session = s
            .create_session(
                task_id,
                "claude",
                Some(dead_pid),
                LivenessSource::Listing,
                None,
            )
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
        let session = s
            .create_session(task_id, "claude", Some(1), LivenessSource::Listing, None)
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- capped deaths read the session's own output (task #415) ---

    /// A `voro.toml` whose `claude` agent lists no sessions and prints
    /// `logs_output` for any session, plus the log file path a launch would
    /// have written, so a test can set the two channels against each other.
    fn logs_fixture(name: &str, logs_output: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("voro-reconcile-logs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let agents_path = dir.join("voro.toml");
        std::fs::write(
            &agents_path,
            format!(
                "default_agent = \"claude\"\n\n[agents.claude]\n\
                 dispatch = \"cat {{prompt_file}}\"\n\
                 logs = \"printf '%s' '{logs_output}' # {{session}}\"\n"
            ),
        )
        .unwrap();
        let log = dir.join("launch.log");
        (agents_path, log, dir)
    }

    /// The whole point of the `logs` verb on the dead path (DESIGN.md §8): a
    /// `--bg` launch's own log holds nothing but the backgrounding banner, so
    /// the session that died capped is only legible in the agent's own record
    /// of it. Before this, the scan read the banner, found nothing, and reported
    /// every capped bg dispatch as a plain failure.
    #[test]
    fn a_dead_session_is_classified_capped_from_its_own_output() {
        let (agents_path, log, dir) = logs_fixture("capped", "Session limit reached");
        std::fs::write(&log, "[voro] launched in background, pid 1234\n").unwrap();
        let (mut s, task_id) = running_task();
        let session = s
            .create_session(
                task_id,
                "claude",
                Some(dead_pid()),
                LivenessSource::Pid,
                Some(log.to_str().unwrap()),
            )
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Stalled);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Capped)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half: a session whose output says nothing about a cap is a
    /// plain failure, and the verb's answer overrides the launch log rather
    /// than being ORed with it — a banner that happened to contain the phrase
    /// cannot re-label a death the agent itself accounts for otherwise.
    #[test]
    fn a_dead_session_the_verb_calls_healthy_is_a_plain_failure() {
        let (agents_path, log, dir) = logs_fixture("failed", "compiling... done.");
        std::fs::write(&log, "hit the usage limit\n").unwrap();
        let (mut s, task_id) = running_task();
        let session = s
            .create_session(
                task_id,
                "claude",
                Some(dead_pid()),
                LivenessSource::Pid,
                Some(log.to_str().unwrap()),
            )
            .unwrap();
        s.set_session_ref(session.id, "full-uuid-1").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 1);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent defining no `logs` verb is reconciled exactly as before, from
    /// the launch log — and now catches the wordings the original three
    /// signatures missed, since both channels share one classifier.
    #[test]
    fn a_verbless_agent_still_classifies_from_the_launch_log() {
        let (mut s, task_id) = running_task();
        let dead = dead_pid();
        let log = std::env::temp_dir().join(format!(
            "voro-reconcile-verbless-{}-{dead}.log",
            std::process::id()
        ));
        std::fs::write(&log, "Session limit reached · resets 9pm").unwrap();

        let session = s
            .create_session(
                task_id,
                "manual",
                Some(dead),
                LivenessSource::Pid,
                Some(log.to_str().unwrap()),
            )
            .unwrap();
        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 1);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Capped)
        );
        let _ = std::fs::remove_file(&log);
    }

    // --- refine rounds read the source they recorded (tasks #379, #387) ---

    /// The refine-side twin of
    /// [`a_listed_live_session_is_left_alone_despite_a_dead_pid`]: a headless
    /// round runs the agent's `dispatch` template, so under a `--bg` launcher
    /// its recorded pid dies at birth. Trusting that pid finalised rounds
    /// seconds after they started and marked the rewrites they went on to apply
    /// as failures; with a ref captured, the listing is what answers.
    #[test]
    fn a_listed_live_refine_round_is_left_alone_despite_a_dead_pid() {
        let (agents_path, dir) = sessions_fixture(
            "refine-alive",
            r#"[{"sessionId": "refine-uuid", "cwd": "/tmp/proj",
                "startedAt": 1, "state": "working"}]"#,
        );
        let (mut s, task_id, session_id) =
            refining_task("claude", Some(dead_pid()), LivenessSource::Listing);
        s.set_session_ref(session_id, "refine-uuid").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session_id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Refining);
        assert!(!s.refine_failed_flag(task_id).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The other half: a round genuinely gone from the listing is still caught
    /// and still marked, the launcher pid on its row proving nothing either way.
    #[test]
    fn a_refine_round_gone_from_the_listing_is_marked_failed() {
        let (agents_path, dir) = sessions_fixture("refine-gone", "[]");
        let (mut s, task_id, session_id) =
            refining_task("claude", Some(dead_pid()), LivenessSource::Listing);
        s.set_session_ref(session_id, "refine-uuid").unwrap();

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Proposed);
        assert!(s.refine_failed_flag(task_id).unwrap());
        assert_eq!(
            s.session(session_id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An agent that defines no `sessions` verb has only the spawned pid, and
    /// for it that pid is right: rounds still reconcile by pid, dead and alive.
    #[test]
    fn a_refine_round_of_a_verbless_agent_is_pid_checked() {
        let (mut s, task_id, session_id) =
            refining_task("manual", Some(dead_pid()), LivenessSource::Pid);
        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Proposed);
        assert!(s.refine_failed_flag(task_id).unwrap());
        assert_eq!(
            s.session(session_id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );

        let (mut s, task_id, _) = refining_task(
            "manual",
            Some(std::process::id() as i64),
            LivenessSource::Pid,
        );
        assert_eq!(reconcile_live_sessions(&mut s, &no_config()).unwrap(), 0);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Refining);
    }

    /// The tail #379 left open (task #387): a *headless* round whose ref capture
    /// timed out. Its recorded pid is a `--bg` launcher, dead within a second of
    /// the launch, and inferring the flavour from the missing ref read that
    /// round as the interactive one and finalised it `failed` while its agent
    /// was still rewriting the body. Recorded listing-authoritative, it is
    /// simply unprobeable — left in `refining` until the rewrite lands or the
    /// listing says it is gone, exactly as a ref-less dispatch already was.
    #[test]
    fn a_headless_refine_round_with_no_ref_is_left_alone() {
        let (agents_path, dir) = sessions_fixture("refine-refless", "[]");
        let (mut s, task_id, session_id) =
            refining_task("claude", Some(dead_pid()), LivenessSource::Listing);

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 0);
        assert!(s.session(session_id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Refining);
        assert!(!s.refine_failed_flag(task_id).unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The flavour the fallback existed for, now saying so on its row: an
    /// interactive round is a foreground `plan` child that appears in no
    /// listing, so its pid decides even under an agent whose listing is right
    /// there and empty. A Voro that dies mid-conversation therefore still
    /// leaves a round another window's reconcile can finish.
    #[test]
    fn an_interactive_refine_round_is_pid_checked_under_a_sessions_agent() {
        let (agents_path, dir) = sessions_fixture("refine-interactive", "[]");
        let (mut s, task_id, session_id) =
            refining_task("claude", Some(dead_pid()), LivenessSource::Pid);

        assert_eq!(reconcile_live_sessions(&mut s, &agents_path).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Proposed);
        assert!(s.refine_failed_flag(task_id).unwrap());
        assert_eq!(
            s.session(session_id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );

        // The live half of the same flavour: a conversation still going is left
        // alone, however empty the listing it never joined.
        let (agents_path2, dir2) = sessions_fixture("refine-interactive-live", "[]");
        let (mut s, task_id, _) = refining_task(
            "claude",
            Some(std::process::id() as i64),
            LivenessSource::Pid,
        );
        assert_eq!(reconcile_live_sessions(&mut s, &agents_path2).unwrap(), 0);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Refining);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
