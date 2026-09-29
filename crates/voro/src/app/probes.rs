//! Background probes (DESIGN.md §8): the review task's conflict verdict and
//! captured revision, and the cap probes behind the running strip's badges.

use voro_core::{AgentsConfig, TaskState, render_cap};

use super::App;

/// One in-flight session the usage-cap readings are taken for (DESIGN.md §8),
/// resolved on refresh where the agents config is already open so the tick that
/// starts a probe does no I/O of its own to decide what to probe.
///
/// It names two readings, on two different scales. `logs` replays *this
/// session's* screen and is taken per row; `cap` asks the account about the
/// window this session's *model* is metered against, so rows asking the same
/// question share one answer.
///
/// That question is carried already rendered, and is also the key the answer is
/// held under, because it is exactly what distinguishes one reading from
/// another: an agent asking per model splits into one question per model in
/// flight, and an agent whose template names no model collapses to a single
/// question for every row it runs — without either case being special.
#[derive(Debug, Clone)]
pub(super) struct CapTarget {
    task_id: i64,
    /// The reference the agent knows this session by.
    session_ref: String,
    /// The agent's `logs` verb, without which there is no target at all.
    logs: String,
    /// The agent's `cap` verb with this session's model bound, where it defines
    /// one.
    cap: Option<String>,
}

impl App {
    /// The selected task's tracked PR URL, when it is a `review` task carrying
    /// one — the only selection there is anything to probe for (DESIGN.md §8).
    /// Read from the refreshed rows rather than the store, so the tick costs no
    /// query.
    fn review_pr_url(&self, id: i64) -> Option<String> {
        self.all
            .iter()
            .find(|r| r.task.id == id)
            .filter(|r| r.task.state == TaskState::Review)
            .and_then(|r| r.task.pr_url.clone())
    }

    /// Advance the stale-branch probe one tick (DESIGN.md §8): collect a
    /// finished verdict and start a new probe when one is due. Both halves are
    /// non-blocking — the `gh` call itself runs on a background thread — so the
    /// event loop never stalls on the network, and the probe only starts once
    /// the selection has rested (`probe::SETTLE`), so scrolling through a queue
    /// of review tasks spawns nothing for the rows passed over. A verdict that
    /// arrives after the selection has moved on is discarded rather than shown
    /// against the wrong task.
    pub fn poll_conflict_probe(&mut self) {
        let selected = self.selected_task_id();
        // The verdict belongs to the row it was taken for; moving off it means
        // there is nothing to show, and coming back re-probes for a fresh one.
        if self
            .conflict_selected
            .is_some_and(|(id, _)| Some(id) != selected)
        {
            self.conflict_selected = None;
        }
        if let Some((id, verdict)) = self.probe.take_result()
            && Some(id) == selected
        {
            self.conflict_selected = Some((id, verdict.conflicts()));
        }

        let target = selected.and_then(|id| self.review_pr_url(id).map(|url| (id, url)));
        let inputs = crate::probe::ProbeInputs {
            target: target.as_ref().map(|(id, _)| *id),
            cached: self.conflict_selected.map(|(id, _)| id),
            in_flight: self.probe.in_flight(),
            rested: self.probe.settle(selected, std::time::Instant::now()),
        };
        if let Some((id, url)) = target
            && crate::probe::probe_due(inputs)
        {
            self.probe.start(id, url);
        }
    }

    /// Capture the revision a rejection was made against, off the event loop
    /// (DESIGN.md §8). The keypress gains nothing by waiting for `gh`: the
    /// value is read only when the rework comes back for re-review, minutes or
    /// hours later, and rejecting has just moved the task to `running`, where
    /// neither read path consults it. A task with neither a PR nor a branch has
    /// nothing to read, so it starts nothing.
    pub(super) fn capture_reviewed(&mut self, task_id: i64) {
        // The head is read a moment after the keypress rather than at it, so a
        // rework commit pushed inside that window would be captured as reviewed
        // and left out of the delta. `voro reject` on the CLI stays synchronous
        // for anyone who wants the tight capture.
        if let Some(source) = crate::pr::reviewed_source(&self.store, task_id) {
            self.capture.start(task_id, source);
        }
    }

    /// Which in-flight sessions can be asked whether they are sitting on a
    /// usage cap (DESIGN.md §8). A target needs all three of a strip row with
    /// work under way, an open session carrying the reference the agent knows
    /// it by, and an agent defining a `logs` verb — so an agent without one
    /// contributes no targets and is probed for nothing, which is how the whole
    /// feature stays absent for `codex` rather than failing loudly on it.
    ///
    /// It carries the agent's `cap` verb rendered for the model this session
    /// launched with, because that is what the second reading asks and this is
    /// where the config is already open. The model is resolved by the same rule
    /// the launch resolved it by — the deeper model for a deep task — since a
    /// subscription meters each strong model separately and a reading taken on
    /// the wrong one answers about the wrong window.
    pub(super) fn resolve_cap_targets(&self, config: Option<&AgentsConfig>) -> Vec<CapTarget> {
        let Some(config) = config else {
            return Vec::new();
        };
        self.running
            .iter()
            .filter(|r| matches!(r.task_state, TaskState::Running | TaskState::Refining))
            .filter_map(|r| {
                let session = self.last_sessions.get(&r.task_id)?;
                if session.ended_at.is_some() {
                    return None;
                }
                let session_ref = session.session_ref.clone()?;
                let agent = config.agent(&session.agent)?;
                let deep = self.store.task(r.task_id).is_ok_and(|t| t.deep);
                Some(CapTarget {
                    task_id: r.task_id,
                    session_ref,
                    logs: agent.logs()?.to_string(),
                    cap: agent
                        .cap()
                        .map(|template| render_cap(template, agent.model_for(deep))),
                })
            })
            .collect()
    }

    /// What the account said about the window holding this task's session, when
    /// anything has been read for it (DESIGN.md §8). The badge and the sweep
    /// both go through here rather than reaching for the map, so the answer a
    /// row shows is the one to the question that row asks.
    pub fn account_cap(&self, task_id: i64) -> Option<&voro_core::AccountCap> {
        let target = self.cap_targets.iter().find(|t| t.task_id == task_id)?;
        self.account_caps.get(target.cap.as_ref()?)
    }

    /// The cap window for one badged task: its session's screen and its
    /// account's instant resolved into the single answer both the badge and the
    /// sweep read (DESIGN.md §8).
    pub fn cap_window(&self, task_id: i64) -> Option<voro_core::CapWindow> {
        let reading = self.caps.get(&task_id)?;
        Some(voro_core::CapWindow::resolve(
            reading,
            self.account_cap(task_id),
            self.now_minutes,
            self.now_epoch,
        ))
    }

    /// Advance the usage-cap readings behind the running strip's badge
    /// (DESIGN.md §8). Both halves are non-blocking: the `logs` verb runs on a
    /// background thread, because replaying a session's screen takes the better
    /// part of a second and the render path may never wait on that.
    ///
    /// A reading that comes back empty *removes* the badge rather than leaving
    /// the last one standing, which is the whole of the self-clearing rule: the
    /// operator continues a capped session, its next output no longer says
    /// "limit reached", and the badge is gone on the following pass.
    ///
    /// The account reading rides the same pass and is gated on the badges this
    /// one produces: it is asked only while a session of that agent is sitting
    /// on a cap, because unlike the screen replay it spends an API call to ask
    /// (DESIGN.md §8).
    pub fn poll_cap_probes(&mut self) {
        for (task_id, reading) in self.cap_probe.take_results() {
            match reading {
                Some(reading) => {
                    self.caps.insert(task_id, reading);
                }
                None => {
                    self.caps.remove(&task_id);
                }
            }
        }
        for (question, reading) in self.account_probe.take_results() {
            match reading {
                Some(reading) => {
                    self.account_caps.insert(question, reading);
                }
                None => {
                    self.account_caps.remove(&question);
                }
            }
        }

        // A task that has left the strip — finished, stalled, redispatched —
        // keeps neither a badge nor a debounce.
        let live: std::collections::HashSet<i64> =
            self.cap_targets.iter().map(|t| t.task_id).collect();
        self.caps.retain(|id, _| live.contains(id));
        self.cap_probe.retain(&live);

        let now = std::time::Instant::now();
        self.refresh_clock(now);

        let due: Vec<CapTarget> = self
            .cap_targets
            .iter()
            .filter(|t| self.cap_probe.due(t.task_id, now))
            .cloned()
            .collect();
        for target in due {
            self.cap_probe
                .start(target.task_id, target.session_ref, target.logs, now);
        }

        // A question is worth asking only while a session it answers for is
        // *held*, and stops being worth holding an answer to the moment none
        // is. A session retrying its rejected request is badged but not held —
        // it is mid-turn and will carry on by itself — so it buys no reading,
        // which matters here more than elsewhere because the reading is bought
        // with an API call.
        let asked_for: std::collections::HashSet<String> = self
            .cap_targets
            .iter()
            .filter(|t| self.caps.get(&t.task_id).is_some_and(|r| !r.retrying))
            .filter_map(|t| t.cap.clone())
            .collect();
        self.account_caps.retain(|q, _| asked_for.contains(q));
        self.account_probe.retain(&asked_for);

        let Some(now_epoch) = self.now_epoch else {
            return;
        };
        // Deduplicated by the question itself, so several capped sessions
        // asking the same thing — same agent, same model — are one call.
        let asking: Vec<String> = asked_for
            .iter()
            .filter(|question| self.account_probe.due(question, now, self.now_epoch))
            .cloned()
            .collect();
        for question in asking {
            self.account_probe.start(question, now, now_epoch);
        }
    }

    /// Hand back a reading as though a background probe had produced it, so
    /// the drain half can be tested without waiting out a real interval.
    #[cfg(test)]
    pub fn inject_cap_result(&mut self, task_id: i64, reading: Option<voro_core::CapReading>) {
        self.cap_probe.inject_result(task_id, reading);
    }

    /// Hand back an account reading the same way, so the precedence the badge
    /// and the sweep read it by can be tested without spending an API call.
    #[cfg(test)]
    pub fn inject_account_cap(&mut self, agent: &str, reading: Option<voro_core::AccountCap>) {
        self.account_probe.inject_result(agent, reading);
    }

    /// How many in-flight sessions can be read for a cap this pass.
    #[cfg(test)]
    pub fn cap_target_ids(&self) -> Vec<i64> {
        self.cap_targets.iter().map(|t| t.task_id).collect()
    }

    /// The account question this task's row asks — the `cap` verb with its
    /// session's model bound — which is also the key its answer is held under.
    #[cfg(test)]
    pub fn cap_question(&self, task_id: i64) -> Option<String> {
        self.cap_targets
            .iter()
            .find(|t| t.task_id == task_id)?
            .cap
            .clone()
    }

    /// Keep the wall clock the reset badge is judged against roughly current,
    /// without paying for it on frames where nothing reads it.
    fn refresh_clock(&mut self, now: std::time::Instant) {
        const CLOCK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
        if self.caps.is_empty() {
            return;
        }
        if self
            .clock_read_at
            .is_some_and(|at| now.saturating_duration_since(at) < CLOCK_INTERVAL)
        {
            return;
        }
        self.clock_read_at = Some(now);
        self.now_minutes = crate::session_probe::local_minutes();
        self.now_epoch = crate::session_probe::local_epoch();
    }

    /// Record every revision a background capture has finished (DESIGN.md §8).
    /// Each belongs to the task it was captured for, not to the selection, so
    /// nothing is discarded here. Errors are swallowed as the synchronous path
    /// swallows them: an unrecorded revision costs a full diff on the
    /// re-review, never a failed reject. Quitting before a capture lands loses
    /// it the same way, which is why no refresh follows either — nothing
    /// rendered reads the reviewed revision, which `pr` and `open` read on
    /// demand.
    pub fn poll_reviewed_capture(&mut self) {
        for (task_id, sha) in self.capture.take_results() {
            let _ = self.store.record_reviewed(task_id, &sha);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::sessions::NUDGE;
    use crate::app::sessions::tests::{delivered, write_listing};
    use crate::app::tests::{app_with, key, scratch_env};
    use ratatui::crossterm::event::KeyCode;
    use voro_core::{NewTask, Priority};

    // --- capped-but-alive sessions ---

    /// [`cap_env`] with an account-level `cap` verb as well, for the
    /// tests that watch what asking the account costs. The verb is a plain
    /// template line, so a test can spell one that records every time it ran.
    pub(crate) fn cap_env(define_logs: bool, logs_output: &str) -> (App, i64, std::path::PathBuf) {
        cap_env_with(define_logs, logs_output, "")
    }

    /// A project with one live dispatch whose agent's `logs` verb prints
    /// `logs_output`, which is the whole of what the cap probe reads.
    /// `define_logs` takes the verb away, for the degradation case.
    fn cap_env_with(
        define_logs: bool,
        logs_output: &str,
        cap_verb: &str,
    ) -> (App, i64, std::path::PathBuf) {
        let (mut store, ctx, project_path) = scratch_env("caps", None);
        let listing = project_path.parent().unwrap().join("listing.json");
        // The session stays listed live, so reconcile-on-read leaves the task
        // `running` and the strip keeps its row while the probe runs.
        write_listing(
            &listing,
            &format!(
                r#"[{{"sessionId": "ref-1", "state": "working", "pid": {}}}]"#,
                std::process::id()
            ),
        );
        // Double-quoted in the shell so a fixture can carry the apostrophe the
        // agent's real cap message has in it.
        let logs = if define_logs {
            format!("logs = \"printf '%s' \\\"{logs_output}\\\" # {{session}}\"\n")
        } else {
            String::new()
        };
        // The nudge sweep goes out through the same `message` verb
        // the quick-message key uses, so the stub defines one that records what
        // it was told and exits — a delivered send, as far as the caller can see.
        let delivered = project_path.parent().unwrap().join("delivered.txt");
        // A capped session is `blocked` with its supervisor alive, so the hold
        // that would refuse an in-place resume is still there and the sweep has
        // to release it itself (DESIGN.md §8). The stub records what it was
        // fired at, so a test can see that it ran and at which session.
        let stopped = project_path.parent().unwrap().join("stopped.txt");
        std::fs::write(
            &ctx.agents_path,
            format!(
                "default_agent = \"stub\"\n\n[agents.stub]\n\
                 dispatch = \"cat {{prompt_file}} && sleep 30\"\n\
                 sessions = \"cat '{}'\"\n\
                 message = \"cat {{prompt_file}} >> '{}' # {{session}}\"\n\
                 stop = \"printf '%s' {{session}} >> '{}'\"\n{logs}{cap_verb}",
                listing.display(),
                delivered.display(),
                stopped.display()
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
                title: "held work".into(),
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
        let mut app = App::new(store, ctx).unwrap();
        app.refresh().unwrap();
        (app, task.id, project_path)
    }

    /// Drive the probe until its reading lands, which is a background thread
    /// running a subprocess and so not instant.
    pub(crate) fn settle_cap(app: &mut App, task_id: i64, want: bool) {
        for _ in 0..200 {
            app.poll_cap_probes();
            if app.caps.contains_key(&task_id) == want {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the cap reading never settled to {want}");
    }

    /// The headline case (DESIGN.md §8): a dispatch alive and sitting on a cap
    /// is read as capped, with the reset time the agent named — and the task is
    /// left exactly where it was, because the session is intact and will resume
    /// on its own.
    #[test]
    fn a_live_capped_dispatch_is_read_with_its_reset_time() {
        let (mut app, task_id, project_path) =
            cap_env(true, "You've hit your session limit · resets 9:50pm");
        assert_eq!(app.cap_target_ids(), vec![task_id]);

        settle_cap(&mut app, task_id, true);
        assert_eq!(
            app.caps[&task_id].reset_label().as_deref(),
            Some("21:50"),
            "the reset time is read off the agent's own output"
        );
        assert_eq!(
            app.store.task(task_id).unwrap().state,
            TaskState::Running,
            "a cap is a display fact, not a transition"
        );
        assert!(
            app.store.sessions_for(task_id).unwrap()[0]
                .ended_at
                .is_none(),
            "the session stays open"
        );

        // Continuing the session displaces the cap message; the next reading
        // comes back empty and the badge goes with it.
        app.inject_cap_result(task_id, None);
        settle_cap(&mut app, task_id, false);

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A working session is read and badged for nothing, so the probe running
    /// at all costs an uncapped fleet no marks.
    #[test]
    fn a_live_healthy_dispatch_is_read_as_uncapped() {
        let (mut app, task_id, project_path) = cap_env(true, "running the test suite");
        for _ in 0..20 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.caps.is_empty(), "{:?}", app.caps);
        assert_eq!(app.store.task(task_id).unwrap().state, TaskState::Running);

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// An agent defining no `logs` verb is probed for nothing at all — no
    /// target, no subprocess, no badge, and no error either. This is the whole
    /// of what `codex` sees of this feature.
    #[test]
    fn an_agent_without_the_verb_is_never_probed() {
        let (mut app, _, project_path) = cap_env(false, "Session limit reached");
        assert!(app.cap_target_ids().is_empty());
        for _ in 0..5 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.caps.is_empty(), "{:?}", app.caps);

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    // --- the account's own reset instant ---

    /// A stub `cap` verb that asks on the session's model, so the tests below
    /// exercise the rendering the real one depends on. `out` is what it prints.
    fn cap_verb(out: &str) -> String {
        format!(
            "cap = \"printf '%s' '{out}' # {{model}}\"\nmodel = \"workhorse\"\nmodel_deep = \"strongest\"\n"
        )
    }

    /// Drive the pass until the account reading lands, which like the session
    /// one is a background thread running a subprocess.
    fn settle_account(app: &mut App, question: &str, want: bool) {
        for _ in 0..200 {
            app.poll_cap_probes();
            if app.account_caps.contains_key(question) == want {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the account reading never settled to {want}");
    }

    /// The headline case (DESIGN.md §8): the account says when its window
    /// reopens, as an instant, and that answers for the badge — including for
    /// a cap whose own wording named no time at all, which nothing else can
    /// time and no clock can judge.
    #[test]
    fn the_accounts_instant_times_a_cap_the_screen_left_untimed() {
        let (mut app, task_id, project_path) =
            cap_env_with(true, "Weekly limit reached", &cap_verb(""));
        settle_cap(&mut app, task_id, true);
        assert_eq!(app.caps[&task_id].reset_minutes, None);
        assert!(
            app.cap_window(task_id).expect("a window").due(),
            "an untimed cap is the operator's call until something times it"
        );

        let now = crate::session_probe::local_epoch().expect("a clock");
        app.now_epoch = Some(now);
        let question = app.cap_question(task_id).expect("a question");
        app.inject_account_cap(
            &question,
            Some(voro_core::AccountCap {
                reset_epoch: now + 3600,
                reset_label: Some("09:00".into()),
            }),
        );
        settle_account(&mut app, &question, true);

        let window = app.cap_window(task_id).expect("a window");
        assert_eq!(window.label.as_deref(), Some("09:00"));
        assert!(window.timed && !window.passed);
        assert!(!window.due(), "the window is known to be shut");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The account's instant decides over the session's own screen, which is
    /// the whole point of asking for it: `9:50pm` on screen is a bare clock
    /// time resolved by nearest occurrence, and an instant simply is one. Here
    /// the two disagree — the parse says the reset is still an hour off — and
    /// the sweep goes by the instant.
    #[test]
    fn the_sweep_goes_by_the_accounts_instant() {
        let (mut app, task_id, project_path) =
            cap_env_with(true, "Session limit reached - resets 9:50pm", &cap_verb(""));
        settle_cap(&mut app, task_id, true);
        // An hour short of the 21:50 the screen named: on the parse alone this
        // session is waiting, and the sweep would leave it alone.
        app.now_minutes = Some(20 * 60 + 50);
        assert!(!app.cap_window(task_id).expect("a window").due());

        let now = crate::session_probe::local_epoch().expect("a clock");
        app.now_epoch = Some(now);
        let question = app.cap_question(task_id).expect("a question");
        app.inject_account_cap(
            &question,
            Some(voro_core::AccountCap {
                reset_epoch: now - 60,
                reset_label: Some("21:50".into()),
            }),
        );
        settle_account(&mut app, &question, true);
        assert!(app.cap_window(task_id).expect("a window").passed);

        key(&mut app, KeyCode::Char('u'));
        assert_eq!(
            delivered(&project_path).as_deref().map(str::trim),
            Some(NUDGE),
            "the account's instant says the window is open"
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// What asking costs is the reason this reading is gated rather than
    /// scheduled: the verb spends an API call, so a fleet with nothing badged
    /// never runs it at all.
    #[test]
    fn a_healthy_fleet_never_asks_the_account() {
        let asked = std::env::temp_dir().join(format!("voro-cap-asked-{}", std::process::id()));
        let _ = std::fs::remove_file(&asked);
        let (mut app, _, project_path) = cap_env_with(
            true,
            "running the test suite",
            &format!("cap = \"printf 'x' >> '{}'\"\n", asked.display()),
        );
        for _ in 0..20 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.caps.is_empty(), "{:?}", app.caps);
        assert!(!asked.exists(), "an uncapped fleet asked anyway");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A retrying session buys no account reading. It is badged, but it is
    /// mid-turn rather than held, so there is nothing for an instant to answer
    /// — and this is the one probe where the difference is money rather than a
    /// subprocess.
    #[test]
    fn a_retrying_session_never_asks_the_account() {
        let asked = std::env::temp_dir().join(format!("voro-cap-retry-{}", std::process::id()));
        let _ = std::fs::remove_file(&asked);
        let (mut app, task_id, project_path) = cap_env_with(
            true,
            "Session limit reached - Retrying in 5m (9:50pm) - attempt 2/10",
            &format!(
                "cap = \"printf 'x' >> '{}'\"\nmodel = \"workhorse\"\n",
                asked.display()
            ),
        );
        settle_cap(&mut app, task_id, true);
        assert!(app.caps[&task_id].retrying);
        for _ in 0..20 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !asked.exists(),
            "a retrying session asked the account anyway"
        );
        assert!(app.account_caps.is_empty());
        assert!(
            !app.cap_window(task_id).expect("a window").due(),
            "and it is never swept"
        );

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// And once something *is* badged, the account is asked once — not once a
    /// tick, and not once per capped session. The reading it lands is the
    /// instant the agent reported.
    #[test]
    fn a_capped_fleet_asks_the_account_once() {
        let now = crate::session_probe::local_epoch().expect("a clock");
        let asked = std::env::temp_dir().join(format!("voro-cap-once-{}", std::process::id()));
        let _ = std::fs::remove_file(&asked);
        let (mut app, task_id, project_path) = cap_env_with(
            true,
            "Session limit reached",
            &format!(
                "cap = \"printf '%s' {{model}} >> '{}'; printf '%s' {}\"\nmodel = \"workhorse\"\n",
                asked.display(),
                now + 1800
            ),
        );
        settle_cap(&mut app, task_id, true);
        let question = app.cap_question(task_id).expect("a question");
        settle_account(&mut app, &question, true);
        for _ in 0..20 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        assert_eq!(app.account_caps[&question].reset_epoch, now + 1800);
        assert_eq!(
            std::fs::read_to_string(&asked).unwrap_or_default(),
            "workhorse",
            "the account was asked more than once, or on the wrong model"
        );
        let _ = std::fs::remove_file(&asked);

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// The question is asked on the model the session actually launched with,
    /// because a subscription meters each strong model separately: a deep
    /// task's session runs the deeper model, so the window that holds it is
    /// that model's window and not the workhorse's. A reading taken on the
    /// wrong model would answer about the wrong window — and answer *earlier*
    /// than the truth whenever the cheaper pool reopens first.
    #[test]
    fn the_question_names_the_model_its_session_ran() {
        let (mut app, task_id, project_path) =
            cap_env_with(true, "Session limit reached", &cap_verb(""));
        assert!(
            app.cap_question(task_id)
                .expect("a question")
                .contains("workhorse")
        );

        app.store.set_deep(task_id, true).unwrap();
        app.refresh().unwrap();
        let deep = app.cap_question(task_id).expect("a question");
        assert!(deep.contains("strongest"), "{deep}");
        assert!(!deep.contains("workhorse"), "{deep}");

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// An agent that cannot say — `codex` names no `cap`, and neither does the
    /// stub here — leaves the badge exactly as it was: the clock time off the
    /// session's own screen, judged by nearest occurrence.
    #[test]
    fn an_agent_without_the_cap_verb_still_badges_from_the_screen() {
        let (mut app, task_id, project_path) =
            cap_env(true, "Session limit reached - resets 9:50pm");
        settle_cap(&mut app, task_id, true);
        app.now_minutes = Some(20 * 60 + 50);
        for _ in 0..10 {
            app.poll_cap_probes();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.account_caps.is_empty());

        let window = app.cap_window(task_id).expect("a window");
        assert_eq!(window.label.as_deref(), Some("21:50"));
        assert!(window.timed && !window.passed);

        let _ = std::fs::remove_dir_all(project_path.parent().unwrap());
    }

    /// A review task with a tracked PR, selected in the cockpit — the only
    /// selection the stale-branch probe has anything to say about.
    fn app_with_review_pr() -> App {
        let mut app = app_with(&[TaskState::Review]);
        let id = app.selected_task_id().expect("the review task is selected");
        app.store
            .set_pr(id, Some("https://github.com/o/r/pull/1"))
            .unwrap();
        app.refresh().unwrap();
        app
    }

    /// Landing on a review task starts no probe on the tick it arrives
    /// (DESIGN.md §8): the selection has not rested yet, so scrolling past the
    /// row costs neither a `gh` call nor a thread.
    #[test]
    fn a_fresh_selection_starts_no_probe() {
        let mut app = app_with_review_pr();
        app.poll_conflict_probe();
        assert_eq!(app.probe.in_flight(), None);
        assert_eq!(app.conflict_selected, None);
    }

    /// A verdict landing while its task is still selected fills the marker.
    #[test]
    fn a_conflicting_verdict_marks_the_selected_task() {
        let mut app = app_with_review_pr();
        let id = app.selected_task_id().unwrap();
        app.probe
            .inject_result(id, voro_core::Mergeability::Conflicting);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, Some((id, true)));

        // A clean verdict is held too, so the row is not probed again.
        app.conflict_selected = None;
        app.probe
            .inject_result(id, voro_core::Mergeability::Mergeable);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, Some((id, false)));
    }

    /// A verdict that arrives after the selection has moved on is discarded,
    /// never shown against whatever is selected now.
    #[test]
    fn a_verdict_for_an_unselected_task_is_discarded() {
        let mut app = app_with_review_pr();
        let id = app.selected_task_id().unwrap();
        app.probe
            .inject_result(id + 1, voro_core::Mergeability::Conflicting);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, None);
        assert_eq!(app.probe.in_flight(), None);
    }

    /// A captured revision reaches the store whatever is selected by the time
    /// it lands (DESIGN.md §8) — the rejection that started it has already
    /// moved its task on to `running`.
    #[test]
    fn a_captured_revision_is_recorded_against_its_task() {
        let mut app = app_with(&[TaskState::Review, TaskState::Ready]);
        let id = app.selected_task_id().unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        app.capture.inject_result(id, Some(sha));
        app.move_selection(1);

        app.poll_reviewed_capture();
        assert_eq!(app.store.last_reviewed(id).unwrap(), Some(sha.to_string()));
    }

    /// Moving off the row drops its verdict, so nothing stale is rendered and
    /// re-selecting it probes afresh.
    #[test]
    fn moving_the_selection_drops_the_verdict() {
        let mut app = app_with(&[TaskState::Review, TaskState::Ready]);
        let id = app.selected_task_id().unwrap();
        app.conflict_selected = Some((id, true));
        app.move_selection(1);
        app.poll_conflict_probe();
        assert_eq!(app.conflict_selected, None);
    }
}
