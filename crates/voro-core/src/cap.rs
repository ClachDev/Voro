//! Reading a usage cap out of an agent session's own output (DESIGN.md §8).
//!
//! Two callers want the same answer from the same text. The reconciler asks it
//! of a session that has *died*, to record `capped` rather than `failed`; and it
//! asks it of a session that is still *alive*, because a cap does not kill a
//! supervisor-owned launch — the session sits waiting for the window to reset,
//! and without this it rides the running strip looking healthy for hours.
//!
//! Everything here is pure: a string in, a reading out. The process that
//! produces the string — an agent's `logs` verb, or the launch log for an agent
//! that defines none — lives in the `voro` crate, so this side stays testable
//! without a terminal or a subprocess.
//!
//! The text is a *terminal capture*, not a log: the built-in `claude` spelling
//! replays a background session's screen, escape sequences and all, so
//! [`strip_ansi`] runs before any matching. Cursor movement is spatial and
//! becomes a space; colour is not and vanishes, which is what stops a style
//! change mid-phrase from splitting the phrase it is styling.
//!
//! Matching stays deliberately narrow, as it always has: a missed cap reads as
//! an ordinary session, which is the failure everyone already lives with, while
//! a false one would badge healthy work as stuck and teach the operator to
//! disbelieve the badge. That asymmetry is why [`NOT_CAP_QUALIFIERS`] exists —
//! the agent says "approaching" and "not your usage limit" in text that
//! otherwise matches, and both mean the session is working fine.

/// Phrases that mean "held at a usage cap", checked case-insensitively.
///
/// The first three are the original list and cover agents that word it
/// generically. The rest are what Claude Code actually renders, which none of
/// the first three catch: a five-hour cap says "Session limit reached", the
/// weekly one "Weekly limit reached", and the per-model and overage ones name
/// the model or the credit. A cap that says none of these is reported as an
/// ordinary failure, which is the same landing either way (§8).
pub const CAP_SIGNATURES: [&str; 8] = [
    "usage limit",
    "rate limit",
    "quota exceeded",
    "session limit",
    "weekly limit",
    "opus limit",
    "sonnet limit",
    "credit limit",
];

/// Phrases that take a matched signature back. Each is something an agent says
/// in the same breath as a limit while still working normally: a
/// warning ahead of the cap ("Approaching usage limit", "You've used 80% of
/// your session limit") or a server-side error explicitly disclaiming one
/// ("Server is temporarily limiting requests (not your usage limit)").
const NOT_CAP_QUALIFIERS: [&str; 4] = ["approaching", "% of your", "not your", "close to your"];

/// Phrases that mean the signature after them is not a *report* at all. Every
/// real limit message ends with the upgrade prompt — `/upgrade to increase your
/// usage limit.` — which contains a signature of its own and, being last, would
/// otherwise be the one that decides.
///
/// That matters twice over. It is the *only* signature in a genuine cap whose
/// window holds no reset time, so letting it decide drops the time from every
/// real cap; and it says nothing about whether the session is held, so a warning
/// that ever trailed the same prompt would badge as a cap. Both go away once the
/// prompt is read as the boilerplate it is: skipped when choosing which
/// signature speaks, rather than negating like a qualifier — a qualifier means
/// "this one is not a cap", and skipping instead would let a genuine earlier cap
/// speak past a warning that had since replaced it.
const MENTION_PREFIXES: [&str; 2] = ["/upgrade", "increase your"];

/// How much text after a matched signature is read for the reset time that
/// goes on the badge.
const WINDOW: usize = 200;

/// How much text before a clock time is read for the date that clock belongs
/// to. Enough for `september 17, ` and no more: anything further back is not
/// this clock's date.
const DATE_WINDOW: usize = 16;

/// Month names, in order, matched by any prefix of at least three letters —
/// `aug`, `sept` and `august` all name the same month, and no shorter word can
/// name one by accident.
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// The span of the synthetic year [`CapDate::ordinal`] lays dates out in.
const SYNTHETIC_YEAR: i32 = 12 * 31;

/// How much text before a matched signature is read for the qualifiers that
/// take it back. Deliberately short: every qualifier attaches directly to the
/// phrase it modifies, so a wider look-back would let an *earlier* warning
/// speak for a later, genuine cap — which is the one reading that loses a real
/// cap rather than merely missing an unworded one.
const QUALIFIER_WINDOW: usize = 32;

/// A month and day as an agent writes them, for the reset times that carry one.
///
/// Only the *side* one of these falls on relative to another is ever read,
/// which is what lets the comparison do without a calendar dependency (see
/// [`CapDate::days_from`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapDate {
    pub month: u8,
    pub day: u8,
}

impl CapDate {
    /// A day number in a synthetic year of twelve 31-day months.
    ///
    /// Non-contiguous across short months and blind to leap years, both of
    /// which are irrelevant here: nothing reads the magnitude of a difference
    /// between two of these, only its sign, and no pair of dates a cap window
    /// spans comes anywhere near the half-year where that could flip.
    fn ordinal(self) -> i32 {
        (i32::from(self.month) - 1) * 31 + i32::from(self.day)
    }

    /// Days from `other` to `self`, negative once `self` is behind it.
    ///
    /// Wrapped into (-186, 186] for the same reason a bare clock is read as its
    /// nearest occurrence: the year is no more written down than the day is, so
    /// a reset dated 2 January read on 30 December is three days out rather
    /// than most of a year behind.
    fn days_from(self, other: Self) -> i32 {
        let delta = (self.ordinal() - other.ordinal()).rem_euclid(SYNTHETIC_YEAR);
        if delta > SYNTHETIC_YEAR / 2 {
            delta - SYNTHETIC_YEAR
        } else {
            delta
        }
    }

    /// `Aug 17`, as the sweep names the day a hold runs to.
    fn label(self) -> String {
        let month = MONTHS[usize::from(self.month) - 1];
        format!("{}{} {}", month[..1].to_uppercase(), &month[1..3], self.day)
    }
}

/// The local wall clock, as much of it as could be read (DESIGN.md §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalNow {
    /// Minutes past local midnight.
    pub minutes: u16,
    /// Today's date, where the reader supplied one. A clock on its own judges a
    /// dated reset by its clock half alone — what Voro did before any reset
    /// carried a date.
    pub date: Option<CapDate>,
}

impl From<u16> for LocalNow {
    fn from(minutes: u16) -> Self {
        Self {
            minutes,
            date: None,
        }
    }
}

impl LocalNow {
    /// Read `22:50 08-14`: the clock and the date, asked of `date` in one call
    /// so both halves come from one reading (`session_probe::local_now`).
    ///
    /// A date half that will not parse costs the date and not the clock, which
    /// leaves every judgement exactly where it was before dates were read.
    pub fn parse(stamp: &str) -> Option<Self> {
        let stamp = stamp.trim();
        let (clock, date) = stamp.split_once(' ').unwrap_or((stamp, ""));
        let (hour, minute) = clock.split_once(':')?;
        let (hour, minute): (u16, u16) = (hour.parse().ok()?, minute.parse().ok()?);
        if hour > 23 || minute > 59 {
            return None;
        }
        Some(Self {
            minutes: hour * 60 + minute,
            date: parse_stamp_date(date),
        })
    }
}

/// `08-14` as a date, for the clock reading above.
fn parse_stamp_date(stamp: &str) -> Option<CapDate> {
    let (month, day) = stamp.split_once('-')?;
    let (month, day): (u8, u8) = (month.parse().ok()?, day.parse().ok()?);
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some(CapDate { month, day })
}

/// What a session's output says about a usage cap. Held in memory only: it is a
/// reading of the current output tail, retaken on the next pass, so it clears
/// itself once the operator continues the session and new output displaces the
/// cap message (§8). Nothing about it reaches the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CapReading {
    /// When the window reopens, as minutes past local midnight, if the message
    /// named a time. Best-effort by design: an unparsed time badges without
    /// one rather than suppressing the badge.
    pub reset_minutes: Option<u16>,
    /// The day that reset falls on, where the message named one — a weekly cap
    /// says `resets Aug 17, 9pm`. The badge shows the clock and never the date
    /// (§8); this half is what stops the clock alone reading a reset three days
    /// out as passed the moment tonight's 9pm goes by.
    pub reset_date: Option<CapDate>,
}

impl CapReading {
    /// The reset time as a 24-hour `21:50`, for the badge.
    pub fn reset_label(&self) -> Option<String> {
        self.reset_minutes
            .map(|m| format!("{:02}:{:02}", m / 60, m % 60))
    }

    /// The reset as the sweep names the window a hold runs to: the same clock,
    /// behind the date where the message carried one, so a five-hour hold reads
    /// `until 21:50` and a weekly one `until Aug 17 21:00`.
    pub fn reset_stamp(&self) -> Option<String> {
        let clock = self.reset_label()?;
        Some(match self.reset_date {
            Some(date) => format!("{} {clock}", date.label()),
            None => clock,
        })
    }

    /// How long until the named reset, negative once the window has opened and
    /// `None` when the message named no time at all.
    ///
    /// Where the agent names a bare clock time — `9:50pm`, no date — the
    /// occurrence meant is the one *nearest* now, in either direction. Taking
    /// the next one instead would be self-defeating: a minute after the window
    /// reopened, "the next 9:50pm" is tomorrow's, and the badge would claim
    /// another 24 hours of waiting exactly when it should be saying the
    /// opposite. Half a day is the widest a bare clock time can be read
    /// unambiguously, and a five-hour window is never further off than that.
    ///
    /// A weekly window is, and it says so with a date. Where the reading and
    /// the clock both carry one, the date decides and the clock only refines
    /// it — which is what stops a reset three days out reading as passed the
    /// moment tonight's 9pm goes by. A date is compared exactly as a clock is,
    /// nearest occurrence in either direction ([`CapDate::days_from`]), and a
    /// reset dated *today* falls back to the clock rule, there being nothing
    /// for the date to decide.
    ///
    /// A signed distance rather than a bool because the sweep has to rank two
    /// live readings against each other to find the binding one
    /// ([`plan_sweep`]).
    pub fn minutes_until(&self, now: impl Into<LocalNow>) -> Option<i64> {
        let now = now.into();
        let reset = i64::from(self.reset_minutes?);
        let clock = reset - i64::from(now.minutes);
        let days = match (self.reset_date, now.date) {
            (Some(reset), Some(today)) => i64::from(reset.days_from(today)),
            _ => 0,
        };
        if days != 0 {
            return Some(days * 1440 + clock);
        }
        // Signed distance wrapped into (-720, 720]: negative is behind us.
        let delta = clock.rem_euclid(1440);
        Some(if delta > 720 { delta - 1440 } else { delta })
    }

    /// Whether the named reset has already gone by, given the local wall clock.
    pub fn reset_passed(&self, now: impl Into<LocalNow>) -> bool {
        self.minutes_until(now).is_some_and(|left| left <= 0)
    }
}

/// Read a session's output tail as a cap reading, or `None` when nothing in it
/// says the session is capped.
///
/// The *last* signature in the text decides, since a session that hit a cap,
/// was continued, and hit another has the live one at the end — and a warning
/// earlier in the same tail must not speak for a genuine cap later.
pub fn read_cap(tail: &str) -> Option<CapReading> {
    let text = strip_ansi(tail).to_lowercase();
    let (at, signature) = last_signature(&text)?;
    if look_back(&text, at, &NOT_CAP_QUALIFIERS) {
        return None;
    }
    let after = &text[at..ceil_boundary(&text, (at + signature.len() + WINDOW).min(text.len()))];
    let (reset_minutes, reset_date) = parse_reset(after);
    Some(CapReading {
        reset_minutes,
        reset_date,
    })
}

/// The position and text of the last cap signature in `text` that reports
/// something, which must already be lowercased. Signatures the upgrade prompt
/// merely mentions ([`MENTION_PREFIXES`]) are not candidates.
fn last_signature(text: &str) -> Option<(usize, &'static str)> {
    CAP_SIGNATURES
        .iter()
        .flat_map(|sig| text.match_indices(sig).map(|(at, _)| (at, *sig)))
        .filter(|(at, _)| !look_back(text, *at, &MENTION_PREFIXES))
        .max_by_key(|(at, _)| *at)
}

/// Whether any of `phrases` appears in the short span of `text` before `at`.
fn look_back(text: &str, at: usize, phrases: &[&str]) -> bool {
    let before = &text[floor_boundary(text, at.saturating_sub(QUALIFIER_WINDOW))..at];
    phrases.iter().any(|p| before.contains(p))
}

/// Drop terminal escape sequences, keeping the spacing the surviving text had
/// on screen.
///
/// A cursor move stands for the gap between two words, so it becomes a space —
/// without that, `claude logs` output runs its words together and no phrase
/// matches. A colour change stands for nothing spatial and is simply dropped,
/// so a phrase styled halfway through stays one word. Other control characters
/// (newlines and tabs included) are spacing too.
pub fn strip_ansi(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(if c.is_control() { ' ' } else { c });
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then a final byte in @..~.
            Some('[') => {
                let mut final_byte = None;
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                }
                // `m` is SGR — styling, no position — and only it is spaceless.
                if final_byte != Some('m') {
                    out.push(' ');
                }
            }
            // OSC: runs to BEL or to a string terminator.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-character escape, or a stray ESC at the end of the tail.
            _ => {}
        }
    }
    out
}

/// When the window reopens, read out of the text just after a cap signature,
/// which must already be lowercased: the clock, and the date that clock belongs
/// to where the message named one.
fn parse_reset(text: &str) -> (Option<u16>, Option<CapDate>) {
    let Some((minutes, at)) = find_clock(text) else {
        return (None, None);
    };
    (Some(minutes), parse_date_before(text, at))
}

/// The `aug 17` immediately before the clock time at `at`, where there is one.
///
/// Best-effort like every reading here: anything else in front of the clock —
/// `resets `, `retrying in 5m (`, nothing at all — is no date, which leaves the
/// reading where it was before dates were read.
fn parse_date_before(text: &str, at: usize) -> Option<CapDate> {
    let before =
        text[floor_boundary(text, at.saturating_sub(DATE_WINDOW))..at].trim_end_matches([' ', ',']);
    let day_at = before.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let day: u8 = before[day_at..].parse().ok()?;
    let word = before[..day_at].trim_end();
    let month_at = word
        .trim_end_matches(|c: char| c.is_ascii_alphabetic())
        .len();
    let word = &word[month_at..];
    let month = MONTHS
        .iter()
        .position(|m| word.len() >= 3 && m.starts_with(word))?;
    (1..=31).contains(&day).then_some(CapDate {
        month: month as u8 + 1,
        day,
    })
}

/// Minutes past midnight for the first `9pm` / `9:50pm` clock time in `text`,
/// which must already be lowercased, and where in `text` that clock begins.
///
/// This is the shape Claude Code renders a reset time in when it is less than a
/// day out, which a five-hour window always is. A longer horizon is spelled
/// with a date (`Aug 17, 9pm`); the clock half still reads here, which is all
/// the badge shows, and the position handed back is what lets the date half be
/// read beside it.
fn find_clock(text: &str) -> Option<(u16, usize)> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len().saturating_sub(1) {
        let meridiem = match (bytes[i], bytes[i + 1]) {
            (b'a', b'm') => 0,
            (b'p', b'm') => 12,
            _ => continue,
        };
        // `9pm` must not be read out of `9pmx` or a word ending in `am`.
        if bytes.get(i + 2).is_some_and(|c| c.is_ascii_alphanumeric()) {
            continue;
        }
        let mut j = i;
        while j > 0 && bytes[j - 1] == b' ' {
            j -= 1;
        }
        let end = j;
        while j > 0 && (bytes[j - 1].is_ascii_digit() || bytes[j - 1] == b':') {
            j -= 1;
        }
        let Some(minutes) = clock_minutes(&text[j..end], meridiem) else {
            continue;
        };
        return Some((minutes, j));
    }
    None
}

/// `9`, `9:50` plus a 0-or-12 hour offset, as minutes past midnight. A 12-hour
/// clock names noon and midnight as `12`, so that hour wraps to zero before the
/// offset applies.
fn clock_minutes(clock: &str, meridiem: u16) -> Option<u16> {
    let (hour, minute) = match clock.split_once(':') {
        Some((h, m)) if m.len() == 2 => (h, m.parse::<u16>().ok()?),
        Some(_) => return None,
        None => (clock, 0),
    };
    let hour: u16 = hour.parse().ok()?;
    if hour == 0 || hour > 12 || minute > 59 {
        return None;
    }
    Some((hour % 12 + meridiem) * 60 + minute)
}

/// The largest char boundary at or below `at`, so a window edge never lands
/// inside the multi-byte glyphs an agent's output is full of.
fn floor_boundary(text: &str, mut at: usize) -> usize {
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// The smallest char boundary at or above `at`.
fn ceil_boundary(text: &str, mut at: usize) -> usize {
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// One agent's badged sessions, held back from a sweep, and what holds them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapHold {
    /// The agent entry those sessions were dispatched with — Voro's proxy for
    /// the account whose window they are all waiting on.
    pub agent: String,
    /// How many of that agent's badged sessions are held.
    pub sessions: usize,
    /// The window they are held until, as [`CapReading::reset_stamp`] names it.
    /// `None` where no reading in the group named a time.
    pub until: Option<String>,
}

/// What a sweep of the badged sessions should do (DESIGN.md §8).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepPlan {
    /// The tasks to nudge, in ascending id order — a sweep visits the strip in
    /// a stable order rather than the caller's.
    pub ready: Vec<i64>,
    /// The agents nothing is nudged on, in agent-name order.
    pub held: Vec<CapHold>,
}

/// Decide which capped sessions the sweep may nudge, given every badged
/// reading as `(task, agent, reading)` and the local clock (DESIGN.md §8).
///
/// A usage cap belongs to the *account* a session was launched under and not to
/// the session, so the verdict is taken once per account rather than once per
/// session: every session of a capped account reports the same window, and
/// reading N of those texts to rebuild one boolean only invites them to
/// disagree. The way they disagree is staleness — a session capped at 4pm still
/// displaying `resets 9pm` at midnight beside one reporting a live cap — and a
/// per-session verdict reads that fossil as ready, quite correctly, and nudges
/// a session the account will re-cap on its next breath.
///
/// Voro's proxy for the account is the agent entry the session was dispatched
/// with, because that is what it knows. The proxy can be wrong in both
/// directions, and the direction it errs in is chosen: two agent entries
/// sharing one account are not pooled, which is no worse than judging each
/// session alone; one entry run under two accounts holds a session that was in
/// fact free, which costs a delay until the next press rather than a stopped
/// session and a turn that re-caps at once. Since #428 a nudge stops its target
/// first, so the conservative direction is the affordable one.
///
/// Within a group, one live reading holds every session — including the ones
/// whose message named no time, whose account has just told Voro, through a
/// sibling, that its window is shut. A group with no live reading is ready
/// entire, untimed readings included: nothing that agent said contradicts the
/// operator's keypress. The hold is labelled with the *furthest-out* live
/// reset, which is the binding one — how a weekly cap speaks over a five-hour
/// message on a sibling session.
pub fn plan_sweep(readings: &[(i64, &str, CapReading)], now: Option<LocalNow>) -> SweepPlan {
    let mut groups: std::collections::BTreeMap<&str, Vec<(i64, CapReading)>> =
        std::collections::BTreeMap::new();
    for (task_id, agent, reading) in readings {
        groups.entry(agent).or_default().push((*task_id, *reading));
    }

    let mut plan = SweepPlan::default();
    for (agent, mut group) in groups {
        group.sort_unstable_by_key(|(task_id, _)| *task_id);
        // With no clock nothing can be judged past its reset, so every timed
        // reading is live and holds its group.
        let live = |reading: &CapReading| match now {
            Some(now) => reading.minutes_until(now).is_some_and(|left| left > 0),
            None => reading.reset_minutes.is_some(),
        };
        if !group.iter().any(|(_, reading)| live(reading)) {
            plan.ready.extend(group.iter().map(|(task_id, _)| *task_id));
            continue;
        }
        // Unjudgeable, the hold takes the lowest task id's window rather than
        // whichever the map happened to yield.
        let binding = match now {
            Some(now) => group
                .iter()
                .filter(|(_, reading)| live(reading))
                .max_by_key(|(_, reading)| reading.minutes_until(now).unwrap_or(0)),
            None => group.first(),
        };
        plan.held.push(CapHold {
            agent: agent.to_string(),
            sessions: group.len(),
            until: binding.and_then(|(_, reading)| reading.reset_stamp()),
        });
    }
    plan.ready.sort_unstable();
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording every `--bg` dispatch of the built-in `claude` hits first,
    /// and the one the original three signatures all missed: a five-hour cap
    /// says "Session limit reached" and nothing about usage, rates or quotas.
    #[test]
    fn a_five_hour_cap_reads_as_capped() {
        let reading = read_cap("Session limit reached · Retrying in 5m (9:50pm) · attempt 2/10")
            .expect("a cap");
        assert_eq!(reading.reset_label().as_deref(), Some("21:50"));
    }

    /// The wording an actual five-hour cap turned out to use, captured from
    /// three live sessions on 2026-08-13 — the first real cap Voro has seen,
    /// every earlier case having been read out of the agent's own binary.
    ///
    /// The upgrade prompt riding along behind it is the whole point: it carries
    /// a signature of its own, it is last, and its window holds no time, so
    /// before it was read as boilerplate every genuine cap badged without the
    /// reset time it had actually named.
    #[test]
    fn the_real_cap_message_reads_with_its_reset_time() {
        let reading = read_cap(
            "You've hit your session limit · resets 6:40pm (Europe/London)\n\
             /upgrade to increase your usage limit.",
        )
        .expect("a cap");
        assert_eq!(reading.reset_label().as_deref(), Some("18:40"));
    }

    /// The real warning short of that cap, captured from a session that went on
    /// working — and which must stay unbadged even though the same upgrade
    /// prompt can follow it.
    #[test]
    fn the_real_warning_short_of_the_cap_is_not_capped() {
        assert_eq!(
            read_cap(
                "You've used 98% of your session limit · resets 6:40pm (Europe/London)\n\
                 /upgrade to keep using Claude Code"
            ),
            None
        );
        assert_eq!(
            read_cap(
                "You've used 99% of your session limit · resets 6:40pm (Europe/London)\n\
                 /upgrade to increase your usage limit."
            ),
            None
        );
    }

    /// The other wordings the agent uses for the same condition.
    #[test]
    fn every_cap_wording_reads_as_capped() {
        for text in [
            "You've hit your session limit · resets 9pm",
            "Weekly limit reached",
            "Opus limit reached · resets Aug 14, 9pm",
            "Usage credit limit reached",
            "usage limit reached — check plan",
            "rate limited — wait and retry",
            "sorry, hit the 5-hour usage limit — try again later",
            "quota exceeded",
        ] {
            assert!(read_cap(text).is_some(), "{text}");
        }
    }

    /// Ordinary output is not a cap, however much it talks about limits.
    #[test]
    fn ordinary_output_is_not_capped() {
        for text in [
            "",
            "running tests",
            "Context limit reached · /compact",
            "Concurrent subagent limit reached. You can run 10 subagents at once.",
            "error: recursion limit reached",
        ] {
            assert_eq!(read_cap(text), None, "{text}");
        }
    }

    /// The qualifiers that take a match back. Each of these appears while the
    /// session is working perfectly well, and badging it would be a lie the
    /// operator learns to ignore the badge over.
    #[test]
    fn a_warning_short_of_the_cap_is_not_capped() {
        for text in [
            "Approaching usage limit · resets 9pm",
            "You've used 80% of your session limit · resets 9pm",
            "Server is temporarily limiting requests (not your usage limit)",
            "You're close to your usage limit",
        ] {
            assert_eq!(read_cap(text), None, "{text}");
        }
    }

    /// A warning earlier in the same tail does not speak for a real cap later:
    /// the last signature decides, so a session that was warned and then
    /// genuinely capped still badges.
    #[test]
    fn the_last_signature_decides() {
        let tail = "Approaching usage limit · resets 9pm\n\
                    ... work continues ...\n\
                    Session limit reached · Retrying in 5m (9:50pm)";
        assert_eq!(
            read_cap(tail).expect("a cap").reset_label().as_deref(),
            Some("21:50")
        );

        // And the other order: a real cap the operator has since cleared, with
        // only a warning left at the end, is no longer capped.
        let tail = "Session limit reached\n... continued ...\nApproaching usage limit";
        assert_eq!(read_cap(tail), None);
    }

    /// A cap with no time still badges — the time is the optional half.
    #[test]
    fn a_cap_without_a_time_reads_without_one() {
        let reading = read_cap("Weekly limit reached").expect("a cap");
        assert_eq!(reading.reset_minutes, None);
        assert_eq!(reading.reset_label(), None);
        assert!(!reading.reset_passed(600));
    }

    #[test]
    fn clock_times_parse_to_minutes_past_midnight() {
        for (text, minutes) in [
            ("9pm", 21 * 60),
            ("9:50pm", 21 * 60 + 50),
            ("9am", 9 * 60),
            ("12am", 0),
            ("12:30am", 30),
            ("12pm", 12 * 60),
            ("12:30pm", 12 * 60 + 30),
            ("1:05am", 65),
        ] {
            assert_eq!(
                read_cap(&format!("Session limit reached · resets {text}"))
                    .expect("a cap")
                    .reset_minutes,
                Some(minutes),
                "{text}"
            );
        }
    }

    /// Nothing that only looks like a clock time is read as one.
    #[test]
    fn non_times_are_not_read_as_times() {
        for text in [
            "Session limit reached",
            "Session limit reached · stream",
            "Session limit reached · 25pm",
            "Session limit reached · 0am",
            "Session limit reached · 9:5pm",
            "Session limit reached · 9:75pm",
            "Session limit reached · 9pmx",
        ] {
            assert_eq!(read_cap(text).expect("a cap").reset_minutes, None, "{text}");
        }
    }

    /// Nearest-occurrence, in both directions: a reset an hour out is still
    /// ahead, one an hour back has gone by, and the answer holds across
    /// midnight where a naive comparison flips.
    #[test]
    fn a_reset_is_passed_by_nearest_occurrence() {
        let at = |m| CapReading {
            reset_minutes: Some(m),
            reset_date: None,
        };
        // 21:50 reset, read at 20:50 — an hour to go.
        assert!(!at(1310).reset_passed(1250));
        // ...and at 22:50, an hour after it opened.
        assert!(at(1310).reset_passed(1370));
        // The reset instant itself counts as passed.
        assert!(at(1310).reset_passed(1310));
        // 00:30 reset read at 23:30 is half an hour ahead, not 23 behind.
        assert!(!at(30).reset_passed(1410));
        // 23:30 reset read at 00:30 is half an hour behind, not 23 ahead.
        assert!(at(1410).reset_passed(30));
    }

    /// The `claude logs` shape: a terminal capture whose words are separated by
    /// cursor-column moves and whose phrases are broken up by colour changes.
    /// Reading it as a plain string finds nothing at all.
    #[test]
    fn a_terminal_capture_reads_as_its_rendered_text() {
        let capture = "\u{1b}[?25l\u{1b}[H\u{1b}[38;2;215;119;87mSession\u{1b}[8G\u{1b}[1mlimit\
                       \u{1b}[39m\u{1b}[14Greached\u{1b}[22G·\u{1b}[24Gresets\u{1b}[31G9:50pm\
                       \u{1b}[39m\u{1b}[?25h";
        let reading = read_cap(capture).expect("a cap");
        assert_eq!(reading.reset_label().as_deref(), Some("21:50"));
    }

    /// The two halves of the stripping rule, stated on their own: a cursor move
    /// is a gap and a colour change is not.
    #[test]
    fn stripping_keeps_spacing_but_not_styling() {
        assert_eq!(
            strip_ansi("and\u{1b}[97Gthe\u{1b}[101Gform"),
            "and the form"
        );
        assert_eq!(
            strip_ansi("ses\u{1b}[1msion \u{1b}[38;5;153mlimit"),
            "session limit"
        );
        assert_eq!(strip_ansi("a\nb\tc"), "a b c");
        assert_eq!(strip_ansi("\u{1b}]0;a title\u{7}kept"), "kept");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    /// A reset far enough out to be spelled with a date is read in both halves:
    /// the clock the badge shows, and the day that tells the sweep the window
    /// is still days away.
    #[test]
    fn a_dated_reset_reads_both_halves() {
        let reading = read_cap("Opus limit reached · resets Aug 14, 9pm").expect("a cap");
        assert_eq!(reading.reset_minutes, Some(21 * 60));
        assert_eq!(reading.reset_date, Some(CapDate { month: 8, day: 14 }));
        assert_eq!(reading.reset_label().as_deref(), Some("21:00"));
        assert_eq!(reading.reset_stamp().as_deref(), Some("Aug 14 21:00"));

        // The other spellings of the same day, and a five-hour reset with no
        // date in front of it.
        for text in ["resets august 14, 9pm", "resets aug 14 9pm"] {
            let reading = read_cap(&format!("Weekly limit reached · {text}")).expect("a cap");
            assert_eq!(
                reading.reset_date,
                Some(CapDate { month: 8, day: 14 }),
                "{text}"
            );
        }
        assert_eq!(
            read_cap("Session limit reached · Retrying in 5m (9:50pm)")
                .expect("a cap")
                .reset_date,
            None
        );
    }

    /// The bug a date exists to fix: a weekly reset three days out was read as
    /// passed the moment tonight's 9pm went by, and the sweep would have
    /// released the session to spend a turn that re-caps at once.
    #[test]
    fn a_weekly_reset_is_not_passed_days_early() {
        let reading = read_cap("Weekly limit reached · resets Aug 17, 9pm").expect("a cap");
        for hour in 0..24u16 {
            let now = LocalNow {
                minutes: hour * 60,
                date: Some(CapDate { month: 8, day: 14 }),
            };
            assert!(!reading.reset_passed(now), "{hour}:00");
        }
    }

    /// And the other direction: a date behind today has gone by whatever the
    /// clock says, where the bare clock would have called half of those hours
    /// a wait.
    #[test]
    fn a_reset_dated_behind_today_has_passed() {
        let reading = read_cap("Weekly limit reached · resets Aug 12, 9pm").expect("a cap");
        for hour in 0..24u16 {
            let now = LocalNow {
                minutes: hour * 60,
                date: Some(CapDate { month: 8, day: 14 }),
            };
            assert!(reading.reset_passed(now), "{hour}:00");
        }
    }

    /// Dates are compared by nearest occurrence exactly as clocks are, the year
    /// being no more written down than the day: a reset in early January read
    /// on 30 December is days out, not most of a year behind.
    #[test]
    fn dates_wrap_the_year_boundary() {
        let ahead = read_cap("Weekly limit reached · resets Jan 2, 9pm").expect("a cap");
        let behind = read_cap("Weekly limit reached · resets Dec 30, 9pm").expect("a cap");
        let dec30 = LocalNow {
            minutes: 12 * 60,
            date: Some(CapDate { month: 12, day: 30 }),
        };
        let jan2 = LocalNow {
            minutes: 12 * 60,
            date: Some(CapDate { month: 1, day: 2 }),
        };
        assert!(!ahead.reset_passed(dec30));
        assert!(behind.reset_passed(jan2));
    }

    /// A date invents no clock, and a date in text that never said a cap
    /// invents no reading at all.
    #[test]
    fn a_date_alone_is_not_a_reset_time() {
        let reading = read_cap("Weekly limit reached · resets Aug 17").expect("a cap");
        assert_eq!(reading.reset_minutes, None);
        assert_eq!(reading.reset_date, None);
        assert_eq!(read_cap("your slot resets Aug 17, 9pm"), None);
    }

    /// A reading with no clock to read `now` against is judged as it always
    /// was, by the clock half alone — which is the whole of what an unreadable
    /// date leaves behind.
    #[test]
    fn an_undated_clock_judges_a_dated_reset_by_its_clock() {
        let reading = read_cap("Weekly limit reached · resets Aug 17, 9pm").expect("a cap");
        assert!(!reading.reset_passed(20 * 60));
        assert!(reading.reset_passed(22 * 60));
    }

    /// A cap reading with a reset `minutes` minutes past midnight and no date.
    fn timed(minutes: u16) -> CapReading {
        CapReading {
            reset_minutes: Some(minutes),
            reset_date: None,
        }
    }

    /// Midday on 14 August, the clock the plan tests judge against.
    fn midday() -> Option<LocalNow> {
        Some(LocalNow {
            minutes: 12 * 60,
            date: Some(CapDate { month: 8, day: 14 }),
        })
    }

    /// The account fact, which is the point of the whole rollup: one agent's
    /// two sessions, one reporting a live window and one still showing the
    /// window it was capped in hours ago. Judged apart, the fossil reads ready
    /// and is nudged into an account that is still shut; judged together, the
    /// live sibling holds them both and the report names the real window.
    #[test]
    fn a_live_sibling_holds_a_fossil_of_the_same_agent() {
        let plan = plan_sweep(
            &[(1, "claude", timed(9 * 60)), (2, "claude", timed(13 * 60))],
            midday(),
        );
        assert!(plan.ready.is_empty(), "{plan:?}");
        assert_eq!(
            plan.held,
            vec![CapHold {
                agent: "claude".into(),
                sessions: 2,
                until: Some("13:00".into()),
            }]
        );
    }

    /// The same two sessions under different agent names are different
    /// accounts, and nothing pools them: the one whose window has reopened is
    /// swept, the other held.
    #[test]
    fn different_agents_are_judged_apart() {
        let plan = plan_sweep(
            &[(1, "claude", timed(9 * 60)), (2, "codex", timed(13 * 60))],
            midday(),
        );
        assert_eq!(plan.ready, vec![1]);
        assert_eq!(
            plan.held,
            vec![CapHold {
                agent: "codex".into(),
                sessions: 1,
                until: Some("13:00".into()),
            }]
        );
    }

    /// A cap whose message named no time is the operator's call, not the
    /// clock's — unless the account itself contradicts them, which is what a
    /// live sibling of the same agent does and a live session of another agent
    /// does not.
    #[test]
    fn an_untimed_reading_is_swept_unless_its_own_agent_is_held() {
        let alone = plan_sweep(&[(1, "claude", CapReading::default())], midday());
        assert_eq!(alone.ready, vec![1]);
        assert!(alone.held.is_empty());

        let sibling = plan_sweep(
            &[
                (1, "claude", CapReading::default()),
                (2, "claude", timed(13 * 60)),
            ],
            midday(),
        );
        assert!(sibling.ready.is_empty(), "{sibling:?}");
        assert_eq!(sibling.held[0].sessions, 2);

        let stranger = plan_sweep(
            &[
                (1, "claude", CapReading::default()),
                (2, "codex", timed(13 * 60)),
            ],
            midday(),
        );
        assert_eq!(stranger.ready, vec![1]);
        assert_eq!(stranger.held[0].agent, "codex");
    }

    /// The binding window is the furthest-out live one — how a weekly cap on
    /// one session speaks over a five-hour message on its sibling.
    #[test]
    fn a_hold_is_labelled_with_the_furthest_out_reset() {
        let weekly = CapReading {
            reset_minutes: Some(21 * 60),
            reset_date: Some(CapDate { month: 8, day: 17 }),
        };
        let plan = plan_sweep(
            &[(1, "claude", timed(13 * 60)), (2, "claude", weekly)],
            midday(),
        );
        assert_eq!(plan.held[0].until.as_deref(), Some("Aug 17 21:00"));
    }

    /// With no clock to read, nothing can be judged past its reset: every timed
    /// reading holds, and a group that named no times at all is still the
    /// operator's call.
    #[test]
    fn an_unknown_clock_judges_nothing_passed() {
        let plan = plan_sweep(
            &[
                (2, "claude", timed(13 * 60)),
                (1, "claude", timed(9 * 60)),
                (3, "codex", CapReading::default()),
            ],
            None,
        );
        assert_eq!(plan.ready, vec![3]);
        assert_eq!(
            plan.held,
            vec![CapHold {
                agent: "claude".into(),
                sessions: 2,
                // The lowest task id's window, rather than whichever the map
                // happened to yield.
                until: Some("09:00".into()),
            }]
        );
    }

    /// A sweep visits the strip in a stable order rather than the caller's.
    #[test]
    fn ready_tasks_come_back_in_ascending_order() {
        let plan = plan_sweep(
            &[
                (9, "codex", timed(9 * 60)),
                (2, "claude", timed(9 * 60)),
                (5, "claude", CapReading::default()),
            ],
            midday(),
        );
        assert_eq!(plan.ready, vec![2, 5, 9]);
        assert!(plan.held.is_empty());
    }

    /// Nothing badged is no plan at all.
    #[test]
    fn nothing_badged_plans_nothing() {
        assert_eq!(plan_sweep(&[], midday()), SweepPlan::default());
    }

    /// A signature landing at the very edge of the text windows the qualifier
    /// check over multi-byte output without panicking on a char boundary.
    #[test]
    fn windows_never_split_a_multibyte_glyph() {
        let padding = "·".repeat(400);
        assert!(read_cap(&format!("{padding}session limit reached{padding}")).is_some());
        assert!(read_cap("session limit").is_some());
    }
}
