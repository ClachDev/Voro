//! Liveness of an agent's own session — the one thing about a session Voro
//! reads by asking rather than by being told (DESIGN.md §8). An agent defining
//! a `sessions` verb lists its sessions as JSON, and a session is live while
//! its captured ref appears there not-yet-finished. Every caller that runs that
//! verb comes through here: reconciliation (has a `running` task's session
//! died?), the jump-in key (attach a live session, resume a finished one), and
//! ref capture at dispatch.
//!
//! Liveness is three-valued on purpose. `None` — no `sessions` verb, no
//! captured ref, or a listing that would not run — means unknowable, and each
//! caller falls back rather than guessing: reconciliation leaves the session
//! alone, jump-in picks its verb from task state.

use std::path::Path;
use std::process::{Command, Stdio};

use voro_core::{AgentSessionEntry, parse_sessions_json};

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

/// Whether a listing shows this ref as a session still going. A ref that has
/// dropped out of the listing reads the same as one marked finished.
pub fn listing_says_live(entries: &[AgentSessionEntry], session_ref: &str) -> bool {
    entries
        .iter()
        .find(|e| e.matches_ref(session_ref))
        .is_some_and(|e| !e.is_finished())
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
