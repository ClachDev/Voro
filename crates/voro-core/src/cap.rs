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

/// How much text before a matched signature is read for the qualifiers that
/// take it back. Deliberately short: every qualifier attaches directly to the
/// phrase it modifies, so a wider look-back would let an *earlier* warning
/// speak for a later, genuine cap — which is the one reading that loses a real
/// cap rather than merely missing an unworded one.
const QUALIFIER_WINDOW: usize = 32;

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
}

impl CapReading {
    /// The reset time as a 24-hour `21:50`, for the badge.
    pub fn reset_label(&self) -> Option<String> {
        self.reset_minutes
            .map(|m| format!("{:02}:{:02}", m / 60, m % 60))
    }

    /// Whether the named reset has already gone by, given the local wall clock
    /// as minutes past midnight.
    ///
    /// The agent names a bare clock time — `9:50pm`, no date — so the occurrence
    /// meant is the one *nearest* now, in either direction. Taking the next one
    /// instead would be self-defeating: a minute after the window reopened,
    /// "the next 9:50pm" is tomorrow's, and the badge would claim another 24
    /// hours of waiting exactly when it should be saying the opposite. Half a
    /// day is the widest a bare clock time can be read unambiguously, and a cap
    /// window the operator is watching is never further off than that.
    pub fn reset_passed(&self, now_minutes: u16) -> bool {
        let Some(reset) = self.reset_minutes else {
            return false;
        };
        // Signed distance wrapped into (-720, 720]: negative is behind us.
        let delta = (i32::from(reset) - i32::from(now_minutes)).rem_euclid(1440);
        let delta = if delta > 720 { delta - 1440 } else { delta };
        delta <= 0
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
    Some(CapReading {
        reset_minutes: parse_clock(after),
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

/// Minutes past midnight for the first `9pm` / `9:50pm` clock time in `text`,
/// which must already be lowercased.
///
/// This is the shape Claude Code renders a reset time in when it is less than a
/// day out, which a cap window the operator is looking at always is. A longer
/// horizon is spelled with a date (`Aug 14, 9pm`) and the clock half still
/// reads, which is the right answer for a badge that shows a time and not a
/// date.
fn parse_clock(text: &str) -> Option<u16> {
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
        return Some(minutes);
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

    /// A signature landing at the very edge of the text windows the qualifier
    /// check over multi-byte output without panicking on a char boundary.
    #[test]
    fn windows_never_split_a_multibyte_glyph() {
        let padding = "·".repeat(400);
        assert!(read_cap(&format!("{padding}session limit reached{padding}")).is_some());
        assert!(read_cap("session limit").is_some());
    }
}
