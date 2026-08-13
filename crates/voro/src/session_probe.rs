//! Liveness of an agent's own session — the one thing about a session Voro
//! reads by asking rather than by being told (DESIGN.md §8). An agent defining
//! a `sessions` verb lists its sessions as JSON, and a session is live while
//! its captured ref appears there on an entry that still reads as running.
//! Every caller that runs that verb comes through here: reconciliation (has a
//! `running` task's session died?), the jump-in key (attach a live session,
//! resume a finished one), and ref capture at dispatch.
//!
//! Reading an entry takes two halves. `voro-core` classifies it from `state`
//! and `pid` alone ([`AgentSessionEntry::liveness`]), and this module supplies
//! the process check that a pid-bearing entry resolves through — the same
//! `kill -0` the pid-only agents are reconciled by, so the boundary that keeps
//! `voro-core` free of process I/O holds for both liveness sources.
//!
//! Liveness is three-valued on purpose. `None` — no `sessions` verb, no
//! captured ref, or a listing that would not run — means unknowable, and each
//! caller falls back rather than guessing: reconciliation leaves the session
//! alone, jump-in picks its verb from task state.

use std::path::Path;
use std::process::{Command, Stdio};

use voro_core::{
    AgentSessionEntry, CapReading, SessionLiveness, parse_sessions_json, read_cap, render_session,
};

/// Run an agent's `sessions` command and parse its listing, in a given
/// directory where the caller has one to scope it to (dispatch matches a fresh
/// session by checkout). `None` on any failure — spawn error, non-zero exit,
/// unparseable output — which callers treat as "unknowable", never as "no
/// sessions".
pub fn run_sessions_command(cmd: &str, cwd: Option<&Path>) -> Option<Vec<AgentSessionEntry>> {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd).stdin(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_sessions_json(&String::from_utf8_lossy(&output.stdout)).ok()
}

/// Whether one listing entry describes a session still going: `voro-core`'s
/// classification with its process check resolved.
pub fn entry_is_live(entry: &AgentSessionEntry) -> bool {
    match entry.liveness() {
        SessionLiveness::Live => true,
        SessionLiveness::WhileProcessLives(pid) => pid_is_alive(pid),
        SessionLiveness::Dead => false,
    }
}

/// Whether a listing shows this ref as a session still going. A ref that has
/// dropped out of the listing reads the same as one whose entry is dead.
pub fn listing_says_live(entries: &[AgentSessionEntry], session_ref: &str) -> bool {
    entries
        .iter()
        .find(|e| e.matches_ref(session_ref))
        .is_some_and(entry_is_live)
}

/// Whether a process with this pid still exists, via `kill -0` (existence
/// check, no signal sent). A non-positive pid is refused: 0 and negative pids
/// address process groups, not the single process meant here. Shared by both
/// liveness sources — the pid a session row recorded at spawn, and the pid a
/// listing entry names for its supervisor.
pub fn pid_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .is_ok_and(|out| out.status.success())
}

/// Run an agent's `logs` command for one session and read the result as a cap
/// reading (DESIGN.md §8). `None` means "nothing says this session is capped",
/// which is also what a command that would not run answers: the verb is
/// best-effort throughout, and an agent that cannot produce output for a
/// session is one Voro badges nothing for.
///
/// Exit status is deliberately not consulted. The built-in `claude` spelling
/// exits zero on a job it cannot find, printing a not-found line, and an agent
/// that exits non-zero while still having printed the tail is no less readable;
/// the only question either way is whether a cap signature is in the text.
pub fn read_session_cap(logs_cmd: &str, session_ref: &str) -> Option<CapReading> {
    let rendered = render_session(logs_cmd, session_ref);
    let output = Command::new("sh")
        .arg("-c")
        .arg(&rendered)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    read_cap(&String::from_utf8_lossy(&output.stdout))
}

/// The local wall clock as minutes past midnight, which is what a bare reset
/// time in an agent's output is compared against ([`CapReading::reset_passed`]).
///
/// Read from `date` because the agent renders that time in the operator's own
/// timezone and the standard library offers no local time — a dependency for
/// one clock reading would cost more than the subprocess, which runs only when
/// a badge with a time is actually on screen. An unreadable clock answers
/// `None`, and the badge simply shows the time without judging it.
pub fn local_minutes() -> Option<u16> {
    let output = Command::new("date")
        .arg("+%H:%M")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let stamp = String::from_utf8_lossy(&output.stdout);
    let (hour, minute) = stamp.trim().split_once(':')?;
    let (hour, minute): (u16, u16) = (hour.parse().ok()?, minute.parse().ok()?);
    (hour < 24 && minute < 60).then_some(hour * 60 + minute)
}

/// Whether a session is still running, for a caller holding no listing of its
/// own — one probe, one answer. `None` when liveness is unknowable.
pub fn session_is_live(sessions_cmd: Option<&str>, session_ref: Option<&str>) -> Option<bool> {
    let session_ref = session_ref?;
    let entries = run_sessions_command(sessions_cmd?, None)?;
    Some(listing_says_live(&entries, session_ref))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `sessions` command that prints a canned listing. The fixtures carry
    /// no single quote, so one pair of them is quoting enough.
    fn listing_cmd(json: &str) -> String {
        format!("printf '%s' '{json}'")
    }

    #[test]
    fn a_listed_working_session_is_live() {
        let cmd = listing_cmd(r#"[{"sessionId": "uuid-1", "state": "working"}]"#);
        assert_eq!(session_is_live(Some(&cmd), Some("uuid-1")), Some(true));
    }

    #[test]
    fn a_finished_or_absent_session_is_not_live() {
        let done = listing_cmd(r#"[{"sessionId": "uuid-1", "state": "done"}]"#);
        assert_eq!(session_is_live(Some(&done), Some("uuid-1")), Some(false));
        let empty = listing_cmd("[]");
        assert_eq!(session_is_live(Some(&empty), Some("uuid-1")), Some(false));
    }

    /// A pid-less zombie: the listing keeps the entry at `blocked` long after
    /// the session died, and with no pid to check there is nothing claiming it
    /// is going. Reading it as live is what sent quick-message and jump-in at a
    /// session that no longer exists (DESIGN.md §8).
    #[test]
    fn a_pidless_blocked_entry_is_not_live() {
        let cmd = listing_cmd(r#"[{"sessionId": "uuid-1", "state": "blocked"}]"#);
        assert_eq!(session_is_live(Some(&cmd), Some("uuid-1")), Some(false));
    }

    /// The other half: the same `blocked` entry with a supervisor pid that is
    /// still around is a session genuinely stuck mid-turn — live, attachable,
    /// and not something a headless message may join.
    #[test]
    fn a_blocked_entry_with_a_live_pid_is_live() {
        let cmd = listing_cmd(&format!(
            r#"[{{"sessionId": "uuid-1", "state": "blocked", "pid": {}}}]"#,
            std::process::id()
        ));
        assert_eq!(session_is_live(Some(&cmd), Some("uuid-1")), Some(true));
    }

    /// A named pid that has gone decides the entry too, whatever its state —
    /// and `done` beside a live pid still reads dead.
    #[test]
    fn the_named_pid_decides_unless_the_entry_says_done() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let cmd = listing_cmd(&format!(
            r#"[{{"sessionId": "uuid-1", "state": "working", "pid": {dead_pid}}}]"#
        ));
        assert_eq!(session_is_live(Some(&cmd), Some("uuid-1")), Some(false));

        let cmd = listing_cmd(&format!(
            r#"[{{"sessionId": "uuid-1", "state": "done", "pid": {}}}]"#,
            std::process::id()
        ));
        assert_eq!(session_is_live(Some(&cmd), Some("uuid-1")), Some(false));
    }

    #[test]
    fn a_non_positive_pid_is_never_alive() {
        assert!(!pid_is_alive(0));
        assert!(!pid_is_alive(-1));
        assert!(pid_is_alive(std::process::id() as i64));
    }

    /// The `logs` verb end to end: the reference is bound and shell-quoted,
    /// the output is classified, and the reset time comes back parsed.
    #[test]
    fn a_capped_session_reads_as_capped_through_the_logs_verb() {
        let reading = read_session_cap(
            "printf 'Session limit reached · Retrying in 5m (9:50pm)' # {session}",
            "uuid-1",
        )
        .expect("a cap");
        assert_eq!(reading.reset_label().as_deref(), Some("21:50"));

        // The reference reaches the command as itself, so a session can be
        // singled out by it rather than the template being merely decorative.
        assert_eq!(
            read_session_cap("printf %s {session}", "session limit reached"),
            Some(voro_core::CapReading::default())
        );
    }

    /// A session that is working, and the ways the verb itself can come to
    /// nothing, all read the same: no cap. The built-in `claude` spelling exits
    /// zero on a job it cannot find, so a not-found line must be as unremarkable
    /// as a command that would not run at all.
    #[test]
    fn a_healthy_or_unreadable_session_reads_as_uncapped() {
        for cmd in [
            "printf 'Running tests… {session}'",
            "printf \"Couldn't read logs for {session} — job not found\"",
            "printf 'No job matching {session}.'",
            "false # {session}",
            "exit 127 # {session}",
        ] {
            assert_eq!(read_session_cap(cmd, "uuid-1"), None, "{cmd}");
        }
    }

    /// The wall clock the reset badge is judged against is a real reading, in
    /// range — the badge is wrong in both directions if this is not.
    #[test]
    fn the_local_clock_reads_within_the_day() {
        let minutes = local_minutes().expect("a local clock");
        assert!(minutes < 24 * 60, "{minutes}");
    }

    /// The three ways liveness is unknowable, each answering `None` rather
    /// than picking a side.
    #[test]
    fn a_missing_verb_ref_or_listing_is_unknowable() {
        let cmd = listing_cmd("[]");
        assert_eq!(session_is_live(None, Some("uuid-1")), None);
        assert_eq!(session_is_live(Some(&cmd), None), None);
        assert_eq!(session_is_live(Some("false"), Some("uuid-1")), None);
        assert_eq!(session_is_live(Some("printf 'not json'"), Some("u")), None);
    }
}
