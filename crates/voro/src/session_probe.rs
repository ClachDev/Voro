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
    AccountCap, AgentSessionEntry, CapReading, SessionLiveness, parse_reset_epoch,
    parse_sessions_json, read_cap, render_session,
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

/// Whether a listing shows this ref as a session whose turn has ended and which
/// the agent is therefore still holding registered — the rest-stop's trigger
/// (DESIGN.md §8). A ref that has dropped out of the listing answers no: it was
/// never registered, or it has already been stopped, and either way there is
/// nothing to release.
pub fn listing_says_at_rest(entries: &[AgentSessionEntry], session_ref: &str) -> bool {
    entries
        .iter()
        .find(|e| e.matches_ref(session_ref))
        .is_some_and(AgentSessionEntry::at_rest)
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

/// Run an agent's `cap` command and read the account's reset instant from it
/// (DESIGN.md §8). `None` means "nothing says this account is capped", which is
/// also what a command that would not run answers, and what an account merely
/// spending its window answers: the verb prints an instant while the account is
/// refused and prints nothing otherwise, so silence is the whole negative
/// answer and every way of arriving at it lands the same.
///
/// Unlike [`read_session_cap`] this costs the agent an API call rather than a
/// screen replay, which is why the caller asks rarely and only while a session
/// is already badged capped. The template names no session and takes no
/// substitution — the answer is about the account, and one of them serves the
/// whole strip.
///
/// The label is rendered here, beside the reading, because it is the one part
/// that needs the operator's timezone and the render path may not shell out for
/// it. A `date` that cannot render the instant costs the badge its time, not
/// the reading its meaning: the comparison that says whether the window has
/// reopened is made on the instant itself.
pub fn read_account_cap(cap_cmd: &str, now_epoch: i64) -> Option<AccountCap> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cap_cmd)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let reset_epoch = parse_reset_epoch(&String::from_utf8_lossy(&output.stdout), now_epoch)?;
    Some(AccountCap {
        reset_epoch,
        reset_label: local_label(reset_epoch),
    })
}

/// An instant as the local `21:50` the badge shows, via the same `date` the
/// wall clock is read from and for the same reason — the agent's instant means
/// nothing to an operator until it is in their own timezone, and the standard
/// library offers no local time.
fn local_label(epoch: i64) -> Option<String> {
    let output = Command::new("date")
        .arg(format!("-d@{epoch}"))
        .arg("+%H:%M")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stamp = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let (hour, minute) = stamp.split_once(':')?;
    let (hour, minute): (u16, u16) = (hour.parse().ok()?, minute.parse().ok()?);
    (hour < 24 && minute < 60).then_some(stamp)
}

/// The wall clock as seconds since the Unix epoch, which is what an agent's own
/// reset instant is compared against ([`CapWindow::resolve`]). No subprocess and
/// no timezone: an instant is an instant, which is the whole reason it beats the
/// clock time on a session's screen.
pub fn local_epoch() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs() as i64)
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

/// What one listing says about one session, for a caller that needs more than
/// the liveness bool out of a single probe — the send path, which asks both
/// whether the session is mid-turn (refuse) and whether it is registered at rest
/// (release it first). Running the listing twice for the two questions would
/// cost a second subprocess on a keypress, and could answer them from two
/// different readings of a session that moved in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SessionVerdict {
    /// Whether the session is still going, `None` when that is unknowable.
    pub live: Option<bool>,
    /// Whether the agent still holds it registered with its turn ended
    /// ([`listing_says_at_rest`]). Unknowable reads as no, since the rest-stop
    /// acts only on positive evidence.
    pub at_rest: bool,
}

/// Read one session's listing entry, for a caller holding no listing of its own
/// — one probe, both answers.
pub fn probe_session(sessions_cmd: Option<&str>, session_ref: Option<&str>) -> SessionVerdict {
    let read = || {
        let session_ref = session_ref?;
        let entries = run_sessions_command(sessions_cmd?, None)?;
        Some(SessionVerdict {
            live: Some(listing_says_live(&entries, session_ref)),
            at_rest: listing_says_at_rest(&entries, session_ref),
        })
    };
    read().unwrap_or_default()
}

/// Whether a session is still running, for a caller that needs only that.
/// `None` when liveness is unknowable.
pub fn session_is_live(sessions_cmd: Option<&str>, session_ref: Option<&str>) -> Option<bool> {
    probe_session(sessions_cmd, session_ref).live
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

    /// The `cap` verb end to end: an agent printing an instant is read as one,
    /// and rendered into the local time the badge shows.
    #[test]
    fn a_capped_account_reads_as_an_instant_with_its_label() {
        let now = local_epoch().expect("a clock");
        let reading = read_account_cap(&format!("printf '%s\\n' {}", now + 3600), now)
            .expect("an account reading");
        assert_eq!(reading.reset_epoch, now + 3600);
        let label = reading.reset_label.expect("a rendered label");
        assert_eq!(label.len(), 5, "{label}");
        assert!(label.as_bytes()[2] == b':', "{label}");
    }

    /// Every way of learning nothing lands the same, because the verb's
    /// negative answer *is* silence: an account with room left prints nothing,
    /// and so does a command that would not run.
    #[test]
    fn an_uncapped_or_unreadable_account_reads_as_nothing() {
        let now = local_epoch().expect("a clock");
        for cmd in ["true", "false", "exit 127", "printf ''", "printf 'allowed'"] {
            assert_eq!(read_account_cap(cmd, now), None, "{cmd}");
        }
        // And a number that cannot be a reset instant is not read as one.
        assert_eq!(read_account_cap("printf '2'", now), None);
    }

    /// The two clocks agree about now: the instant a reset is compared against
    /// and the minutes-past-midnight a parsed time is compared against are
    /// readings of the same moment, or the badge contradicts itself.
    #[test]
    fn both_clocks_read_the_same_moment() {
        let epoch = local_epoch().expect("a clock");
        let minutes = local_minutes().expect("a local clock");
        let label = local_label(epoch).expect("a rendered label");
        let (hour, minute) = label.split_once(':').expect("HH:MM");
        let rendered: u16 = hour.parse::<u16>().unwrap() * 60 + minute.parse::<u16>().unwrap();
        // A minute may tick between the two readings, midnight included.
        let apart = (i32::from(rendered) - i32::from(minutes)).rem_euclid(1440);
        assert!(apart <= 1, "{label} vs {minutes} minutes past midnight");
    }

    /// The rest reading the send path and the reconciler act on: a session the
    /// agent still holds registered with its turn ended. `blocked` — a
    /// permission prompt, a supervisor mid-turn — is the one that must not
    /// answer yes, and an entry that has left the listing has nothing to
    /// release.
    #[test]
    fn only_a_done_entry_reads_as_at_rest() {
        let at_rest = |json: &str| probe_session(Some(&listing_cmd(json)), Some("uuid-1")).at_rest;
        assert!(at_rest(r#"[{"sessionId": "uuid-1", "state": "done"}]"#));
        assert!(!at_rest(r#"[{"sessionId": "uuid-1", "state": "blocked"}]"#));
        assert!(!at_rest(r#"[{"sessionId": "uuid-1", "state": "working"}]"#));
        assert!(!at_rest("[]"));
        // and the unknowable cases read as no rather than as a stop to make
        assert!(!probe_session(None, Some("uuid-1")).at_rest);
        assert!(!probe_session(Some("false"), Some("uuid-1")).at_rest);
        assert!(!probe_session(Some(&listing_cmd("[]")), None).at_rest);
    }

    /// Both readings come off one listing run, so the send path cannot see a
    /// session as mid-turn and at rest from two different moments.
    #[test]
    fn one_probe_answers_both_questions() {
        let cmd = listing_cmd(r#"[{"sessionId": "uuid-1", "state": "done"}]"#);
        assert_eq!(
            probe_session(Some(&cmd), Some("uuid-1")),
            SessionVerdict {
                live: Some(false),
                at_rest: true
            }
        );
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
