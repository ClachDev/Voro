//! The pieces a task row and a detail view are built from, shared by every
//! screen: badges, spans, and the score, history, session and dependency
//! lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use voro_core::{
    CapWindow, CompletionReport, DepKind, DepRef, Event, ScoreBreakdown, Session, SessionOutcome,
    TaskState,
};

use super::task_ref;
use crate::app::App;

/// A compact one-line rendering of a possibly multi-line question, for the row
/// summaries where only a single line fits: the first line, with a trailing `…`
/// when there is more (the full text reads in the cockpit detail pane).
pub(super) fn question_summary(question: &str) -> String {
    let mut lines = question.lines();
    let first = lines.next().unwrap_or("");
    if lines.next().is_some() {
        format!("{first}…")
    } else {
        first.to_string()
    }
}

/// The inline score decomposition (DESIGN.md §7) that `x` folds into a detail
/// view: one dim line breaking the total down, plus a "not scheduled" note
/// where the task's state keeps it out of the queue. Shared by the cockpit pane
/// and the tasks-screen Detail popup.
pub(super) fn score_lines(b: &ScoreBreakdown) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "weight {} · {} (value {}) · {} (+{}) · blocks ×{} (+{}) · base w×(p+s+u) {:.1} · age {:.1}d (+{:.2}) = {:.2}",
            b.weight,
            b.priority,
            b.priority_value,
            b.state,
            b.state_bonus,
            b.open_dependents,
            b.unblock_bonus,
            b.base,
            b.age_days,
            b.age_bonus,
            b.total
        ),
        Style::new().dim(),
    ))];
    if !matches!(
        b.state,
        TaskState::Ready
            | TaskState::NeedsInput
            | TaskState::Review
            | TaskState::Stalled
            | TaskState::Proposed
    ) {
        lines.push(Line::from(Span::styled(
            format!("({} tasks are not scheduled)", b.state),
            Style::new().dim(),
        )));
    }
    lines
}

/// The event-history section that `h` folds into a detail view: a bold
/// "History" header over one line per event — timestamp dim, kind bold, detail
/// plain, oldest first.
pub(super) fn history_lines(events: &[Event]) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled("History", Style::new().bold()))];
    if events.is_empty() {
        lines.push(Line::from(Span::styled(
            "no events yet",
            Style::new().dim(),
        )));
    } else {
        lines.extend(events.iter().map(|e| {
            Line::from(vec![
                Span::styled(format!("{:<19} ", e.at), Style::new().dim()),
                Span::styled(format!("{:<10} ", e.kind), Style::new().bold()),
                Span::raw(crate::cli::event_detail(e)),
            ])
        }));
    }
    lines
}

pub(super) fn score_span(total: f64) -> Span<'static> {
    Span::styled(format!("{total:5.1} "), Style::new().fg(Color::Yellow))
}

/// The incomplete-report flag (DESIGN.md §8): a `review` task carrying a branch
/// and no summary. Yellow to match the running strip's "no live
/// session" warning, since both are anomalies needing the operator.
pub(super) fn incomplete_report_span() -> Span<'static> {
    Span::styled(
        "  [incomplete report]",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// The refine marker (DESIGN.md §6): a proposal whose last refine round
/// reworked its body against the operator's note, so this row is an improved
/// version awaiting a fresh verdict. A property of the row rather than an
/// anomaly — cyan like the question text, not the warning yellow.
pub(super) fn refined_span() -> Span<'static> {
    Span::styled("  ↻ refined", Style::new().fg(Color::Cyan))
}

/// Its counterpart (DESIGN.md §6): a proposal whose last refine round died
/// without rewriting anything. Red rather than cyan, because the body the
/// operator is about to read is the *old* one and the rewrite they asked for
/// never happened — an absence they should never have to notice for themselves.
pub(super) fn refine_failed_span() -> Span<'static> {
    Span::styled(
        "  ⚠ refine failed",
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    )
}

/// A refine in flight on the running strip (DESIGN.md §9), where it sits beside
/// dispatched work: same columns, but named for what it is, since the keys that
/// act on a dispatch do not act on this.
pub(super) fn refining_span() -> Span<'static> {
    Span::styled(
        format!("{:11} ", "⟳ refining"),
        Style::new().fg(Color::Cyan),
    )
}

/// A hand-off on the running strip (DESIGN.md §9): work in flight that someone
/// else owns, sitting beside the work an agent owns. Blue rather than the
/// refine's cyan, because nothing here is being typed into — the elapsed time
/// beside it counts from the hand-off, not from any session.
///
/// Padded one narrower than the other state labels: the hourglass is a
/// double-width glyph, so nine characters here occupy the same eleven columns
/// the state column holds everywhere else.
pub(super) fn waiting_span() -> Span<'static> {
    Span::styled(
        format!("{:10} ", "⏳ waiting"),
        Style::new().fg(Color::Blue),
    )
}

/// What a waiting strip row is holding up (DESIGN.md §7): its direct open
/// `blocks` dependents, counted by the rule the `unblock_bonus` uses. A waiting
/// task earns no score, so the count cannot reach the operator through the
/// queue — this badge is the only place the gating shows.
pub(super) fn blocks_span(open_dependents: usize) -> Span<'static> {
    Span::styled(
        format!("  blocks {open_dependents}"),
        Style::new().fg(Color::Yellow),
    )
}

/// A tracked PR on a waiting strip row — presence only, never its state, which
/// Voro does not poll (DESIGN.md §8).
pub(super) fn strip_pr_span() -> Span<'static> {
    Span::styled("  PR", Style::new().fg(Color::Magenta))
}

/// The usage-cap badge on a running strip row (DESIGN.md §8): this session is
/// alive but held at a cap, doing nothing until the window reopens.
///
/// Yellow rather than red, and no state change behind it, because nothing has
/// gone wrong — the session is intact and will pick up where it left off.
/// Without the badge a capped row is indistinguishable from work in progress.
///
/// Three shapes, in decreasing order of what Voro managed to learn. With a
/// reset time still ahead, `⚠ capped ↻21:50` — the operator can decide
/// whether to wait. Past that time the window is open and the session is merely
/// waiting to be nudged, which is a different situation and a different thing
/// to do about it, so it says so. With no time learned at all, the bare badge:
/// the cap is the part worth knowing, and suppressing it for want of a
/// timestamp would trade the whole signal for a detail.
///
/// The time itself comes from whichever source could give it — the account's
/// own instant where the agent reports one, the clock time on the session's
/// screen otherwise ([`CapWindow`]) — and the badge is the same either way. It
/// is only the *third* shape that differs, and invisibly: "reset passed" read
/// off an instant is a fact, where read off a bare `6:40pm` it is the nearest
/// occurrence of a time that carries no date.
///
/// A fourth says the session is retrying it (§8), which is the one shape that
/// wants *nothing* done about it: the turn is still running and will carry on
/// by itself. It still badges rather than reading as healthy, because a
/// session sitting on a retry is as idle-looking on the strip as a capped one
/// and the operator deserves the reason — but it says which, so `u` passing it
/// over reads as the right answer instead of a missed row.
pub(super) fn capped_span(window: &CapWindow) -> Span<'static> {
    let text = match (&window.label, window.passed, window.retrying) {
        (Some(at), _, true) => format!("  ⚠ capped · retrying ↻{at}"),
        (None, _, true) => "  ⚠ capped · retrying".to_string(),
        (_, true, false) => "  ⚠ capped · reset passed".to_string(),
        (Some(at), false, false) => format!("  ⚠ capped ↻{at}"),
        (None, false, false) => "  ⚠ capped".to_string(),
    };
    Span::styled(
        text,
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// The stale-branch marker (DESIGN.md §8): a review task whose tracked PR
/// reports a merge conflict, probed on demand for the selected task. Purely
/// informational — it flags that the branch needs resolving before it can
/// merge — the same shout as `[incomplete report]`.
pub(super) fn conflict_span() -> Span<'static> {
    Span::styled(
        "  [branch conflicts]",
        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
    )
}

/// A queue row's state cell, coloured by what the state asks of the operator.
/// Only the states that are stuck on them take a colour: `needs-input` cyan,
/// the hue the agent already speaks in throughout the TUI (the question suffix,
/// `↻ refined`, `⟳ refining`); `review` green, matching the branch and repo
/// lines in the detail pane, since a git artifact is what there is to look at;
/// `stalled` red, matching the failed-session line. `ready` stays plain so an
/// uncoloured queue reads as nothing waiting on the operator, and `proposed`
/// stays dim with the rest of its row. No bold — that is reserved for the row
/// markers, which sit above the state cell in the hierarchy.
pub(super) fn state_span(state: TaskState) -> Span<'static> {
    let style = match state {
        TaskState::NeedsInput => Style::new().fg(Color::Cyan),
        TaskState::Review => Style::new().fg(Color::Green),
        TaskState::Stalled => Style::new().fg(Color::Red),
        TaskState::Proposed => Style::new().dim(),
        _ => Style::new(),
    };
    Span::styled(format!("{:11}", state.as_str()), style)
}

/// The human-only flag rendered as a row marker. A property of the
/// task rather than an anomaly, so it stays dim where the warning flags shout.
pub(super) fn human_span() -> Span<'static> {
    Span::styled("  [human]", Style::new().dim())
}

/// The same flag spelled out for a detail view, beside the branch/PR lines.
pub(super) fn human_line() -> Line<'static> {
    Line::from(Span::styled(
        "human-only — never dispatched",
        Style::new().dim(),
    ))
}

/// The deep flag as a one-column row marker sitting beside the
/// priority cell: `!` when the task dispatches on the agent's strongest model,
/// a blank of the same width otherwise, so the columns after it stay aligned
/// whether or not any row in the list is deep.
pub(super) fn deep_marker(deep: bool) -> Span<'static> {
    if deep {
        Span::styled(
            "!",
            Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(" ")
    }
}

/// The same flag spelled out for a detail view, beside the human line.
pub(super) fn deep_line() -> Line<'static> {
    Line::from(Span::styled(
        "deep — dispatches on the agent's strongest model",
        Style::new().fg(Color::Magenta),
    ))
}

/// A tracked GitHub PR (DESIGN.md §11c) rendered for the detail pane, with the
/// jump-to-PR key spelled out so the reviewer knows how to reach it.
pub(super) fn pr_span(url: &str) -> Span<'static> {
    Span::styled(
        format!("PR: {url}  (g to open)"),
        Style::new().fg(Color::Blue),
    )
}

/// The task's git branch rendered for the detail pane — the intended
/// name dispatch injects, or the name the agent reported it worked on.
pub(super) fn branch_span(branch: &str) -> Span<'static> {
    Span::styled(format!("branch: {branch}"), Style::new().fg(Color::Green))
}

/// The checkout a task runs in (DESIGN.md §3), rendered only when the task
/// names a repo of its own — a task on the project default reads as it always
/// did, so the line appears exactly when it carries information.
pub(super) fn repo_span(name: &str, path: &str) -> Span<'static> {
    Span::styled(
        format!("repo: {name} ({path})"),
        Style::new().fg(Color::Green),
    )
}

/// The plans a task derives from (DESIGN.md §3), one line each: the title when
/// the document carries one, then where it resolves to — the same location
/// dispatch names in the agent's prompt. A task citing no document renders
/// nothing.
pub(super) fn doc_lines(app: &App, task_id: i64) -> Vec<Line<'static>> {
    app.docs
        .get(&task_id)
        .map_or(&[][..], |v| v)
        .iter()
        .map(|doc| {
            let location = app
                .doc_locations
                .get(&doc.id)
                .cloned()
                .unwrap_or_else(|| doc.location.clone());
            let text = match &doc.title {
                Some(title) => format!("doc: {title} — {location}"),
                None => format!("doc: {location}"),
            };
            Line::from(Span::styled(text, Style::new().fg(Color::Magenta)))
        })
        .collect()
}

/// What the agent reported (DESIGN.md §8): the completion summary of the cycle
/// in hand, rendered above the body — the body is the instruction that has
/// already been carried out, the summary is the account of carrying it out, and
/// the account is what a verdict is given against. It is the only such account
/// a project with no PR and no configured viewer has, so the card cannot leave
/// it to `voro show`. On a rework it is headed by the feedback it answers,
/// which is the other half of making a re-review proportional to the fix — the
/// operator reads *what the agent says it changed* here and the diff since the
/// rejected revision beside it.
pub(super) fn completion_lines(report: &CompletionReport, width: u16) -> Vec<Line<'static>> {
    let heading = match report.feedback {
        Some(_) => "response to the review feedback:",
        None => "completion summary:",
    };
    let mut lines = vec![Line::default()];
    lines.extend(agent_voice_block(heading, &report.summary, width));
    lines
}

/// The gutter that marks a block as the agent's own words rather than the
/// operator's instruction. Two columns wide, repeated on every visual line.
const GUTTER: &str = "│ ";

/// An agent-authored block — a completion summary, its rework variant, or a
/// question — rendered as markdown behind a quote-style gutter.
/// The content is styled exactly as a task body is, so cyan text means inline
/// code here as it does there; the voice is carried by the bar in the margin
/// instead of by a colour wash. Lines are wrapped to fit inside the gutter
/// before it is prefixed, because the card's `Paragraph` re-wraps a long line
/// without repeating anything and would leave the bar broken part-way down.
pub(super) fn agent_voice_block(heading: &str, text: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        heading.to_string(),
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(crate::markdown::body_lines(text));
    let inner = (width as usize).saturating_sub(GUTTER.chars().count());
    crate::markdown::wrap_lines(lines, inner)
        .into_iter()
        .map(|line| {
            // A blank line keeps a bare bar, so the block reads as one column
            // for its full height rather than as fragments.
            let bar = if line.width() == 0 {
                GUTTER.trim_end()
            } else {
                GUTTER
            };
            let mut spans = vec![Span::styled(bar, Style::new().fg(Color::Cyan))];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}

/// A task's newest session, rendered for the attention states.
/// A finished session is a post-mortem: its outcome (`capped` yellow — it clears
/// when the quota resets — `failed` red and wanting its log read), agent, and
/// end time. An open one shows agent and start time. Both end on the log path
/// the `l` key pages. States where the session is history rather than context
/// (`done`, `rejected`, a redispatch-ready task) render nothing.
pub(super) fn session_lines(session: &Session, state: TaskState) -> Vec<Line<'static>> {
    if !matches!(
        state,
        TaskState::Stalled
            | TaskState::Running
            | TaskState::Review
            | TaskState::Waiting
            | TaskState::NeedsInput
    ) {
        return Vec::new();
    }
    let mut lines = vec![match &session.ended_at {
        Some(ended) => {
            let outcome_color = match session.outcome {
                Some(SessionOutcome::Capped) => Color::Yellow,
                _ => Color::Red,
            };
            let outcome = session
                .outcome
                .map(|o| o.to_string())
                .unwrap_or_else(|| "unknown".into());
            Line::from(vec![
                Span::raw("last session: "),
                Span::styled(
                    outcome,
                    Style::new().fg(outcome_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" · {} · ended {ended}", session.agent),
                    Style::new().dim(),
                ),
            ])
        }
        None => Line::from(vec![
            Span::raw("session: "),
            Span::styled(
                format!("{} · started {}", session.agent, session.started_at),
                Style::new().dim(),
            ),
        ]),
    }];
    lines.push(match &session.log_path {
        Some(path) => Line::from(vec![
            Span::styled(format!("log: {path}"), Style::new().dim()),
            Span::styled("  (l opens in $PAGER)", Style::new().fg(Color::Blue)),
        ]),
        None => Line::from(Span::styled(
            "no session log was recorded",
            Style::new().dim(),
        )),
    });
    lines
}

/// The dependency section of a detail view, both directions, one
/// line per edge: `blocked by #N title` for the task's own blockers, `blocks #N
/// title` for the reverse edges, and other forward kinds by name. Closed tasks
/// are dimmed, as in `blocker_spans`.
pub(super) fn dep_lines(deps: &[DepRef], dependents: &[DepRef]) -> Vec<Line<'static>> {
    let blocked_by = deps.iter().filter(|d| d.kind == DepKind::Blocks);
    let blocks = dependents.iter().filter(|d| d.kind == DepKind::Blocks);
    let other = deps.iter().filter(|d| d.kind != DepKind::Blocks);
    blocked_by
        .map(|d| dep_line("blocked by", d))
        .chain(blocks.map(|d| dep_line("blocks", d)))
        .chain(other.map(|d| dep_line(d.kind.as_str(), d)))
        .collect()
}

fn dep_line(label: &str, d: &DepRef) -> Line<'static> {
    let target = if d.is_open() {
        Style::new()
    } else {
        Style::new().dim()
    };
    Line::from(vec![
        Span::styled(format!("{label} "), Style::new().dim()),
        Span::styled(format!("{} {}", task_ref(d.id).trim(), d.title), target),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::draw;
    use crate::ui::tests::alt_screen;
    use voro_core::Priority;

    #[test]
    fn question_summary_collapses_a_multi_line_question_to_its_first_line() {
        // a single-line question is unchanged
        assert_eq!(question_summary("Schema A or B?"), "Schema A or B?");
        // a multi-line one collapses to the first line with a trailing ellipsis,
        // the marker that there is more to read in the detail pane
        assert_eq!(
            question_summary("Which schema?\nA: normalised\nB: flat"),
            "Which schema?…"
        );
    }

    /// The queue's state cell is coloured only where the state is stuck on the
    /// operator, and never bold — the row markers own that weight.
    #[test]
    fn state_cell_colours_only_the_states_stuck_on_the_operator() {
        let cases = [
            (TaskState::NeedsInput, Some(Color::Cyan)),
            (TaskState::Review, Some(Color::Green)),
            (TaskState::Stalled, Some(Color::Red)),
            (TaskState::Ready, None),
            (TaskState::Proposed, None),
        ];
        for (state, fg) in cases {
            let span = state_span(state);
            assert_eq!(span.style.fg, fg, "{}", state.as_str());
            assert!(
                !span.style.add_modifier.contains(Modifier::BOLD),
                "{}",
                state.as_str()
            );
        }
        assert!(
            state_span(TaskState::Proposed)
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !state_span(TaskState::Ready)
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    /// End-to-end: a human-only task carries the `[human]` marker on its queue
    /// and browser rows and the spelled-out line in the cockpit detail pane,
    /// drawn dim rather than warning-coloured — a property, not an anomaly.
    #[test]
    fn human_only_flag_renders_in_queue_browser_and_detail() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "hands-on".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: true,
                deep: false,
                milestone: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let cockpit = render(&app, &mut terminal);
        assert!(
            cockpit.contains("[human]"),
            "queue row should carry the marker: {cockpit}"
        );
        assert!(
            cockpit.contains("human-only — never dispatched"),
            "detail pane should spell the flag out: {cockpit}"
        );

        app.toggle_screen();
        let browser = render(&app, &mut terminal);
        assert!(
            browser.contains("[human]"),
            "browser row should carry the marker: {browser}"
        );

        let marker = human_span();
        assert!(marker.style.add_modifier.contains(Modifier::DIM));
        assert!(!marker.style.add_modifier.contains(Modifier::BOLD));
    }

    /// End-to-end: a deep task carries the `!` marker beside the priority cell
    /// on its queue and browser rows and the spelled-out line in the cockpit
    /// detail pane; a task on the workhorse carries neither.
    #[test]
    fn deep_flag_renders_in_queue_browser_and_detail() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str, deep: bool| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep,
            milestone: false,
        };
        store.create_task(new("the hard one", true)).unwrap();
        store.create_task(new("the ordinary one", false)).unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let cockpit = render(&app, &mut terminal);
        assert!(
            cockpit.contains("P2! voro: the hard one"),
            "queue row should mark the deep task: {cockpit}"
        );
        assert!(
            cockpit.contains("P2  voro: the ordinary one"),
            "a workhorse row keeps the column blank: {cockpit}"
        );
        assert!(
            cockpit.contains("deep — dispatches on the agent's strongest model"),
            "detail pane should spell the flag out: {cockpit}"
        );

        app.toggle_screen();
        let browser = render(&app, &mut terminal);
        assert!(
            browser.contains("P2! w3"),
            "browser row should mark the deep task: {browser}"
        );
        assert!(
            browser.contains("P2  w3"),
            "a workhorse row keeps the column blank: {browser}"
        );
    }

    /// The dependency section lists the task's own blockers first, then the
    /// reverse edges it holds back, then other kinds by name — closed tasks
    /// dimmed, open ones plain — and reverse edges of non-blocks kinds (which
    /// would read in the wrong direction) not at all.
    #[test]
    fn dep_lines_render_both_directions_with_closed_targets_dimmed() {
        use voro_core::{DepKind, DepRef};

        let dep = |id: i64, kind, state| DepRef {
            id,
            title: format!("t{id}"),
            state,
            kind,
        };
        let deps = vec![
            dep(4, DepKind::Blocks, TaskState::Done),
            dep(6, DepKind::DiscoveredFrom, TaskState::Ready),
        ];
        let dependents = vec![
            dep(9, DepKind::Blocks, TaskState::Ready),
            dep(11, DepKind::Related, TaskState::Ready),
        ];

        let lines = dep_lines(&deps, &dependents);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            text,
            vec!["blocked by #4 t4", "blocks #9 t9", "discovered-from #6 t6"]
        );

        let closed = &lines[0].spans[1];
        assert!(closed.style.add_modifier.contains(Modifier::DIM));
        let open = &lines[1].spans[1];
        assert!(!open.style.add_modifier.contains(Modifier::DIM));

        assert!(dep_lines(&[], &[]).is_empty());
    }

    /// End-to-end: a task with dependencies in both directions renders them in
    /// the cockpit detail pane and in the tasks-screen Detail popup — blockers,
    /// the task it blocks, and its discovered-from source, each with its title.
    #[test]
    fn detail_views_show_dependencies_in_both_directions() {
        use crate::app::{App, Mode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{Action, DepKind, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let new = |title: &str, priority| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
            milestone: false,
        };
        let closed = store
            .create_task(new("closed blocker", Priority::P2))
            .unwrap();
        store.apply(closed.id, Action::Start).unwrap();
        store.apply(closed.id, Action::Complete(None)).unwrap();
        store.apply(closed.id, Action::Accept).unwrap();
        let source = store.create_task(new("source", Priority::P2)).unwrap();
        // P1 puts the target at the top of the queue, so the cockpit detail
        // pane shows it without moving the selection.
        let target = store.create_task(new("target", Priority::P1)).unwrap();
        let waiting = store.create_task(new("waiting", Priority::P2)).unwrap();
        store
            .add_dep(target.id, closed.id, DepKind::Blocks)
            .unwrap();
        store
            .add_dep(target.id, source.id, DepKind::DiscoveredFrom)
            .unwrap();
        store
            .add_dep(waiting.id, target.id, DepKind::Blocks)
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        let render = |app: &App, terminal: &mut Terminal<TestBackend>| -> String {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };

        let blocked_by = format!("blocked by #{} closed blocker", closed.id);
        let blocks = format!("blocks #{} waiting", waiting.id);
        let discovered = format!("discovered-from #{} source", source.id);

        let cockpit = render(&app, &mut terminal);
        for needle in [&blocked_by, &blocks, &discovered] {
            assert!(
                cockpit.contains(needle.as_str()),
                "cockpit detail pane should show '{needle}': {cockpit}"
            );
        }

        // The same lines in the tasks-screen Detail popup, on the target row.
        alt_screen(&mut app, '2');
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(
            matches!(app.mode, Mode::Detail { task_id, .. } if task_id == target.id),
            "expected the Detail popup on the target task"
        );
        let popup = render(&app, &mut terminal);
        for needle in [&blocked_by, &blocks, &discovered] {
            assert!(
                popup.contains(needle.as_str()),
                "Detail popup should show '{needle}': {popup}"
            );
        }
    }

    /// A `body` event's detail is a whole replaced brief (DESIGN.md §8), so the
    /// history folds it to a marker naming the event that holds it rather than
    /// spilling a superseded body across the pane. Every other kind reads as-is.
    #[test]
    fn history_folds_a_replaced_body_to_a_recovery_marker() {
        let events = vec![
            Event {
                id: 4,
                task_id: Some(62),
                at: "2026-08-01 10:00:00".into(),
                kind: "priority".into(),
                detail: Some("P1".into()),
            },
            Event {
                id: 5,
                task_id: Some(62),
                at: "2026-08-01 10:01:00".into(),
                kind: "body".into(),
                detail: Some("the brief\nline two\nline three".into()),
            },
        ];
        let rendered: Vec<String> = history_lines(&events)
            .iter()
            .map(ratatui::text::Line::to_string)
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("priority") && l.contains("P1"))
        );
        let body = rendered
            .iter()
            .find(|l| l.contains("body"))
            .expect("the body event renders");
        assert!(body.contains("replaced body kept (3 lines)"), "{body}");
        assert!(body.contains("voro show 62 --event 5"), "{body}");
        assert!(!body.contains("line two"), "{body}");
    }

    /// The agent's blocks are parsed as markdown, not printed raw:
    /// bold and inline code are styled rather than showing their markers, and
    /// every visual line of the block — continuations included, at a pane too
    /// narrow to hold the line — carries the cyan gutter.
    #[test]
    fn agent_voice_blocks_parse_markdown_behind_a_gutter() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Color;
        use voro_core::{Action, NewTask, Store};

        let summary = "**Landed** the `parser`.\n\n- rewrote the lexer so every token \
                       carries its span through to the reporter\n- covered it";
        let app = |width: u16| {
            let mut store = Store::open_in_memory().unwrap();
            let p = store.create_project("voro", "/tmp/voro").unwrap();
            store.set_weight(p.id, 3).unwrap();
            let task = store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title: "parse it".into(),
                    body: "acceptance: **it parses**".into(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                    milestone: false,
                })
                .unwrap();
            store.apply(task.id, Action::Start).unwrap();
            store
                .apply(task.id, Action::Complete(Some(summary.into())))
                .unwrap();
            let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            ));
            let app = App::new(store, ctx).unwrap();
            let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
            terminal
                .draw(|f| {
                    draw(f, &app);
                })
                .unwrap();
            terminal.backend().buffer().clone()
        };
        // The detail pane is the right-hand half of the cockpit, so a row of
        // the buffer holds other panes too; the block's rows are the ones with
        // a gutter, and a cell is looked up by its position in the row.
        let rows = |buf: &ratatui::buffer::Buffer, width: u16| -> Vec<String> {
            buf.content()
                .chunks(width as usize)
                .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                .collect()
        };

        // The pane's own border is a `│` too, so the gutter is identified by
        // the column it sits in rather than by the glyph alone.
        let col_of = |row: &str, needle: &str| row.find(needle).map(|b| row[..b].chars().count());
        let char_at = |row: &str, col: usize| row.chars().nth(col).unwrap_or(' ');
        let locate = |rows: &[String], needle: &str| -> (usize, usize) {
            rows.iter()
                .enumerate()
                .find_map(|(y, r)| col_of(r, needle).map(|c| (y, c)))
                .unwrap_or_else(|| panic!("no {needle:?} in {rows:?}"))
        };

        let wide = app(140);
        let wide_rows = rows(&wide, 140);
        let (heading, gutter) = locate(&wide_rows, "│ completion summary:");
        for expected in [
            "│ completion summary:",
            "│ Landed the parser.",
            "│ • rewrote the lexer",
            "│ • covered it",
        ] {
            assert!(
                wide_rows.iter().any(|r| r.contains(expected)),
                "{wide_rows:?}"
            );
        }
        // The bar runs the block's full height — the blank line between the
        // summary and its bullets included.
        for y in heading..heading + 5 {
            assert_eq!(
                char_at(&wide_rows[y], gutter),
                '│',
                "row {y}: {wide_rows:?}"
            );
        }
        // The markers themselves are gone: styling replaced them.
        let all: String = wide_rows.concat();
        assert!(!all.contains("**Landed**"), "{all}");
        assert!(!all.contains("`parser`"), "{all}");

        // The styling lands on the right cells: bold "Landed", cyan "parser",
        // and neither colour bleeding onto the plain text between them.
        let row = heading as u16 + 1;
        let cell = |dx: u16| wide.cell((gutter as u16 + dx, row)).unwrap();
        assert_eq!(cell(0).symbol(), "│");
        assert_eq!(cell(0).fg, Color::Cyan);
        assert!(cell(2).modifier.contains(Modifier::BOLD), "'L' of Landed");
        assert_eq!(cell(2).fg, Color::Reset, "bold, not cyan");
        // "│ Landed the parser." — the 'p' of the inline code is 13 in.
        assert_eq!(cell(13).symbol(), "p");
        assert_eq!(cell(13).fg, Color::Cyan, "inline code stays cyan");
        assert!(!cell(13).modifier.contains(Modifier::BOLD));
        // The body is named, and named in the operator's voice: no gutter, so
        // its text starts in the column the bar occupies above.
        let (task_row, task_col) = locate(&wide_rows, "task:");
        assert_eq!(task_col, gutter, "{wide_rows:?}");

        // Narrow enough that the bullet cannot fit on one row: the
        // continuation keeps the gutter, which is what pre-wrapping buys.
        let narrow_rows = rows(&app(60), 60);
        let (bullet, ncol) = locate(&narrow_rows, "│ • rewrote the lexer");
        assert!(
            !narrow_rows[bullet].contains("reporter"),
            "the line was meant to be too long to fit: {narrow_rows:?}"
        );
        assert_eq!(
            char_at(&narrow_rows[bullet + 1], ncol),
            '│',
            "continuation lost the gutter: {narrow_rows:?}"
        );
        let (tail, _) = locate(&narrow_rows, "reporter");
        assert!(tail > bullet && tail <= task_row + 6, "{narrow_rows:?}");
    }
}
