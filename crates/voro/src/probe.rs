//! The stale-review-branch probe's off-loop machinery (DESIGN.md §8). The
//! verdict costs a `gh` round-trip, so it is never taken on the render path:
//! [`ConflictProbe`] runs [`crate::pr::conflict_status_url`] on a background
//! thread and hands the answer back over a channel the event loop drains each
//! tick. [`probe_due`] is the whole decision — pure, so the debounce and the
//! at-most-one-in-flight rule are testable without a terminal or a network.

use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::time::{Duration, Instant};

use voro_core::Mergeability;

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
}
