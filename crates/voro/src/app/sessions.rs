//! The agent sessions behind tasks (DESIGN.md §8): jumping in, sending a
//! message, nudging a capped session, and paging a session's log.

use super::Mode;
use super::PromptKind;
use voro_core::{Action, AgentsConfig, TaskState};

use super::{App, AttachRequest};

/// What the quick-message key needs resolved before it can send: the session
/// the line lands in, the template that puts it there, and the listing the
/// liveness probe reads to be sure the session is between turns.
struct MessageTarget {
    /// The session row the send updates once it is confirmed — its process, and
    /// its reference where the agent's verb forks (DESIGN.md §8). Carried whole
    /// rather than as an id, because the release the send may have to make
    /// first is addressed at the session itself.
    session: voro_core::Session,
    /// The reference the send is addressed to: the row's, already established to
    /// be present.
    session_ref: String,
    template: String,
    sessions_cmd: Option<String>,
}

/// The two ways into an agent's own session (DESIGN.md §8): join one still
/// running, or reopen one that has finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JumpVerb {
    Attach,
    Resume,
}

/// Which verb a task's state implies — the fallback for when the agent cannot
/// say whether its session is still live. `None` for a state with no session
/// worth jumping into, which is what gates the key. `waiting` keeps its session
/// open exactly as `review` does (DESIGN.md §8), so it jumps in on the same
/// terms — the strip is where the operator now meets it.
fn state_jump_verb(state: TaskState) -> Option<JumpVerb> {
    match state {
        TaskState::Running => Some(JumpVerb::Attach),
        TaskState::Review | TaskState::Waiting | TaskState::Stalled => Some(JumpVerb::Resume),
        _ => None,
    }
}

/// Which states accept a quick message into the task's session (DESIGN.md §8) —
/// the states whose session is open and between turns, so a headless resume
/// lands as the next thing the agent reads. `running` and `refining` are refused
/// because the session is mid-turn with no injection channel, and `stalled`
/// because its session is dead: a headless resume there would restart the work
/// with no tracked pid and no session row, invisible to the reconciler.
/// Redispatch is the honest path for that, and `A` the one for the rest.
/// What a nudged session is told (DESIGN.md §8). One word, because the session
/// already holds the whole task: its transcript, its worktree and whatever it
/// had half-written when the window closed. Anything longer would be Voro
/// restating a brief the agent can already read, and would risk redirecting work
/// that was only ever interrupted.
pub(super) const NUDGE: &str = "continue";

pub(super) fn state_accepts_message(state: TaskState) -> bool {
    matches!(
        state,
        TaskState::NeedsInput | TaskState::Review | TaskState::Waiting
    )
}

/// The template to jump in with: liveness decides the verb wherever the agent
/// can report it, and the task state stands in only when it cannot. The two
/// come apart in both directions — a `claude --bg` session outlives the
/// `running` state and refuses `--resume` while it does, and a session that
/// died before its task left `running` has nothing left to attach to.
///
/// The choice then falls back to whichever verb the agent actually defines —
/// the built-in `codex` defines only `resume` — so a one-verb agent jumps in
/// with that verb rather than erroring; `None` only when it defines neither.
fn jump_verb<'a>(
    live: Option<bool>,
    by_state: JumpVerb,
    attach: Option<&'a str>,
    resume: Option<&'a str>,
) -> Option<&'a str> {
    let want = match live {
        Some(true) => JumpVerb::Attach,
        Some(false) => JumpVerb::Resume,
        None => by_state,
    };
    match want {
        JumpVerb::Attach => attach.or(resume),
        JumpVerb::Resume => resume.or(attach),
    }
}

impl App {
    /// Page through the selected task's newest session log, in
    /// any state that has a session on record. `$PAGER` (default `less`) owns
    /// the terminal, so this runs through `pending_attach` with the TUI torn
    /// down around it, like attach/resume. Missing pieces report via the status
    /// line.
    pub(super) fn view_session_log(&mut self) {
        let Some(task_id) = self.selected_task().map(|t| t.id) else {
            return;
        };
        let Some(session) = self.last_sessions.get(&task_id) else {
            self.status = Some(format!("task {task_id} has no session on record"));
            return;
        };
        let Some(log_path) = session.log_path.clone() else {
            self.status = Some(format!("session {} recorded no log path", session.id));
            return;
        };
        let cwd = match self.task_checkout(task_id) {
            Ok(path) => path,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        self.pending_attach = Some(AttachRequest {
            command: format!(
                "${{PAGER:-less}} {}",
                crate::dispatch::shell_quote(std::path::Path::new(&log_path))
            ),
            cwd,
        });
    }

    /// Kill the process *group* of a task's open session, best-effort,
    /// returning what to say about it. A headless expansion is spawned into its
    /// own group (pgid = pid), so the negated pid reaches the agent under the
    /// launching shell rather than only the shell — which is the case this key
    /// exists for, a detached round nobody is watching. Deliberately no
    /// plain-pid fallback: a recorded pid outlives its process and can be
    /// recycled, and signalling a stranger is worse than the round the operator
    /// can end by quitting it. An interactive round's child is in voro's own
    /// group (it must be, to own the terminal) and so names no group here; that
    /// round ends by the operator leaving the session, which concludes it.
    pub(super) fn kill_open_session(&self, task_id: i64) -> String {
        let Some(pid) = self
            .store
            .sessions_for(task_id)
            .ok()
            .and_then(|sessions| sessions.into_iter().find(|s| s.ended_at.is_none()))
            .and_then(|session| session.pid)
        else {
            return String::new();
        };
        let killed = std::process::Command::new("kill")
            .args(["-TERM", "--"])
            .arg(format!("-{pid}"))
            .status()
            .is_ok_and(|status| status.success());
        if killed {
            format!(" — agent (pid {pid}) killed")
        } else {
            format!(" — agent (pid {pid}) could not be killed")
        }
    }

    /// Jump into the selected task's agent session. Which verb that
    /// takes is decided by the session itself where the agent can say: a live
    /// session is `attach`ed to, a finished one `resume`d — task state only
    /// standing in when liveness is unknowable. The two do not follow from each
    /// other, since a `claude --bg` session commonly outlives the `running`
    /// state (DESIGN.md §8's stale-review rebase attaches to a `review` task's
    /// session) and `--resume` refuses a session still held by the supervisor.
    /// The run happens in main() via `pending_attach`, with the TUI torn down
    /// around it. Every missing piece (state, session, captured ref, verb)
    /// reports via the status line.
    pub(super) fn jump_into_session(&mut self) {
        let (task_id, state) = match self.selected_task() {
            Some(task) => (task.id, task.state),
            None => return,
        };
        let Some(by_state) = state_jump_verb(state) else {
            self.status = Some(format!(
                "task is {state} — jump-in works on running, review, waiting, or \
                 stalled tasks"
            ));
            return;
        };
        let sessions = match self.store.sessions_for(task_id) {
            Ok(sessions) => sessions,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let Some(session) = sessions.first() else {
            self.status = Some(format!(
                "task {task_id} has no recorded session to jump into"
            ));
            return;
        };
        let Some(session_ref) = session.session_ref.clone() else {
            self.status = Some(format!(
                "no session reference was captured for session {} — nothing to {}",
                session.id,
                match by_state {
                    JumpVerb::Attach => "attach to",
                    JumpVerb::Resume => "resume",
                }
            ));
            return;
        };
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let agent = config.agent(&session.agent);
        // A synchronous probe: the TUI is about to hand the terminal to a
        // full-screen session anyway, so one listing costs nothing felt.
        let live = crate::session_probe::session_is_live(
            agent.and_then(|a| a.sessions()),
            Some(&session_ref),
        );
        let template = jump_verb(
            live,
            by_state,
            agent.and_then(|a| a.attach()),
            agent.and_then(|a| a.resume()),
        );
        let Some(template) = template else {
            self.status = Some(format!(
                "agent '{}' defines no attach or resume template in {}",
                session.agent,
                self.dispatch_ctx.agents_path.display()
            ));
            return;
        };
        let cwd = match self.task_checkout(task_id) {
            Ok(path) => path,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        self.pending_attach = Some(AttachRequest {
            command: template.replace(
                voro_core::SESSION_PLACEHOLDER,
                &crate::dispatch::shell_quote(std::path::Path::new(&session_ref)),
            ),
            cwd,
        });
    }

    /// Quick-message the selected task's agent session (DESIGN.md §8): `a`
    /// collects one line and fires it into the session headlessly, so steering
    /// an agent costs a sentence rather than the attach round-trip `A` still
    /// performs. The state gate and the session's own pieces are checked here,
    /// before the input opens, so a refusal costs no typing.
    pub(super) fn message_session(&mut self) {
        let (task_id, state) = match self.selected_task() {
            Some(task) => (task.id, task.state),
            None => return,
        };
        if !state_accepts_message(state) {
            self.status = Some(format!(
                "task is {state} — a message lands on a needs-input, review, or \
                 waiting task; A jumps into the session instead"
            ));
            return;
        }
        if self.message_target(task_id).is_none() {
            return;
        }
        self.mode = Mode::Prompt {
            task_id,
            kind: PromptKind::SessionMessage,
            buffer: String::new(),
        };
    }

    /// Nudge every cap-stuck session whose window has reopened (DESIGN.md §8).
    ///
    /// A usage cap ends a session's turn and leaves it sitting there: nothing
    /// retries, so the work waits for a human however long ago the window
    /// reopened. Walking the strip by hand costs an attach, a typed word and a
    /// detach per session, which is why the reset hours go missing overnight.
    /// This is that walk as one key.
    ///
    /// Both guards the quick-message key answers to are stood down here, and the
    /// cap reading is what earns that: [`state_accepts_message`] refuses a
    /// `running` task because its session is mid-turn, and `send_session_message`
    /// refuses a session that is still up — but a capped session is precisely
    /// one that is up, `running`, and *not* mid-turn. Nothing else in the cockpit
    /// can tell those apart, so nothing else may skip the guards.
    pub(super) fn nudge_capped(&mut self) {
        // A cap whose time never parsed is the operator's call, not the clock's:
        // they pressed the key, and a nudge sent early is refused by the agent
        // rather than doing harm. This is the one rule an automatic sweep would
        // have to invert — with no keypress behind it, an untimed cap has
        // nothing saying the window has opened — and it is the rule the
        // account's own instant retires case by case: a cap the screen never
        // timed is timed after all once the agent has said when.
        let (mut ready, mut waiting): (Vec<i64>, Vec<i64>) = (Vec::new(), Vec::new());
        let mut retrying = 0usize;
        for id in self.caps.keys().copied() {
            let window = self.cap_window(id);
            // A session retrying the rejected request is the one badged shape
            // that must be walked past. It is mid-turn, so it will carry on by
            // itself — and since the nudge stops its target before resuming it
            // (`nudge_one`), sending into one does not add a redundant turn but
            // ends the turn already running. That the operator pressed the key
            // is no argument for it either: the untimed cap below is swept on
            // their judgement because nothing else knows whether the window is
            // open, whereas here the session has said outright that it is
            // working.
            if window.as_ref().is_some_and(|window| window.retrying) {
                retrying += 1;
                continue;
            }
            let due = window.is_none_or(|window| window.due());
            if due { &mut ready } else { &mut waiting }.push(id);
        }
        // A sweep visits the strip in a stable order rather than the map's.
        ready.sort_unstable();
        // Every badged session the sweep declined to touch is accounted for in
        // what it reports, retrying ones included: a row left alone in silence
        // reads as one the sweep missed.
        let retrying_note = match retrying {
            0 => String::new(),
            n => format!(" — {n} retrying"),
        };
        if ready.is_empty() {
            self.status = Some(if waiting.is_empty() && retrying == 0 {
                "no session is capped".into()
            } else if waiting.is_empty() {
                format!(
                    "{retrying} capped session{} — retrying, none waiting on you",
                    if retrying == 1 { "" } else { "s" }
                )
            } else {
                format!(
                    "{} capped session{} — none has reached its reset yet{retrying_note}",
                    waiting.len(),
                    if waiting.len() == 1 { "" } else { "s" }
                )
            });
            return;
        }

        let mut sent = 0usize;
        let mut refused: Vec<String> = Vec::new();
        for task_id in ready {
            match self.nudge_one(task_id) {
                Ok(()) => {
                    sent += 1;
                    // The badge goes at once rather than waiting out the probe
                    // interval, so a second press cannot put a second agent on
                    // the same worktree. A session that is still capped when the
                    // next reading lands badges again.
                    self.caps.remove(&task_id);
                }
                Err(e) => refused.push(format!("{task_id}: {e}")),
            }
        }

        let mut note = format!(
            "nudged {sent} capped session{}",
            if sent == 1 { "" } else { "s" }
        );
        if !waiting.is_empty() {
            note.push_str(&format!(" — {} still before its reset", waiting.len()));
        }
        note.push_str(&retrying_note);
        if !refused.is_empty() {
            note.push_str(&format!(" — refused {}", refused.join("; ")));
        }
        self.status = Some(note);
        let refreshed = self.refresh();
        self.report(refreshed);
    }

    /// Say [`NUDGE`] into one capped session and record the send, much as the
    /// quick-message key does — the same verb, the same tracked pid — so a
    /// nudged session stays as visible to the reconciler as a messaged one.
    ///
    /// It releases the session first, and *unconditionally*, which is the one
    /// place a send departs from the rest rule (DESIGN.md §8). That rule's
    /// `done` test is only sufficient because every session it does not cover is
    /// refused by the liveness gate before a send is ever attempted — and a
    /// capped session is exactly such a session, `blocked` with its supervisor
    /// alive, which the sweep walks past on the strength of the cap reading.
    /// Having stood down the guard that made the test sufficient, it cannot then
    /// lean on the test: the hold is there, nothing will ever report it as
    /// `done`, and an in-place resume would simply be refused. The bypass has to
    /// be complete or the nudge does not land.
    ///
    /// What that costs is worth naming. The stop is exactly as safe as the cap
    /// reading is right — the same bet the sweep already makes — but the
    /// consequence of a wrong one is severe: a send into a session
    /// that turns out to be mid-turn kills the turn.
    /// The reading can also be up to a probe interval stale, so a
    /// session an operator restarted by hand in the last minute is still badged
    /// and can still be stopped from under them. Both are why the sweep stays on
    /// a keypress rather than on the clock (DESIGN.md §8).
    fn nudge_one(&mut self, task_id: i64) -> Result<(), String> {
        let target = self
            .message_target(task_id)
            .ok_or_else(|| self.status.clone().unwrap_or_else(|| "no session".into()))?;
        let cwd = self.task_checkout(task_id).map_err(|e| e.to_string())?;
        self.release_session(&target.session)?;
        let sent = crate::dispatch::send_message(
            &self.dispatch_ctx,
            crate::dispatch::SessionMessage {
                task_id,
                template: &target.template,
                session_ref: &target.session_ref,
                message: NUDGE,
                cwd,
            },
        )?;
        let pid = sent.pid();
        if let Err(e) =
            self.store
                .record_session_send(target.session.id, sent.new_session_ref(), pid)
        {
            sent.abandon();
            return Err(format!(
                "recording the send failed ({e}); the spawned agent (pid {pid}) was killed"
            ));
        }
        sent.confirm(&self.dispatch_ctx);
        Ok(())
    }

    /// Release the agent's hold on a session, waiting for the answer (DESIGN.md
    /// §8), so a headless resume into it can land. Both senders call it inline,
    /// immediately before the send it makes deliverable: the quick-message key
    /// where the target's listing entry says the agent is holding a session
    /// that has come to rest, the capped-session sweep unconditionally.
    ///
    /// Nothing about the row changes either way — the session stays the task's
    /// conversation — so a config that will not load costs the release and
    /// nothing else, and the send that follows is refused by the agent rather
    /// than by Voro.
    fn release_session(&self, session: &voro_core::Session) -> Result<(), String> {
        let Ok(config) = AgentsConfig::load(&self.dispatch_ctx.agents_path) else {
            return Ok(());
        };
        crate::dispatch::stop_session_now(&self.dispatch_ctx, &config, session)
    }

    /// Resolve what a quick message needs, reporting whichever piece is missing
    /// on the status line exactly as `jump_into_session` does. Config is loaded
    /// fresh, so an agent that gained a `message` verb since the TUI started
    /// gains the key too.
    fn message_target(&mut self, task_id: i64) -> Option<MessageTarget> {
        let sessions = match self.store.sessions_for(task_id) {
            Ok(sessions) => sessions,
            Err(e) => {
                self.status = Some(e.to_string());
                return None;
            }
        };
        let Some(session) = sessions.first() else {
            self.status = Some(format!("task {task_id} has no recorded session to message"));
            return None;
        };
        let Some(session_ref) = session.session_ref.clone() else {
            self.status = Some(format!(
                "no session reference was captured for session {} — nothing to message",
                session.id
            ));
            return None;
        };
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return None;
            }
        };
        let agent = config.agent(&session.agent);
        let Some(template) = agent.and_then(|a| a.message()) else {
            self.status = Some(format!(
                "agent '{}' defines no message template in {} — A jumps into the session instead",
                session.agent,
                self.dispatch_ctx.agents_path.display()
            ));
            return None;
        };
        Some(MessageTarget {
            sessions_cmd: agent.and_then(|a| a.sessions()).map(str::to_string),
            template: template.to_string(),
            session: session.clone(),
            session_ref,
        })
    }

    /// Send the collected line into the task's session (DESIGN.md §8). A
    /// review or waiting task's message *is* its rejection, and the send goes
    /// first: a message that never left would otherwise leave the feedback in
    /// the body, the task back in `running`, and the agent none the wiser —
    /// which is exactly the state a redispatch cannot tell from a stall. So the
    /// spawn is confirmed, then the session row and the transition commit
    /// together, and a refused send leaves the task precisely where it was. A
    /// `needs-input` task transitions not at all — the answer lives in the
    /// transcript and the agent's own `voro resume` moves it back to `running`
    /// (DESIGN.md §6).
    pub(super) fn send_session_message(&mut self, task_id: i64, message: &str) {
        if message.trim().is_empty() {
            self.status = Some("a message is required".into());
            return;
        }
        let state = match self.store.task(task_id) {
            Ok(task) => task.state,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        if !state_accepts_message(state) {
            self.status = Some(format!("task is now {state} — nothing was sent"));
            return;
        }
        let Some(target) = self.message_target(task_id) else {
            return;
        };
        // One listing read answers both of the questions the send has about the
        // session: whether it is mid-turn, and whether the agent is still
        // holding it registered at rest.
        let verdict = crate::session_probe::probe_session(
            target.sessions_cmd.as_deref(),
            Some(&target.session_ref),
        );
        // A live session is mid-turn, so a headless resume would either be
        // refused or land out of order; the operator wants the real terminal.
        if verdict.live == Some(true) {
            self.status = Some(format!(
                "task {task_id}'s session is still running — A attaches to it"
            ));
            return;
        }
        // The agent still holds a session it has finished a turn on, and that
        // hold refuses an in-place resume, so it is released here and waited on
        // before the send goes out (DESIGN.md §8). Failing to release it
        // refuses the send outright rather than spawning one that cannot land,
        // so the task is left exactly where it was.
        if verdict.at_rest
            && let Err(e) = self.release_session(&target.session)
        {
            self.status = Some(format!("{e} — task {task_id} is unchanged"));
            return;
        }
        let cwd = match self.task_checkout(task_id) {
            Ok(path) => path,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let rejected = matches!(state, TaskState::Review | TaskState::Waiting);
        // A rejection reaches the session framed as one: the feedback, plus the
        // instruction to answer it point by point at `done` (DESIGN.md §8). An
        // ordinary message is said as written.
        let framed = rejected
            .then(|| crate::dispatch::rework_message(task_id, &self.dispatch_ctx.db_path, message));
        let sent = crate::dispatch::send_message(
            &self.dispatch_ctx,
            crate::dispatch::SessionMessage {
                task_id,
                template: &target.template,
                session_ref: &target.session_ref,
                message: framed.as_deref().unwrap_or(message),
                cwd,
            },
        );
        let sent = match sent {
            Ok(sent) => sent,
            Err(e) => {
                self.status = Some(format!("{e} — task {task_id} is unchanged"));
                return;
            }
        };
        // The send is under way, so the session row follows it — the process
        // now carrying the turn, and the reference the agent forked into where
        // its verb does that — and the rejection commits behind it. A store
        // failure here takes the agent down with it rather than leaving it
        // working on feedback no state records.
        let pid = sent.pid();
        if let Err(e) =
            self.store
                .record_session_send(target.session.id, sent.new_session_ref(), pid)
        {
            sent.abandon();
            self.status = Some(format!(
                "recording the send failed ({e}); the spawned agent (pid {pid}) was killed"
            ));
            return;
        }
        if rejected {
            if let Err(e) = self
                .store
                .apply(task_id, Action::RejectWork(message.to_string()))
            {
                sent.abandon();
                self.status = Some(format!("{e}; the spawned agent (pid {pid}) was killed"));
                return;
            }
            // What the operator just judged, so the re-review can be narrowed to
            // the rework (DESIGN.md §8) — off the loop, so nothing waits on `gh`.
            self.capture_reviewed(task_id);
        }
        let summary = sent.confirm(&self.dispatch_ctx);
        self.status = Some(if rejected {
            format!("{summary} — task returned to running")
        } else {
            summary
        });
        let refreshed = self.refresh();
        self.report(refreshed);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::probes::tests::{cap_env, settle_cap};
    use crate::app::tests::{app_with, dummy_ctx, key, scratch_env, type_str};
    use ratatui::crossterm::event::KeyCode;
    use voro_core::{LivenessSource, NewTask, Priority, Store};

    // --- jump-in keybinding ---

    /// A listing showing the fixture's session still going, and one showing it
    /// finished — what decides the jump-in verb.
    const LIVE_LISTING: &str = r#"[{"sessionId": "ref-1", "state": "working"}]"#;

    const FINISHED_LISTING: &str = r#"[{"sessionId": "ref-1", "state": "done"}]"#;

    /// The zombie shape the pid rule exists for: an entry the
    /// agent's listing leaves at `blocked` long after the session died, with
    /// no pid to check. It read as live while not-`done` meant live, which
    /// sent `A` at `claude attach <uuid>` — "No job matching" — and made `a`
    /// refuse a session there was nothing to attach to.
    const ZOMBIE_LISTING: &str = r#"[{"sessionId": "ref-1", "state": "blocked"}]"#;

    /// The same entry with a supervisor pid that is still around: a session
    /// genuinely stuck mid-turn — on a permission prompt, say — which stays
    /// live, attachable, and closed to a headless send.
    fn blocked_live_listing() -> String {
        format!(
            r#"[{{"sessionId": "ref-1", "state": "blocked", "pid": {}}}]"#,
            std::process::id()
        )
    }

    struct JumpIn {
        store: Store,
        ctx: crate::dispatch::DispatchCtx,
        task_id: i64,
        project_path: std::path::PathBuf,
        listing: std::path::PathBuf,
        /// Where the stub's `stop` verb records the reference it was fired at,
        /// so a test can tell a release that happened from one that did not.
        stopped: std::path::PathBuf,
    }

    /// Rewrite the canned listing, moving the session between live and
    /// finished under a task whose state stays where it is.
    pub(crate) fn write_listing(path: &std::path::Path, json: &str) {
        std::fs::write(path, json).unwrap();
    }

    /// A project with one dispatched task, its session's ref recorded, and a
    /// canned `sessions` listing the test can rewrite. `verbs` names the
    /// session verbs the stub agent defines, so a test can take one away. The
    /// stub lingers after printing its prompt, so a verb-less agent — whose
    /// liveness is the pid — keeps its task `running` through reconcile.
    ///
    /// The `message` verb lingers too: a send that exits non-zero inside its
    /// grace window is a send that did not happen, so the stub has
    /// to be a command that survives. It says what it is in a trailing comment,
    /// which the launch log records verbatim — that is what the assertions
    /// below read the rendered `{session}` out of.
    fn jump_in_env(verbs: &[&str], listing_json: &str) -> JumpIn {
        let (mut store, ctx, project_path) = scratch_env("jumpin", None);
        let listing = project_path.parent().unwrap().join("listing.json");
        write_listing(&listing, listing_json);
        let stopped = project_path.parent().unwrap().join("stopped");
        let templates = [
            ("sessions", format!("cat '{}'", listing.display())),
            ("attach", "agent attach {session}".into()),
            ("resume", "agent resume {session}".into()),
            (
                "message",
                "sleep 30 # agent message {session} {prompt_file}".into(),
            ),
            (
                "stop",
                format!("printf '%s' {{session}} >> '{}'", stopped.display()),
            ),
        ]
        .into_iter()
        .filter(|(verb, _)| verbs.contains(verb))
        .map(|(verb, template)| format!("{verb} = \"{template}\"\n"))
        .collect::<String>();
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}} && sleep 30\"\n{templates}"
            ),
        )
        .unwrap();
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        let session_id = store.sessions_for(task.id).unwrap()[0].id;
        store.set_session_ref(session_id, "ref-1").unwrap();
        JumpIn {
            store,
            ctx,
            task_id: task.id,
            project_path,
            listing,
            stopped,
        }
    }

    /// Every session verb, the ordinary configuration.
    fn all_verbs() -> &'static [&'static str] {
        &["sessions", "attach", "resume", "message", "stop"]
    }

    // --- nudging capped sessions back to work ---

    /// What a nudge was told, if anything.
    pub(crate) fn delivered(project_path: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(project_path.parent().unwrap().join("delivered.txt")).ok()
    }

    /// Which session the sweep released before sending, if it released one.
    fn nudge_stopped(project_path: &std::path::Path) -> Option<String> {
        std::fs::read_to_string(project_path.parent().unwrap().join("stopped.txt")).ok()
    }

    /// The headline case (DESIGN.md §8): one key puts every capped session whose
    /// window has reopened back to work, without the operator visiting any of
    /// them. Both the guards the quick-message key answers to are stood down —
    /// the task is `running` and its session is listed live — because the cap
    /// reading says the session is up and idle rather than mid-turn.
    #[test]
    fn u_nudges_a_capped_session_whose_window_has_reopened() {
        let (mut app, task_id, project_path) =
            cap_env(true, "You've hit your session limit · resets 9:50pm");
        settle_cap(&mut app, task_id, true);
        // An hour past the 21:50 the agent named.
        app.now_minutes = Some(22 * 60 + 50);

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(
            delivered(&project_path).as_deref().map(str::trim),
            Some(NUDGE),
            "the session was told to continue: {:?}",
            app.status
        );
        assert!(
            !app.caps.contains_key(&task_id),
            "the badge goes with the nudge, so a second press cannot double it"
        );
        assert_eq!(
            app.store.task(task_id).unwrap().state,
            TaskState::Running,
            "a nudge is a send, not a transition"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("nudged 1")),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The sweep releases its target before resuming it, and does so
    /// unconditionally (DESIGN.md §8). A capped session is `blocked` with its
    /// supervisor alive — it never reads `done`, so no reconcile pass will ever
    /// release it, and the rest rule's own test would answer "nothing to do"
    /// while the hold that refuses an in-place resume sits right there. The
    /// sweep has already walked past the liveness gate that makes that test
    /// sufficient, so it cannot lean on the test.
    #[test]
    fn u_releases_the_capped_session_before_resuming_it() {
        let (mut app, task_id, project_path) =
            cap_env(true, "You've hit your session limit · resets 9:50pm");
        settle_cap(&mut app, task_id, true);
        app.now_minutes = Some(22 * 60 + 50);

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(
            nudge_stopped(&project_path).as_deref(),
            Some("ref-1"),
            "the sweep released the session it was about to resume: {:?}",
            app.status
        );
        // And the send still went, at the same reference: a release, not a move.
        assert_eq!(
            delivered(&project_path).as_deref().map(str::trim),
            Some(NUDGE)
        );
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        assert_eq!(session.session_ref.as_deref(), Some("ref-1"));

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A release that fails means the hold is still there and the resume behind
    /// it could only be refused, so the nudge is abandoned before it spawns
    /// anything — the session keeps its badge and the sweep reports the refusal
    /// rather than counting a send that never happened.
    #[test]
    fn a_nudge_whose_release_fails_sends_nothing() {
        let (mut app, task_id, project_path) =
            cap_env(true, "You've hit your session limit · resets 9:50pm");
        settle_cap(&mut app, task_id, true);
        app.now_minutes = Some(22 * 60 + 50);
        set_verb(
            &app.dispatch_ctx.agents_path,
            "stop",
            "printf 'no such session' >&2; exit 1 # {session}",
        );

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(delivered(&project_path), None, "nothing was sent");
        assert!(
            app.caps.contains_key(&task_id),
            "the badge stays, since the session was never nudged"
        );
        let status = app.status.as_deref().unwrap_or("").to_string();
        assert!(status.contains("nudged 0"), "{status}");
        assert!(status.contains("refused"), "{status}");
        assert!(status.contains("could not be released"), "{status}");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A cap whose window has not reopened is left alone: nudging it would spend
    /// a send on a session the agent will only refuse again, and the badge is
    /// the operator's cue that there is nothing to do yet.
    #[test]
    fn u_leaves_a_capped_session_that_is_still_waiting() {
        let (mut app, task_id, project_path) =
            cap_env(true, "You've hit your session limit · resets 9:50pm");
        settle_cap(&mut app, task_id, true);
        // An hour short of the 21:50 the agent named.
        app.now_minutes = Some(20 * 60 + 50);

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(delivered(&project_path), None, "nothing was sent");
        assert!(
            app.caps.contains_key(&task_id),
            "the badge stands until the window opens"
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|s| s.contains("none has reached its reset")),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The key on a healthy fleet says so and sends nothing, so a stray press
    /// costs no agent turns.
    #[test]
    fn u_on_an_uncapped_fleet_sends_nothing() {
        let (mut app, _, project_path) = cap_env(true, "running the test suite");
        for _ in 0..20 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(delivered(&project_path), None);
        assert_eq!(app.status.as_deref(), Some("no session is capped"));

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A cap whose reset time never parsed is still nudgeable: the operator
    /// pressing the key is the judgement the clock could not supply, and a send
    /// that turns out to be early is refused by the agent rather than doing harm.
    #[test]
    fn u_nudges_a_cap_that_named_no_reset_time() {
        let (mut app, task_id, project_path) = cap_env(true, "Weekly limit reached");
        settle_cap(&mut app, task_id, true);
        assert_eq!(app.caps[&task_id].reset_minutes, None);

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(
            delivered(&project_path).as_deref().map(str::trim),
            Some(NUDGE),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The one badged session the sweep walks past (DESIGN.md §8): one that is
    /// retrying the request the limit rejected. It is mid-turn and will carry
    /// on by itself, and a nudge would not add a turn to it but end the one
    /// running, since the send stops its target first. So it is skipped however
    /// far past the named time the clock has gone — and said aloud, because a
    /// row passed over in silence reads as one the sweep failed to reach.
    #[test]
    fn u_leaves_a_retrying_session_alone() {
        let (mut app, task_id, project_path) = cap_env(
            true,
            "429 exceeded your rate limit · Retrying in 30s · attempt 3/10",
        );
        settle_cap(&mut app, task_id, true);
        assert!(app.caps[&task_id].retrying);
        app.now_minutes = Some(22 * 60 + 50);

        key(&mut app, KeyCode::Char('u'));

        assert_eq!(
            delivered(&project_path),
            None,
            "a mid-turn session is not sent into: {:?}",
            app.status
        );
        assert_eq!(
            nudge_stopped(&project_path),
            None,
            "nor stopped, which is what a send would have done to it first"
        );
        assert!(
            app.caps.contains_key(&task_id),
            "and it keeps its badge, since nothing was done about it"
        );
        assert_eq!(
            app.status.as_deref(),
            Some("1 capped session — retrying, none waiting on you")
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// `A` on a running task whose session is still listed queues the agent's
    /// `attach` command — ref substituted, project path as cwd — for main() to
    /// run with the TUI suspended.
    #[test]
    fn attach_key_prepares_the_attach_command_for_a_running_task() {
        let env = jump_in_env(all_verbs(), LIVE_LISTING);
        let project_path = env.project_path.clone();
        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");
        assert_eq!(request.cwd, project_path.to_str().unwrap());

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// `A` on a review task whose session has finished uses `resume` — the
    /// point is reopening it, not attaching to a live one.
    #[test]
    fn attach_key_uses_resume_for_a_finished_review_session() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A `--bg` session commonly outlives the `running` state, and `resume`
    /// refuses a session the supervisor still holds. A review task whose
    /// session is listed live attaches.
    #[test]
    fn attach_key_attaches_to_a_review_tasks_live_session() {
        let mut env = jump_in_env(all_verbs(), LIVE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A review task whose entry is a pid-less zombie resumes: `blocked` with
    /// nothing behind it is not a claim that anything is still running, and
    /// attaching to it fails at the agent.
    #[test]
    fn attach_key_resumes_a_review_tasks_zombie_session() {
        let mut env = jump_in_env(all_verbs(), ZOMBIE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The same entry with a live pid is the case the pid rule protects: a
    /// stalled-but-alive session is attached to, not resumed out from under.
    #[test]
    fn attach_key_attaches_to_a_blocked_session_with_a_live_pid() {
        let mut env = jump_in_env(all_verbs(), &blocked_live_listing());
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// And the mirror: a running task whose session has died since the last
    /// refresh resumes rather than attaching to nothing.
    #[test]
    fn attach_key_resumes_a_running_tasks_finished_session() {
        let env = jump_in_env(all_verbs(), LIVE_LISTING);
        let (project_path, listing, task_id) =
            (env.project_path.clone(), env.listing.clone(), env.task_id);

        let mut app = App::new(env.store, env.ctx).unwrap();
        // the session ends after the refresh that left the task running
        write_listing(&listing, FINISHED_LISTING);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// An agent that cannot report liveness falls back to task state, exactly
    /// as before liveness was consulted — the listing is not read, even
    /// though this one would have said the session had finished.
    #[test]
    fn attach_key_without_a_sessions_verb_follows_task_state() {
        let env = jump_in_env(&["attach", "resume"], FINISHED_LISTING);
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let mut app = App::new(env.store, env.ctx).unwrap();
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("an attach request");
        assert_eq!(request.command, "agent attach 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// An agent defining only one of the two verbs jumps in with that one —
    /// the built-in `codex` has no `attach` — rather than refusing a live
    /// session it has a way into.
    #[test]
    fn attach_key_falls_back_to_the_verb_the_agent_defines() {
        let env = jump_in_env(&["sessions", "resume"], LIVE_LISTING);
        let project_path = env.project_path.clone();
        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        let request = app.pending_attach.clone().expect("a resume request");
        assert_eq!(request.command, "agent resume 'ref-1'");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// With neither verb defined there is no way in, and the message names
    /// both rather than only the one the state would have picked.
    #[test]
    fn attach_key_without_either_verb_reports_both() {
        let env = jump_in_env(&["sessions"], LIVE_LISTING);
        let project_path = env.project_path.clone();
        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no attach or resume template"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The verb choice itself, over the grid the App tests can only sample:
    /// liveness wins where it is known, state stands in where it is not, and
    /// either way the chosen verb degrades to the one the agent defines.
    #[test]
    fn jump_verb_prefers_liveness_then_state_then_availability() {
        let (a, r) = (Some("attach {session}"), Some("resume {session}"));
        assert_eq!(jump_verb(Some(true), JumpVerb::Resume, a, r), a);
        assert_eq!(jump_verb(Some(false), JumpVerb::Attach, a, r), r);
        assert_eq!(jump_verb(None, JumpVerb::Attach, a, r), a);
        assert_eq!(jump_verb(None, JumpVerb::Resume, a, r), r);
        // only one verb defined: take it whichever way the choice went
        assert_eq!(jump_verb(Some(true), JumpVerb::Attach, None, r), r);
        assert_eq!(jump_verb(Some(false), JumpVerb::Resume, a, None), a);
        assert_eq!(jump_verb(Some(true), JumpVerb::Attach, None, None), None);
    }

    /// The state fallback, and the gate on which states offer a jump-in at all.
    #[test]
    fn state_jump_verb_covers_the_three_jumpable_states() {
        assert_eq!(state_jump_verb(TaskState::Running), Some(JumpVerb::Attach));
        assert_eq!(state_jump_verb(TaskState::Review), Some(JumpVerb::Resume));
        assert_eq!(state_jump_verb(TaskState::Stalled), Some(JumpVerb::Resume));
        assert_eq!(state_jump_verb(TaskState::Ready), None);
        assert_eq!(state_jump_verb(TaskState::Done), None);
    }

    /// Without a captured ref there is nothing to substitute into the verb;
    /// the key explains instead of queuing a broken command. The fixture's
    /// verb-less stub agent also exercises the pid-reconcile path: the dead
    /// session is finalised and the task lands in `stalled` (DESIGN.md §6/§8),
    /// whose jump-in is `resume`.
    #[test]
    fn attach_key_without_a_captured_ref_reports_and_does_nothing() {
        let (mut store, ctx, project_path) = scratch_env(
            "jumpin-noref",
            Some(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {prompt_file}\"\n\
                 attach = \"agent attach {session}\"\n\
                 resume = \"agent resume {session}\"\n",
            ),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        // the stub exits immediately; wait for it so App::new's
        // reconcile-on-read reliably finds the pid dead
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut app = App::new(store, ctx).unwrap();
        // the dead session's task is stalled by reconcile-on-read, so it
        // belongs to the queue, not the running strip
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Stalled);
        assert!(app.running.is_empty(), "{:?}", app.running);
        app.toggle_screen();
        key(&mut app, KeyCode::Char('A'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no session reference"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// States with no session to jump into no-op with an explanation.
    #[test]
    fn attach_key_on_a_ready_task_reports_the_states_that_work() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('A'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("jump-in"),
            "{:?}",
            app.status
        );
    }

    // --- quick message into a task's session ---

    /// The gate on which states take a quick message: the ones whose session
    /// is open and between turns. `running`/`refining` are mid-turn and
    /// `stalled` is dead, so all three belong to `A` or to redispatch.
    #[test]
    fn state_accepts_message_covers_the_three_messageable_states() {
        for state in [TaskState::NeedsInput, TaskState::Review, TaskState::Waiting] {
            assert!(state_accepts_message(state), "{state}");
        }
        for state in [
            TaskState::Running,
            TaskState::Refining,
            TaskState::Stalled,
            TaskState::Ready,
            TaskState::Done,
        ] {
            assert!(!state_accepts_message(state), "{state}");
        }
    }

    /// Type a message into the quick-message input and submit it.
    fn send_message(app: &mut App, text: &str) {
        key(app, KeyCode::Char('a'));
        assert!(
            matches!(
                app.mode,
                Mode::Prompt {
                    kind: PromptKind::SessionMessage,
                    ..
                }
            ),
            "a should open the message input: {:?}",
            app.status
        );
        type_str(app, text);
        key(app, KeyCode::Enter);
    }

    /// The launch log, which records every command Voro spawns before it runs.
    /// Absent until something is spawned, which is itself an assertion a test
    /// wants to make.
    fn launches(root: &std::path::Path) -> String {
        std::fs::read_to_string(root.join("sessions").join("launches.log")).unwrap_or_default()
    }

    /// Every message to a review task is a reject-with-feedback: the
    /// transition runs first, so the feedback is in the body and the event log
    /// before the send, and the task is back on its agent in `running`. The
    /// agent here reports no `sessions` listing, so liveness is unknowable and
    /// the send proceeds — a missing signal is never a refusal.
    #[test]
    fn message_on_a_review_task_rejects_with_feedback_and_sends() {
        let mut env = jump_in_env(&["attach", "resume", "message"], FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        assert!(
            app.store
                .events_for(task_id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "feedback"),
            "the rejection is logged"
        );
        let log = launches(&root);
        assert!(log.contains("agent message 'ref-1'"), "{log}");
        // fire-and-forget: the TUI never suspends for it
        assert!(app.pending_attach.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `needs-input` task transitions not at all — per DESIGN.md §6 the
    /// answer lives in the transcript, and the agent's own `voro resume` moves
    /// the task back to `running`. Its session row still follows the send, so
    /// the answer's process is what the reconciler reads.
    #[test]
    fn message_on_a_needs_input_task_sends_without_transitioning() {
        let mut env = jump_in_env(&["attach", "resume", "message"], FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Ask("which crate?".into()))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        send_message(&mut app, "voro-core");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::NeedsInput);
        assert!(!task.body.contains("voro-core"), "{}", task.body);
        assert!(launches(&root).contains("agent message 'ref-1'"));
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        assert!(crate::session_probe::pid_is_alive(session.pid.unwrap()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Rewrite one of the stub agent's verbs — the config is loaded fresh on
    /// every send, so a test can change what a send *does* after the environment
    /// is built.
    /// Define or redefine one verb of the stub agent, by path — so a test can
    /// also do it *after* the App is built, which is what isolates a verb the
    /// reconcile-on-read pass would otherwise have fired first.
    fn set_verb(agents_path: &std::path::Path, verb: &str, template: &str) {
        let prefix = format!("{verb} = ");
        let mut config: String = std::fs::read_to_string(agents_path)
            .unwrap()
            .lines()
            .filter(|line| !line.starts_with(&prefix))
            .map(|line| format!("{line}\n"))
            .collect();
        config.push_str(&format!("{prefix}\"{template}\"\n"));
        std::fs::write(agents_path, config).unwrap();
    }

    fn set_message_verb(env: &JumpIn, template: &str) {
        set_verb(&env.ctx.agents_path, "message", template);
    }

    /// The send is what the
    /// rejection hangs off, so a message the agent refuses — a supervisor-held
    /// session, a stale reference — leaves the task in `review` with its body
    /// untouched and the refusal on the status line. Recording feedback the
    /// agent never received, and returning the task to `running` on the
    /// strength of it, is the one outcome worse than not sending.
    #[test]
    fn a_refused_send_leaves_the_review_task_exactly_where_it_was() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        set_message_verb(
            &env,
            "printf 'Session is currently running as a background agent' >&2; \
             exit 1 # {session} {prompt_file}",
        );
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Review);
        assert!(!task.body.contains("Feedback"), "{}", task.body);
        assert!(
            !task.body.contains("the tests are missing"),
            "{}",
            task.body
        );
        assert!(
            !app.store
                .events_for(task_id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "feedback"),
            "no rejection is logged for a message that never landed"
        );
        // the agent's own account of the refusal, out of the log and onto the
        // status line
        let status = app.status.as_deref().unwrap_or("").to_string();
        assert!(status.contains("background agent"), "{status}");
        assert!(status.contains("unchanged"), "{status}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A verb that forks rather than resuming in place: the session row follows
    /// the fork, so the next message and the next jump-in address the
    /// conversation where it actually continued — and the rejection lands as
    /// usual behind the confirmed send.
    #[test]
    fn a_forking_send_moves_the_session_to_the_reference_it_opened() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        set_message_verb(
            &env,
            "sleep 30 # agent message {session} --session-id {new_session} {prompt_file}",
        );
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        let new_ref = session.session_ref.expect("a reference");
        assert_ne!(new_ref, "ref-1", "the row followed the fork");
        assert!(crate::session_probe::pid_is_alive(session.pid.unwrap()));
        assert!(
            launches(&root).contains(&format!("--session-id '{new_ref}'")),
            "the recorded reference is the one the command was given"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // --- releasing a session the agent still holds ---

    /// A review task whose session verbs are all defined *except* `stop`, plus
    /// the config path a test adds one at afterwards. Withholding it until the
    /// App exists is what separates the send path's own inline release from the
    /// reconcile-on-read pass that would otherwise have made it first.
    fn send_env(listing: &str) -> (App, i64, std::path::PathBuf, SendPaths) {
        let mut env = jump_in_env(&["sessions", "attach", "resume", "message"], listing);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let root = env.project_path.parent().unwrap().to_path_buf();
        let paths = SendPaths {
            agents_path: env.ctx.agents_path.clone(),
            stopped: env.stopped.clone(),
        };
        let task_id = env.task_id;
        (App::new(env.store, env.ctx).unwrap(), task_id, root, paths)
    }

    /// The two files a send test writes to and reads back after the App has
    /// taken ownership of everything else.
    struct SendPaths {
        agents_path: std::path::PathBuf,
        stopped: std::path::PathBuf,
    }

    impl SendPaths {
        /// A `stop` verb that records the reference it was fired at.
        fn recording_stop(&self) {
            set_verb(
                &self.agents_path,
                "stop",
                &format!("printf '%s' {{session}} >> '{}'", self.stopped.display()),
            );
        }

        /// What the stop verb recorded; empty when no stop ran.
        fn stopped_refs(&self) -> String {
            std::fs::read_to_string(&self.stopped).unwrap_or_default()
        }
    }

    /// The rest rule (DESIGN.md §8): a session the agent is still holding with
    /// its turn ended is released by the send itself, at its own reference, and
    /// then resumed in place. Without that the hold would refuse the resume and
    /// the feedback would go nowhere.
    #[test]
    fn a_send_releases_a_session_the_agent_still_holds_at_rest() {
        let (mut app, task_id, root, paths) = send_env(FINISHED_LISTING);
        paths.recording_stop();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        assert_eq!(paths.stopped_refs(), "ref-1");
        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        // Released, not moved: the send is addressed at the same reference the
        // stop named, and the row still holds it afterwards.
        assert!(
            launches(&root).contains("agent message 'ref-1'"),
            "{}",
            launches(&root)
        );
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        assert_eq!(session.session_ref.as_deref(), Some("ref-1"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The other half of the same guard: nothing about a send stops a session
    /// unconditionally. A `blocked` entry is a turn still under way and an
    /// absent one has nothing registered to release, so both go straight to the
    /// send — the stop verb is defined and simply never fires.
    #[test]
    fn a_send_makes_no_stop_when_the_session_is_not_listed_at_rest() {
        for (name, listing) in [("blocked", ZOMBIE_LISTING), ("absent", "[]")] {
            let (mut app, task_id, root, paths) = send_env(listing);
            paths.recording_stop();
            app.toggle_screen();
            send_message(&mut app, "the tests are missing");

            assert_eq!(
                paths.stopped_refs(),
                "",
                "{name}: stopped a session mid-turn"
            );
            assert_eq!(
                app.store.task(task_id).unwrap().state,
                TaskState::Running,
                "{name}"
            );
            assert!(launches(&root).contains("agent message 'ref-1'"), "{name}");

            let _ = std::fs::remove_dir_all(&root);
        }
    }

    /// A release that fails is a session still held, so the resume behind it
    /// could only be refused. The send is abandoned before it is spawned and the
    /// task is left exactly where it was — the same commit-nothing rule a
    /// refused send already answers to.
    #[test]
    fn a_failed_release_refuses_the_send_and_commits_nothing() {
        let (mut app, task_id, root, paths) = send_env(FINISHED_LISTING);
        set_verb(
            &paths.agents_path,
            "stop",
            "printf 'no such session' >&2; exit 1 # {session}",
        );
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Review);
        assert!(
            !task.body.contains("the tests are missing"),
            "{}",
            task.body
        );
        assert!(
            !app.store
                .events_for(task_id)
                .unwrap()
                .iter()
                .any(|e| e.kind == "feedback"),
            "no rejection is logged for a message that was never sent"
        );
        assert!(
            !launches(&root).contains("agent message"),
            "{}",
            launches(&root)
        );
        let status = app.status.as_deref().unwrap_or("").to_string();
        assert!(status.contains("could not be released"), "{status}");
        assert!(status.contains("unchanged"), "{status}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An agent defining no `stop` verb is one Voro was never going to release,
    /// so the send goes as it always did and whatever the agent makes of it is
    /// the agent's answer — a refusal there, not a refusal here.
    #[test]
    fn a_send_without_a_stop_verb_is_unaffected() {
        let (mut app, task_id, root, _paths) = send_env(FINISHED_LISTING);
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);
        assert!(launches(&root).contains("agent message 'ref-1'"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A confirmed in-place send moves the pid and nothing else: one session id
    /// for the task's whole life, which is what the operator's `voro-<id>` name
    /// is attached to.
    #[test]
    fn a_confirmed_in_place_send_moves_the_pid_and_keeps_the_reference() {
        let (mut app, task_id, root, _paths) = send_env(FINISHED_LISTING);
        let before = app.store.sessions_for(task_id).unwrap().remove(0);
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let after = app.store.sessions_for(task_id).unwrap().remove(0);
        assert_eq!(after.id, before.id, "the same session row");
        assert_eq!(after.session_ref.as_deref(), Some("ref-1"));
        assert_ne!(after.pid, before.pid, "the process carrying the turn");
        assert!(crate::session_probe::pid_is_alive(after.pid.unwrap()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session still running is mid-turn, so the headless send is refused
    /// and the operator is pointed at the terminal instead. Nothing is sent
    /// and — the part that matters — nothing is transitioned.
    #[test]
    fn message_refuses_a_session_that_is_still_running() {
        let mut env = jump_in_env(all_verbs(), LIVE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "one more thing");

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Review);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("still running"),
            "{:?}",
            app.status
        );
        assert!(!launches(&root).contains("agent message"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The refusal's other half: a pid-less `blocked` zombie is
    /// not a session still running, so the message goes headlessly — and the
    /// send that lands is what the task then rides on. The reconcile that
    /// follows finds the same zombie in the listing but the send's own process
    /// on the row, so the task stays `running` rather than being stalled out
    /// from under the agent now answering.
    #[test]
    fn message_sends_into_a_zombie_session() {
        let mut env = jump_in_env(all_verbs(), ZOMBIE_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        assert_eq!(task.state, TaskState::Running);
        assert!(launches(&root).contains("agent message 'ref-1'"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same entry with a live pid keeps the refusal: the session is stuck
    /// mid-turn with its supervisor still there, so the operator wants `A`.
    #[test]
    fn message_refuses_a_blocked_session_with_a_live_pid() {
        let mut env = jump_in_env(all_verbs(), &blocked_live_listing());
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "one more thing");

        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Review);
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("still running"),
            "{:?}",
            app.status
        );
        assert!(!launches(&root).contains("agent message"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session the listing reports finished must not be stalled by the next
    /// reconcile seconds after a rejection was sent into it: the send's own
    /// process is on the row, so the task rides `running` for as long as the
    /// turn takes.
    #[test]
    fn message_to_a_finished_session_keeps_the_task_running() {
        let mut env = jump_in_env(all_verbs(), FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let (project_path, task_id) = (env.project_path.clone(), env.task_id);
        let root = project_path.parent().unwrap().to_path_buf();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        send_message(&mut app, "the tests are missing");

        let task = app.store.task(task_id).unwrap();
        assert_eq!(task.state, TaskState::Running);
        assert!(task.body.contains("the tests are missing"), "{}", task.body);
        assert!(launches(&root).contains("agent message 'ref-1'"));
        let session = app.store.sessions_for(task_id).unwrap().remove(0);
        assert!(session.ended_at.is_none());
        assert!(
            crate::session_probe::pid_is_alive(session.pid.unwrap()),
            "the row carries the send's own process, not the dispatch launcher"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An agent with no `message` verb degrades per-verb: the key explains and
    /// names the one that still works, and no input opens.
    #[test]
    fn message_without_the_verb_reports_and_points_at_the_jump_in() {
        let mut env = jump_in_env(&["sessions", "attach", "resume"], FINISHED_LISTING);
        env.store
            .apply(env.task_id, Action::Complete(None))
            .unwrap();
        let project_path = env.project_path.clone();

        let mut app = App::new(env.store, env.ctx).unwrap();
        app.toggle_screen();
        key(&mut app, KeyCode::Char('a'));

        assert!(matches!(app.mode, Mode::Normal));
        let status = app.status.as_deref().unwrap_or("").to_string();
        assert!(status.contains("no message template"), "{status}");
        assert!(status.contains('A'), "{status}");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// Without a captured reference there is nothing to resume into, so the
    /// key explains rather than opening an input that could not be sent.
    #[test]
    fn message_without_a_captured_ref_reports_and_opens_nothing() {
        // No `sessions` verb, so dispatch captures no reference at all.
        let (mut store, ctx, project_path) = scratch_env(
            "message-noref",
            Some(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {prompt_file}\"\n\
                 message = \"agent message {session} {prompt_file}\"\n",
            ),
        );
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "Do the thing".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        crate::dispatch::dispatch(&mut store, &ctx, task.id, None).unwrap();
        store.apply(task.id, Action::Complete(None)).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        key(&mut app, KeyCode::Char('a'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no session reference"),
            "{:?}",
            app.status
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A task nobody ever dispatched has no session to say anything into.
    #[test]
    fn message_on_a_task_with_no_session_reports() {
        let mut app = app_with(&[TaskState::Review]);
        key(&mut app, KeyCode::Char('a'));

        assert!(matches!(app.mode, Mode::Normal));
        assert!(
            app.status
                .as_deref()
                .unwrap_or("")
                .contains("no recorded session"),
            "{:?}",
            app.status
        );
    }

    /// `l` on a stalled task queues `$PAGER <log>` for main() to run with the
    /// TUI suspended, in the project's checkout.
    #[test]
    fn log_key_pages_a_stalled_tasks_session_log() {
        let mut app = app_with(&[TaskState::Stalled]);
        key(&mut app, KeyCode::Char('l'));

        let request = app.pending_attach.clone().expect("a pager request");
        assert_eq!(request.command, "${PAGER:-less} '/tmp/demo/s.log'");
        assert_eq!(request.cwd, "/tmp/demo");
    }

    /// The key is not gated on state: a task whose session is
    /// still open — here parked mid-flight into needs-input — pages the same
    /// way, answering "what is this session doing?".
    #[test]
    fn log_key_pages_an_open_sessions_log_in_any_state() {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "mid-flight".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        store
            .record_dispatch(
                task.id,
                "claude",
                Some(1),
                LivenessSource::Pid,
                Some("/tmp/demo/open.log"),
            )
            .unwrap();
        store.apply(task.id, Action::Ask("A or B?".into())).unwrap();

        let mut app = App::new(store, dummy_ctx()).unwrap();
        assert_eq!(
            app.store.task(task.id).unwrap().state,
            TaskState::NeedsInput
        );
        key(&mut app, KeyCode::Char('l'));

        let request = app.pending_attach.clone().expect("a pager request");
        assert_eq!(request.command, "${PAGER:-less} '/tmp/demo/open.log'");
        assert_eq!(request.cwd, "/tmp/demo");
    }

    /// `l` on a task nothing ever dispatched explains itself instead of
    /// paging nothing.
    #[test]
    fn log_key_on_a_ready_task_reports_and_does_nothing() {
        let mut app = app_with(&[TaskState::Ready]);
        key(&mut app, KeyCode::Char('l'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("no session"),
            "{:?}",
            app.status
        );
    }

    /// A stalled session that recorded no log path refuses with an
    /// explanation rather than handing the pager an empty argument.
    #[test]
    fn log_key_without_a_recorded_log_path_reports() {
        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: project.id,
                repo_id: None,
                title: "died without a log".into(),
                body: String::new(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        let (_, session) = store
            .record_dispatch(task.id, "claude", Some(1), LivenessSource::Pid, None)
            .unwrap();
        store.reconcile_session(session.id, false, false).unwrap();

        let mut app = App::new(store, dummy_ctx()).unwrap();
        key(&mut app, KeyCode::Char('l'));

        assert!(app.pending_attach.is_none());
        assert!(
            app.status.as_deref().unwrap_or("").contains("no log path"),
            "{:?}",
            app.status
        );
    }
}
