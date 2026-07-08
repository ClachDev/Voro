//! The observation half of dispatch (DESIGN.md §8): closing out sessions
//! whose backing process has already exited. `voro-core` owns the
//! reconciliation *decision* (`Store::reconcile_session`) given pid liveness
//! as a plain bool; this module supplies that bool — and a best-effort read
//! of whether the log looks like a usage cap — the two inputs that need
//! process or filesystem I/O and so stay out of voro-core.
//!
//! There is no daemon watching for a session's process to exit — the
//! dispatching `voro` invocation may not outlive it, whether that was a
//! one-shot `voro dispatch` or a TUI session closed hours ago. A pragmatic
//! v1 instead reconciles on read: `App::refresh` and every CLI verb call
//! [`reconcile_live_sessions`] before consulting session or task state, so a
//! dead session is finalised the next time anything looks, without ever
//! needing a resident watcher.

use std::io::{Read, Seek, SeekFrom};
use std::process::Command;

use voro_core::{Result, Store};

/// How much of a session's log tail to scan for a usage-cap signature.
const LOG_TAIL_BYTES: u64 = 4096;

/// Phrases that plausibly mean "usage cap", checked case-insensitively
/// against the log tail. Deliberately narrow (DESIGN.md §8): this is not a
/// general log parser, and anything it misses is reported `failed` rather
/// than guessed at — a wrong `failed` costs a beat (redispatch still works
/// the same either way), a wrong `capped` would be a confident lie about the
/// cause.
const CAP_SIGNATURES: [&str; 3] = ["usage limit", "rate limit", "quota exceeded"];

/// Reconcile every session still marked live: for each whose process is no
/// longer running, finalise it via [`Store::reconcile_session`]. Returns how
/// many were finalised. Cheap to call on every read — with no live sessions
/// it costs one query and nothing else.
pub fn reconcile_live_sessions(store: &mut Store) -> Result<usize> {
    let mut finalised = 0;
    for session in store.live_sessions()? {
        let Some(pid) = session.pid else {
            // No pid was recorded for this session — liveness can't be
            // checked, so it is left alone rather than guessed at.
            continue;
        };
        if pid_is_alive(pid) {
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
    use std::process::Stdio;
    use voro_core::{Action, NewTask, Priority, SessionOutcome, TaskState};

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

        assert_eq!(reconcile_live_sessions(&mut s).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
    }

    #[test]
    fn a_dead_pid_finalises_the_session_and_drops_the_task_to_ready() {
        let (mut s, task_id) = running_task();
        // spawn and reap a child so its pid is guaranteed to no longer exist
        let mut child = Command::new("true").stdout(Stdio::null()).spawn().unwrap();
        let dead_pid = child.id() as i64;
        child.wait().unwrap();

        let session = s
            .create_session(task_id, "claude", Some(dead_pid), None)
            .unwrap();

        assert_eq!(reconcile_live_sessions(&mut s).unwrap(), 1);
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Ready);
        assert_eq!(
            s.session(session.id).unwrap().outcome,
            Some(SessionOutcome::Failed)
        );
        assert!(s.redispatch_flag(task_id).unwrap());
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
            "claude",
            Some(dead_pid),
            Some(log.to_str().unwrap()),
        )
        .unwrap();
        reconcile_live_sessions(&mut s).unwrap();

        let sessions = s.sessions_for(task_id).unwrap();
        assert_eq!(sessions[0].outcome, Some(SessionOutcome::Capped));
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn sessions_without_a_recorded_pid_are_left_alone() {
        let (mut s, task_id) = running_task();
        let session = s.create_session(task_id, "claude", None, None).unwrap();

        assert_eq!(reconcile_live_sessions(&mut s).unwrap(), 0);
        assert!(s.session(session.id).unwrap().ended_at.is_none());
        assert_eq!(s.task(task_id).unwrap().state, TaskState::Running);
    }
}
