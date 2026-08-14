//! The TUI's off-loop machinery (DESIGN.md §8): a call that has to wait on
//! something — the network, or a subprocess replaying a session — is never
//! taken on the render path, and never on a keypress whose answer only feeds a
//! later read. Each runner here spawns a background thread and hands its answer
//! back over a channel the event loop drains each tick.
//!
//! [`ConflictProbe`] is the stale-review-branch probe, behind the *selection*:
//! [`probe_due`] is the whole decision — pure, so the debounce and the
//! at-most-one-in-flight rule are testable without a terminal or a network.
//! [`ReviewedCapture`] is the reviewed-revision capture, behind a *keypress*:
//! there is nothing to debounce, so every start runs, and every answer is
//! recorded against the task it was captured for rather than against whatever
//! is selected when it lands. [`PrCreate`] is the create-PR push-and-open,
//! behind a keypress too, and the one runner whose answer the operator is
//! visibly waiting on: it lands a URL rather than a fact, and the loop opens
//! the browser on it. [`CapProbe`] is the usage-cap reading, behind neither:
//! every in-flight session is a target on every tick, so it is the one runner
//! debounced against the *clock* ([`CAP_INTERVAL`]) rather than against
//! anything the operator does. [`AccountCapProbe`] is its account-wide
//! counterpart, and the only runner whose debounce guards *money* rather than
//! latency: the verb behind it spends an API call, so it is gated on a badge
//! already being on the strip and asks once per capped episode.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use voro_core::{AccountCap, CapReading, Mergeability};

use crate::pr::{PrCreateInput, ReviewedSource};

/// How long the selection must rest on a row before its probe starts. Scrolling
/// through the queue moves faster than this, so rows passed over never spawn a
/// probe; the event loop's own 500ms wake is enough to notice the rest expiring,
/// so no timer machinery is needed.
pub const SETTLE: Duration = Duration::from_millis(400);

/// What the event loop knows when deciding whether to start a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeInputs {
    /// The selected task, when it is a `review` task with a tracked PR — `None`
    /// for any other selection, which has nothing to probe.
    pub target: Option<i64>,
    /// The task the in-memory verdict is currently held for, if any.
    pub cached: Option<i64>,
    /// The task a background probe is already running for, if any.
    pub in_flight: Option<i64>,
    /// How long the selection has rested on its current row.
    pub rested: Duration,
}

/// Whether a probe should start this tick (DESIGN.md §8): a probeable selection
/// that has no verdict yet, has rested for [`SETTLE`], and has no probe already
/// in flight — at most one `gh` call is outstanding at a time, so a queue
/// scrolled through quickly costs nothing.
pub fn probe_due(inputs: ProbeInputs) -> bool {
    let Some(target) = inputs.target else {
        return false;
    };
    if inputs.cached == Some(target) || inputs.in_flight.is_some() {
        return false;
    }
    inputs.rested >= SETTLE
}

/// The untested shell around [`probe_due`]: the thread spawn, the channel, and
/// the rest timestamp the debounce is measured against. Owns no store handle —
/// the PR URL is resolved on the event loop and moved into the thread, so
/// nothing but a `String` crosses it.
pub struct ConflictProbe {
    tx: Sender<(i64, Mergeability)>,
    rx: Receiver<(i64, Mergeability)>,
    /// The row the selection currently rests on, and when it landed there.
    resting: Option<(i64, Instant)>,
    in_flight: Option<i64>,
}

impl Default for ConflictProbe {
    fn default() -> Self {
        let (tx, rx) = channel();
        ConflictProbe {
            tx,
            rx,
            resting: None,
            in_flight: None,
        }
    }
}

impl ConflictProbe {
    /// Note where the selection is and return how long it has rested there. A
    /// selection that moved restarts the clock, which is what keeps a held
    /// `j`/`k` from ever reaching [`SETTLE`].
    pub fn settle(&mut self, selected: Option<i64>, now: Instant) -> Duration {
        match self.resting {
            Some((id, since)) if Some(id) == selected => now.saturating_duration_since(since),
            _ => {
                self.resting = selected.map(|id| (id, now));
                Duration::ZERO
            }
        }
    }

    pub fn in_flight(&self) -> Option<i64> {
        self.in_flight
    }

    /// Collect a finished probe's verdict, if one has landed. Never blocks.
    pub fn take_result(&mut self) -> Option<(i64, Mergeability)> {
        match self.rx.try_recv() {
            Ok(result) => {
                self.in_flight = None;
                Some(result)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Hand back a verdict as though a background probe had produced it, so the
    /// event loop's collect-and-discard half can be tested without `gh`.
    #[cfg(test)]
    pub fn inject_result(&mut self, task_id: i64, verdict: Mergeability) {
        self.in_flight = Some(task_id);
        let _ = self.tx.send((task_id, verdict));
    }

    /// Ask GitHub about `url` on a background thread, tagging the answer with
    /// the task it was asked for so the event loop can discard a verdict whose
    /// selection has moved on.
    pub fn start(&mut self, task_id: i64, url: String) {
        let tx = self.tx.clone();
        self.in_flight = Some(task_id);
        std::thread::spawn(move || {
            let verdict = crate::pr::conflict_status_url(&url);
            let _ = tx.send((task_id, verdict));
        });
    }
}

/// The reviewed-revision capture's off-loop runner (DESIGN.md §8):
/// [`crate::pr::resolve_reviewed`] on a background thread, with the answer sent
/// back for the event loop to record. Started by a rejection rather than by the
/// selection, which is what it does differently from [`ConflictProbe`] — there
/// is no settle interval, because a keypress is already the operator's
/// deliberate act and there is nothing to debounce; captures for several tasks
/// may be in flight at once; and nothing that lands is ever discarded, because
/// a captured revision belongs to its task whatever is on screen.
pub struct ReviewedCapture {
    tx: Sender<(i64, Option<String>)>,
    rx: Receiver<(i64, Option<String>)>,
    /// The tasks a capture is running for, so a repeated reject key cannot
    /// spawn threads without bound.
    in_flight: HashSet<i64>,
}

impl Default for ReviewedCapture {
    fn default() -> Self {
        let (tx, rx) = channel();
        ReviewedCapture {
            tx,
            rx,
            in_flight: HashSet::new(),
        }
    }
}

impl ReviewedCapture {
    /// Resolve `source` on a background thread. Returns whether a thread was
    /// started — `false` when this task already has a capture in flight, which
    /// a held reject key would otherwise multiply.
    pub fn start(&mut self, task_id: i64, source: ReviewedSource) -> bool {
        if !self.in_flight.insert(task_id) {
            return false;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send((task_id, crate::pr::resolve_reviewed(&source)));
        });
        true
    }

    /// Every revision that has landed since the last drain. Never blocks, and
    /// drops nothing: unlike a conflict verdict, a captured revision is recorded
    /// against the task it was captured for whatever the selection has moved on
    /// to. A capture that resolved nothing still clears its task's guard, so a
    /// later rejection of that task tries again.
    pub fn take_results(&mut self) -> Vec<(i64, String)> {
        let mut landed = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok((task_id, sha)) => {
                    self.in_flight.remove(&task_id);
                    if let Some(sha) = sha {
                        landed.push((task_id, sha));
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return landed,
            }
        }
    }

    /// Hand back a revision as though a background capture had produced it, so
    /// the event loop's drain-and-record half can be tested without `gh`.
    #[cfg(test)]
    pub fn inject_result(&mut self, task_id: i64, sha: Option<&str>) {
        self.in_flight.insert(task_id);
        let _ = self.tx.send((task_id, sha.map(str::to_string)));
    }
}

/// The create-PR runner (DESIGN.md §8): [`crate::pr::run_create`] — a `git
/// push` and a `gh pr create`, seconds of network between them — on a
/// background thread, with the URL or the failure sent back for the event loop
/// to record, open, and report.
///
/// Started by the confirmation modal's Enter rather than by the selection, so
/// like [`ReviewedCapture`] it has no settle interval and nothing to debounce.
/// It differs from that capture in what it guards and what it hands back: a
/// second create for a task already creating one would open a second pull
/// request, so [`Self::in_flight`] is asked before the modal opens as well as
/// before a job starts; and a failure is as much of an answer as a URL, because
/// this is the one runner whose result the operator pressed a key to see.
/// Nothing that lands is discarded — a created PR is a fact about the branch,
/// so it is recorded against its own task whatever the queue has moved on to.
pub struct PrCreate {
    tx: Sender<(i64, Result<String, String>)>,
    rx: Receiver<(i64, Result<String, String>)>,
    /// The tasks a create is running for, so neither a repeated `g` nor a
    /// repeated Enter can open a second pull request for one branch.
    in_flight: HashSet<i64>,
}

impl Default for PrCreate {
    fn default() -> Self {
        let (tx, rx) = channel();
        PrCreate {
            tx,
            rx,
            in_flight: HashSet::new(),
        }
    }
}

impl PrCreate {
    /// Whether a create is already running for this task.
    pub fn in_flight(&self, task_id: i64) -> bool {
        self.in_flight.contains(&task_id)
    }

    /// Push and open the PR `input` describes on a background thread. Returns
    /// whether a thread was started — `false` when this task already has a
    /// create in flight.
    pub fn start(&mut self, task_id: i64, input: PrCreateInput, local_diff: &str) -> bool {
        if !self.in_flight.insert(task_id) {
            return false;
        }
        let tx = self.tx.clone();
        let local_diff = local_diff.to_string();
        std::thread::spawn(move || {
            let _ = tx.send((task_id, crate::pr::run_create(&input, &local_diff)));
        });
        true
    }

    /// Every create that has finished since the last drain. Never blocks, and
    /// drops nothing: the URL has to be recorded and the browser opened on it
    /// whatever the operator is looking at, and a failure has to be said.
    pub fn take_results(&mut self) -> Vec<(i64, Result<String, String>)> {
        let mut landed = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok((task_id, result)) => {
                    self.in_flight.remove(&task_id);
                    landed.push((task_id, result));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return landed,
            }
        }
    }

    /// Hand back a create's outcome as though a background thread had produced
    /// it, so the event loop's drain-record-and-open half can be tested without
    /// `gh`.
    #[cfg(test)]
    pub fn inject_result(&mut self, task_id: i64, result: Result<String, String>) {
        self.in_flight.insert(task_id);
        let _ = self.tx.send((task_id, result));
    }
}

/// How long a session's cap reading stands before it is taken again. A cap
/// window is hours long and the operator's own continuation is the only thing
/// that clears one early, so a minute is fine-grained enough for both edges of
/// the badge — and coarse enough that the probes stay rare, which matters:
/// each one is a subprocess replaying a session's screen, and there is one per
/// dispatch in flight.
pub const CAP_INTERVAL: Duration = Duration::from_secs(60);

/// Whether a session's cap reading should be taken this tick: none already in
/// flight for it, and either never read or read longer ago than
/// [`CAP_INTERVAL`]. Pure, so the debounce is testable without a subprocess.
pub fn cap_probe_due(in_flight: bool, last_started: Option<Instant>, now: Instant) -> bool {
    if in_flight {
        return false;
    }
    last_started.is_none_or(|at| now.saturating_duration_since(at) >= CAP_INTERVAL)
}

/// The usage-cap probe's off-loop runner (DESIGN.md §8): an agent's `logs` verb
/// on a background thread, reading whether the session is sitting on a cap.
///
/// It exists as a runner at all — rather than as one more thing the reconcile
/// pass computes — because the verb is *slow*: replaying a background session's
/// screen costs the better part of a second, and reconciliation happens inside
/// `App::refresh`, on the render path, where §8 permits no blocking at all. So
/// it takes [`ReviewedCapture`]'s shape: several in flight at once, one per
/// dispatch, each answer belonging to the task it was asked for.
///
/// Unlike the other two runners it also *debounces across time* rather than
/// across a selection, because every in-flight session is a target on every
/// tick and nothing the operator does narrows that down.
pub struct CapProbe {
    tx: Sender<(i64, Option<CapReading>)>,
    rx: Receiver<(i64, Option<CapReading>)>,
    in_flight: HashSet<i64>,
    /// When each task's reading was last taken, which is what [`CAP_INTERVAL`]
    /// is measured from.
    last_started: HashMap<i64, Instant>,
}

impl Default for CapProbe {
    fn default() -> Self {
        let (tx, rx) = channel();
        CapProbe {
            tx,
            rx,
            in_flight: HashSet::new(),
            last_started: HashMap::new(),
        }
    }
}

impl CapProbe {
    pub fn due(&self, task_id: i64, now: Instant) -> bool {
        cap_probe_due(
            self.in_flight.contains(&task_id),
            self.last_started.get(&task_id).copied(),
            now,
        )
    }

    /// Read `session_ref`'s output through `logs_cmd` on a background thread.
    pub fn start(&mut self, task_id: i64, session_ref: String, logs_cmd: String, now: Instant) {
        self.in_flight.insert(task_id);
        self.last_started.insert(task_id, now);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let reading = crate::session_probe::read_session_cap(&logs_cmd, &session_ref);
            let _ = tx.send((task_id, reading));
        });
    }

    /// Every reading that has landed since the last drain. Never blocks. A
    /// `None` is as meaningful as a reading — it is how a badge clears once the
    /// operator continues the session — so both are handed back.
    pub fn take_results(&mut self) -> Vec<(i64, Option<CapReading>)> {
        let mut landed = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok((task_id, reading)) => {
                    self.in_flight.remove(&task_id);
                    landed.push((task_id, reading));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return landed,
            }
        }
    }

    /// Drop the debounce for tasks that are no longer in flight, so a
    /// redispatch reads afresh rather than waiting out a dead session's
    /// interval, and the map cannot grow for the life of the process.
    pub fn retain(&mut self, live: &HashSet<i64>) {
        self.last_started.retain(|id, _| live.contains(id));
    }

    /// Hand back a reading as though a background probe had produced it, so the
    /// drain-and-render half can be tested without an agent.
    #[cfg(test)]
    pub fn inject_result(&mut self, task_id: i64, reading: Option<CapReading>) {
        self.in_flight.insert(task_id);
        let _ = self.tx.send((task_id, reading));
    }
}

/// The floor between two readings of one agent's account, and the only thing
/// standing between a stuck badge and a standing charge: unlike every other
/// probe here, this one spends an API call to ask (DESIGN.md §8). It is a
/// backstop rather than the schedule — [`account_probe_due`] normally asks once
/// per capped episode — so it is set where a pathological loop would cost cents
/// an hour rather than pounds.
pub const ACCOUNT_CAP_INTERVAL: Duration = Duration::from_secs(600);

/// Whether an account reading should be taken this tick (DESIGN.md §8).
///
/// The rule is "ask once, while it matters": a session this question answers
/// for is badged capped, nothing is in flight for it, and either nothing has
/// been read yet or what was read has expired — a window that has reopened
/// while a session is still badged is the one case where asking again can learn
/// something, since the account may have entered a new one. A reading that came
/// back *empty* is held too, and that is what stops a session whose badge has
/// gone stale from buying a fresh call every interval for as long as it sits
/// there.
pub fn account_probe_due(
    in_flight: bool,
    last: Option<(Instant, Option<i64>)>,
    now: Instant,
    now_epoch: Option<i64>,
) -> bool {
    if in_flight {
        return false;
    }
    let Some((started, reset_epoch)) = last else {
        return true;
    };
    if now.saturating_duration_since(started) < ACCOUNT_CAP_INTERVAL {
        return false;
    }
    match (reset_epoch, now_epoch) {
        (Some(reset), Some(now)) => reset <= now,
        _ => false,
    }
}

/// The account-cap probe's off-loop runner (DESIGN.md §8): an agent's `cap`
/// verb on a background thread, reading the instant the window holding a
/// session reopens.
///
/// It is [`CapProbe`]'s counterpart and differs from it in both directions of
/// what it costs. It is keyed by the *question* — the rendered `cap` command —
/// rather than by task, because a cap belongs to the account and one reading
/// answers for every session that would ask the same thing, where a screen
/// replay has to be taken per session. That key is what makes the per-model
/// case fall out without being special: an agent asking on `{model}` renders a
/// different command per model in flight and gets a reading each, one that
/// names no model renders one command and is asked once. And it is debounced
/// far harder, because the verb spends an API call rather than a subprocess:
/// [`account_probe_due`] asks once per capped episode, and
/// [`ACCOUNT_CAP_INTERVAL`] catches anything that would ask in a loop.
pub struct AccountCapProbe {
    tx: Sender<(String, Option<AccountCap>)>,
    rx: Receiver<(String, Option<AccountCap>)>,
    in_flight: HashSet<String>,
    /// Each question's last reading: when it was asked, and the instant it came
    /// back with — `None` for a reading that found the account uncapped, which
    /// is held exactly as a positive one is.
    last: HashMap<String, (Instant, Option<i64>)>,
}

impl Default for AccountCapProbe {
    fn default() -> Self {
        let (tx, rx) = channel();
        AccountCapProbe {
            tx,
            rx,
            in_flight: HashSet::new(),
            last: HashMap::new(),
        }
    }
}

impl AccountCapProbe {
    pub fn due(&self, question: &str, now: Instant, now_epoch: Option<i64>) -> bool {
        account_probe_due(
            self.in_flight.contains(question),
            self.last.get(question).copied(),
            now,
            now_epoch,
        )
    }

    /// Ask one rendered `cap` command what the account says, on a background
    /// thread.
    pub fn start(&mut self, question: String, now: Instant, now_epoch: i64) {
        self.in_flight.insert(question.clone());
        // Held from the start, not from the answer, so a verb slower than the
        // interval cannot be started twice over.
        self.last.insert(question.clone(), (now, None));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let reading = crate::session_probe::read_account_cap(&question, now_epoch);
            let _ = tx.send((question, reading));
        });
    }

    /// Every reading that has landed since the last drain, each tagged with the
    /// question it answers. Never blocks. An empty reading is handed back as
    /// meaningfully as a full one: it clears that question's answer, leaving the
    /// badge to the session's own screen again.
    pub fn take_results(&mut self) -> Vec<(String, Option<AccountCap>)> {
        let mut landed = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok((question, reading)) => {
                    self.in_flight.remove(&question);
                    if let Some(entry) = self.last.get_mut(&question) {
                        entry.1 = reading.as_ref().map(|r| r.reset_epoch);
                    }
                    landed.push((question, reading));
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return landed,
            }
        }
    }

    /// Drop the debounce for questions no badged session is asking any more, so
    /// the next cap reads afresh rather than waiting out the last one's
    /// interval.
    pub fn retain(&mut self, asked_for: &HashSet<String>) {
        self.last.retain(|question, _| asked_for.contains(question));
    }

    /// Hand back a reading as though a background probe had produced it, so the
    /// drain-and-render half can be tested without spending an API call.
    #[cfg(test)]
    pub fn inject_result(&mut self, question: &str, reading: Option<AccountCap>) {
        self.in_flight.insert(question.to_string());
        self.last
            .entry(question.to_string())
            .or_insert((Instant::now(), None));
        let _ = self.tx.send((question.to_string(), reading));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs() -> ProbeInputs {
        ProbeInputs {
            target: Some(1),
            cached: None,
            in_flight: None,
            rested: SETTLE,
        }
    }

    #[test]
    fn probes_a_rested_unprobed_selection() {
        assert!(probe_due(inputs()));
    }

    #[test]
    fn no_probe_without_a_probeable_target() {
        assert!(!probe_due(ProbeInputs {
            target: None,
            ..inputs()
        }));
    }

    #[test]
    fn no_probe_before_the_selection_settles() {
        assert!(!probe_due(ProbeInputs {
            rested: SETTLE - Duration::from_millis(1),
            ..inputs()
        }));
        assert!(!probe_due(ProbeInputs {
            rested: Duration::ZERO,
            ..inputs()
        }));
    }

    #[test]
    fn no_probe_when_the_verdict_is_already_held() {
        assert!(!probe_due(ProbeInputs {
            cached: Some(1),
            ..inputs()
        }));
    }

    #[test]
    fn a_verdict_for_another_task_does_not_count_as_cached() {
        assert!(probe_due(ProbeInputs {
            cached: Some(2),
            ..inputs()
        }));
    }

    #[test]
    fn at_most_one_probe_in_flight() {
        assert!(!probe_due(ProbeInputs {
            in_flight: Some(1),
            ..inputs()
        }));
        assert!(!probe_due(ProbeInputs {
            in_flight: Some(2),
            ..inputs()
        }));
    }

    #[test]
    fn resting_accumulates_while_the_selection_holds() {
        let start = Instant::now();
        let mut probe = ConflictProbe::default();
        assert_eq!(probe.settle(Some(7), start), Duration::ZERO);
        assert_eq!(probe.settle(Some(7), start + SETTLE), SETTLE);
    }

    #[test]
    fn moving_the_selection_restarts_the_clock() {
        let start = Instant::now();
        let mut probe = ConflictProbe::default();
        probe.settle(Some(7), start);
        assert_eq!(probe.settle(Some(8), start + SETTLE), Duration::ZERO);
        assert_eq!(probe.settle(Some(8), start + SETTLE), Duration::ZERO);
        assert_eq!(probe.settle(Some(8), start + SETTLE * 2), SETTLE);
    }

    #[test]
    fn scrolling_never_reaches_the_settle_interval() {
        // A held `j` steps far faster than the settle interval, so no row
        // passed over is ever due for a probe.
        let step = Duration::from_millis(30);
        let mut probe = ConflictProbe::default();
        let mut now = Instant::now();
        for id in 1..20 {
            now += step;
            let rested = probe.settle(Some(id), now);
            assert!(!probe_due(ProbeInputs {
                target: Some(id),
                cached: None,
                in_flight: None,
                rested,
            }));
        }
        // Resting on the last row for the settle interval is then due once.
        now += SETTLE;
        let rested = probe.settle(Some(19), now);
        assert!(probe_due(ProbeInputs {
            target: Some(19),
            cached: None,
            in_flight: None,
            rested,
        }));
    }

    #[test]
    fn an_empty_selection_clears_the_rest() {
        let start = Instant::now();
        let mut probe = ConflictProbe::default();
        probe.settle(Some(7), start);
        assert_eq!(probe.settle(None, start + SETTLE), Duration::ZERO);
        assert_eq!(probe.settle(Some(7), start + SETTLE), Duration::ZERO);
    }

    #[test]
    fn no_result_before_a_probe_runs() {
        let mut probe = ConflictProbe::default();
        assert_eq!(probe.take_result(), None);
        assert_eq!(probe.in_flight(), None);
    }

    #[test]
    fn no_revision_before_a_capture_runs() {
        let mut capture = ReviewedCapture::default();
        assert!(capture.take_results().is_empty());
    }

    /// A capture with nothing to read starts anyway — the runner does not know
    /// what a source will resolve to — and lands as no revision.
    #[test]
    fn a_capture_that_resolves_nothing_yields_no_revision() {
        let mut capture = ReviewedCapture::default();
        assert!(capture.start(7, ReviewedSource::empty()));
        // The thread may not have finished, so drain until it has: what is
        // asserted is that a resolved-to-nothing capture never yields a row.
        for _ in 0..100 {
            assert!(capture.take_results().is_empty());
            if !capture.in_flight.contains(&7) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the capture never landed");
    }

    /// Every revision that has landed is handed back, for as many tasks as have
    /// captures in flight — a rejection's revision belongs to its own task, so
    /// none of them may be dropped.
    #[test]
    fn draining_hands_back_every_landed_revision() {
        let mut capture = ReviewedCapture::default();
        capture.inject_result(7, Some("aaaa1111"));
        capture.inject_result(8, None);
        capture.inject_result(9, Some("bbbb2222"));
        assert_eq!(
            capture.take_results(),
            vec![(7, "aaaa1111".to_string()), (9, "bbbb2222".to_string())]
        );
        assert!(capture.take_results().is_empty());
    }

    #[test]
    fn no_pull_request_before_a_create_runs() {
        let mut creates = PrCreate::default();
        assert!(creates.take_results().is_empty());
        assert!(!creates.in_flight(7));
    }

    /// Both outcomes are handed back, for as many tasks as have creates in
    /// flight: a URL has to be recorded and opened, and a failure is the answer
    /// the operator pressed a key to see.
    #[test]
    fn draining_hands_back_every_finished_create() {
        let mut creates = PrCreate::default();
        creates.inject_result(7, Ok("https://github.com/o/r/pull/1".into()));
        creates.inject_result(8, Err("`git push origin feat` failed".into()));
        assert_eq!(
            creates.take_results(),
            vec![
                (7, Ok("https://github.com/o/r/pull/1".to_string())),
                (8, Err("`git push origin feat` failed".to_string())),
            ]
        );
        assert!(creates.take_results().is_empty());
        assert!(!creates.in_flight(7));
    }

    /// One branch, one pull request: a second confirmation while a create is
    /// running starts nothing, and only once the first has landed can the task
    /// be tried again. Another task is unaffected — creates are per task.
    #[test]
    fn at_most_one_create_in_flight_per_task() {
        let mut creates = PrCreate::default();
        creates.inject_result(7, Ok("https://github.com/o/r/pull/1".into()));
        assert!(creates.in_flight(7));
        assert!(!creates.start(7, PrCreateInput::unrunnable(), "`o`"));

        assert_eq!(
            creates.take_results(),
            vec![(7, Ok("https://github.com/o/r/pull/1".to_string()))]
        );
        assert!(!creates.in_flight(7));
        // A create for another task is unaffected: this one starts, and fails
        // on its unrunnable checkout without pushing anything.
        assert!(creates.start(8, PrCreateInput::unrunnable(), "`o`"));
    }

    /// A session never read is due at once, so a capped dispatch badges on the
    /// first tick rather than after an interval's wait.
    #[test]
    fn an_unread_session_is_due_immediately() {
        assert!(cap_probe_due(false, None, Instant::now()));
    }

    /// The debounce proper: one reading per session per interval, however many
    /// ticks pass. Each probe replays a session's screen, so an undebounced
    /// sweep would spawn one subprocess per dispatch per tick.
    #[test]
    fn a_session_is_reread_only_once_an_interval_has_passed() {
        let start = Instant::now();
        assert!(!cap_probe_due(false, Some(start), start));
        assert!(!cap_probe_due(
            false,
            Some(start),
            start + CAP_INTERVAL - Duration::from_millis(1)
        ));
        assert!(cap_probe_due(false, Some(start), start + CAP_INTERVAL));
    }

    /// A probe already running is never doubled, even once its interval is up —
    /// a `logs` verb slower than the interval would otherwise pile threads up.
    #[test]
    fn a_probe_in_flight_is_never_doubled() {
        let start = Instant::now();
        assert!(!cap_probe_due(true, None, start));
        assert!(!cap_probe_due(true, Some(start), start + CAP_INTERVAL * 10));
    }

    /// Both kinds of answer are handed back: a reading badges its task, and an
    /// empty one clears it. Dropping the empties is what would leave a badge
    /// standing on a session the operator had already continued.
    #[test]
    fn draining_hands_back_readings_and_their_absence() {
        let mut probe = CapProbe::default();
        let capped = CapReading {
            reset_minutes: Some(1310),
        };
        probe.inject_result(7, Some(capped));
        probe.inject_result(8, None);
        assert_eq!(probe.take_results(), vec![(7, Some(capped)), (8, None)]);
        assert!(probe.take_results().is_empty());
    }

    /// A task that leaves the strip drops its debounce, so the same task
    /// redispatched is read at once rather than inheriting the dead session's
    /// interval.
    #[test]
    fn leaving_the_strip_drops_the_debounce() {
        let start = Instant::now();
        let mut probe = CapProbe::default();
        probe.start(7, "ref-7".into(), "true".into(), start);
        // Wait the probe out rather than racing it: what is asserted is the
        // debounce that outlives it, not how quickly the thread lands.
        for _ in 0..100 {
            if !probe.take_results().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !probe.due(7, start),
            "the reading is still within its interval"
        );

        probe.retain(&HashSet::from([9]));
        assert!(probe.due(7, start));
    }

    /// An agent whose account has never been asked is asked at once, so the
    /// first badged cap gets its instant on the tick it appears rather than
    /// after an interval's wait.
    #[test]
    fn an_unasked_account_is_due_immediately() {
        assert!(account_probe_due(false, None, Instant::now(), Some(1)));
    }

    /// The debounce proper, and the one that guards money rather than
    /// subprocesses: a reading stands until the window it named has passed, and
    /// never within the interval whatever it named.
    #[test]
    fn an_account_is_reasked_only_once_its_window_has_passed() {
        let start = Instant::now();
        let now_epoch = 1_786_722_000;
        let ahead = Some((start, Some(now_epoch + 3600)));
        let behind = Some((start, Some(now_epoch - 1)));
        // Inside the interval nothing is asked, however stale the reading.
        assert!(!account_probe_due(false, behind, start, Some(now_epoch)));
        // Past it, a window still shut is left alone and a reopened one is
        // asked again — the account may have entered a fresh window.
        let later = start + ACCOUNT_CAP_INTERVAL;
        assert!(!account_probe_due(false, ahead, later, Some(now_epoch)));
        assert!(account_probe_due(false, behind, later, Some(now_epoch)));
    }

    /// A reading that found the account uncapped is held exactly as a positive
    /// one is. Dropping it is what would buy a fresh API call every interval
    /// for a session capped on a limit the probe never hits.
    #[test]
    fn an_empty_reading_is_not_reasked() {
        let start = Instant::now();
        let empty = Some((start, None));
        assert!(!account_probe_due(false, empty, start, Some(1)));
        assert!(!account_probe_due(
            false,
            empty,
            start + ACCOUNT_CAP_INTERVAL * 10,
            Some(1)
        ));
    }

    /// A probe already running is never doubled, whatever the clock says.
    #[test]
    fn an_account_probe_in_flight_is_never_doubled() {
        let start = Instant::now();
        assert!(!account_probe_due(true, None, start, Some(1)));
        assert!(!account_probe_due(
            true,
            Some((start, Some(0))),
            start + ACCOUNT_CAP_INTERVAL,
            Some(1)
        ));
    }

    /// Readings land against the agent they were asked of, and an empty one
    /// clears that agent's answer rather than leaving the last standing.
    #[test]
    fn draining_hands_back_each_agents_reading() {
        let mut probe = AccountCapProbe::default();
        let cap = AccountCap {
            reset_epoch: 1_786_758_000,
            reset_label: Some("02:40".into()),
        };
        probe.inject_result("claude", Some(cap.clone()));
        probe.inject_result("codex", None);
        assert_eq!(
            probe.take_results(),
            vec![
                ("claude".to_string(), Some(cap)),
                ("codex".to_string(), None)
            ]
        );
        assert!(probe.take_results().is_empty());
    }

    /// An agent with nothing capped on the strip drops its debounce, so the
    /// next cap is read at once rather than inheriting the last one's interval.
    #[test]
    fn an_uncapped_agent_drops_its_debounce() {
        let now = Instant::now();
        let mut probe = AccountCapProbe::default();
        probe.inject_result("claude", None);
        probe.take_results();
        assert!(!probe.due("claude", now, Some(1)));

        probe.retain(&HashSet::from(["codex".to_string()]));
        assert!(probe.due("claude", now, Some(1)));
    }

    /// Holding the reject key on one task spawns one capture, not one per
    /// press; the next press after its answer is drained captures afresh.
    #[test]
    fn at_most_one_capture_in_flight_per_task() {
        let mut capture = ReviewedCapture::default();
        capture.inject_result(7, Some("aaaa1111"));
        assert!(!capture.start(7, ReviewedSource::empty()));
        // Another task is unaffected: captures are per task, not global.
        assert!(capture.start(8, ReviewedSource::empty()));

        assert_eq!(capture.take_results(), vec![(7, "aaaa1111".to_string())]);
        assert!(capture.start(7, ReviewedSource::empty()));
    }
}
