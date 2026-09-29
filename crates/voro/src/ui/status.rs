//! The status region and the keys (DESIGN.md §9): the status line, the store
//! footer, the key line's hints, and the `?` key map.

use super::popup_area;
use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use voro_core::Store;

use super::milestones;
use crate::app::{App, Screen};

/// How many lines the status region needs at this frame size (DESIGN.md §9).
/// The key line is always one; a message takes as many as it wraps to, up to
/// half the screen — a message Voro cannot fit in half a terminal is past the
/// point where growing the region further helps.
pub(super) fn status_height(app: &App, area: Rect) -> u16 {
    let Some(msg) = &app.status else {
        return 1;
    };
    let needed = wrap_status(msg, area.width).len() as u16;
    needed.clamp(1, (area.height / 2).max(1))
}

/// Greedy word wrap for the status line. Voro's errors end with the actionable
/// half — the key to press instead — so truncating them at the pane width hides
/// exactly the part worth reading (DESIGN.md §9). Wrapping here rather than
/// through `Wrap` keeps the drawn line count knowable before the layout is
/// split, so the region can be sized to the message. A word wider than the pane
/// gets its own line and truncates there, having nowhere else to go.
fn wrap_status(msg: &str, width: u16) -> Vec<String> {
    let width = width.max(1) as usize;
    let mut lines = Vec::new();
    for paragraph in msg.split('\n') {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let fits = Span::raw(current.as_str()).width() + 1 + Span::raw(word).width() <= width;
            if !current.is_empty() && !fits {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    lines
}

pub(super) fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    // A red status message overrides the key line, as before, and owns the row
    // whole — the indicator below would be competing with wrapped text for a
    // right margin that moves (DESIGN.md §9). The message is gone on the next
    // keystroke; the store is not going anywhere.
    if let Some(msg) = &app.status {
        let lines: Vec<Line> = wrap_status(msg, area.width)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Red))))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, label)) in key_hints(app).into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ", Style::new().dim()));
        }
        spans.push(Span::styled(key, Style::new().bold()));
        spans.push(Span::styled(format!(" {label}"), Style::new().dim()));
    }
    // The store, right-aligned the way the header right-aligns its counts, so
    // the key line keeps the left margin it has always started at. The line is
    // measured first and the indicator takes what is left over: the key line's
    // slot budget is fixed and documented (DESIGN.md §9), a store path's length
    // is not, so the occupant that cannot be bounded is the one that yields. On
    // the operator's own store there is no indicator, the reserved width is
    // zero, and the key line gets the row back byte for byte.
    let keys_line = Line::from(spans);
    let leftover = area
        .width
        .saturating_sub(keys_line.width() as u16)
        .saturating_sub(1);
    let indicator = db_indicator(app.db_path(), leftover);
    let reserved = indicator
        .as_deref()
        .map_or(0, |text| Span::raw(text).width() as u16 + 1);
    let [keys, store] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(reserved)]).areas(area);
    frame.render_widget(keys_line, keys);
    if let Some(text) = indicator {
        frame.render_widget(
            Line::from(vec![Span::raw(" "), Span::styled(text, Style::new().dim())]),
            store,
        );
    }
}

/// The store to name in the footer within `budget` columns — the ones the key
/// line left — or `None` when it is the operator's own and there is nothing to
/// say, or when not even the store's filename fits (DESIGN.md §9).
///
/// The comparison is against [`Store::production_db_path`] rather than
/// [`Store::default_db_path`], which is the crux of it: the default is `dev.db`
/// for a `target/` build, so an indicator keyed on it would stay silent on a dev
/// store — the very case that asks the question. This is the rule dispatch's
/// `--db` flag and `voro seed` already follow (§5).
fn db_indicator(db_path: &Path, budget: u16) -> Option<String> {
    if db_path == Store::production_db_path() {
        return None;
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    shorten_store_path(db_path, home.as_deref(), budget)
}

/// A store path cut down to what identifies it inside `budget` columns: `~` for
/// the home directory, then leading directories given up whole for a `…`, since
/// the filename and its parent are the half that names the store and the path to
/// them is the half that does not. The ladder ends at the bare filename —
/// `dev.db` says "not your store" in six columns — and below that at `None`,
/// because a fragment of a name identifies nothing and the columns are the key
/// line's to have back. Absence is not neutral here: an empty right margin means
/// the operator's own store, so the indicator would rather say nothing than say
/// something unreadable.
fn shorten_store_path(path: &Path, home: Option<&Path>, budget: u16) -> Option<String> {
    let text = match home
        .filter(|home| !home.as_os_str().is_empty())
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    };
    let budget = usize::from(budget);
    if text.chars().count() <= budget {
        return Some(text);
    }
    let parts: Vec<&str> = text.split('/').collect();
    for first in 1..parts.len() {
        let candidate = format!("…/{}", parts[first..].join("/"));
        if candidate.chars().count() <= budget {
            return Some(candidate);
        }
    }
    let name = parts.last().copied().unwrap_or_default();
    (!name.is_empty() && name.chars().count() <= budget).then(|| name.to_string())
}

/// Whether the selection is a brief refine can still rewrite — a proposal or a
/// ready task (DESIGN.md §6).
fn selection_is_refinable(app: &App) -> bool {
    app.selected_task_id()
        .is_some_and(|id| app.is_refinable(id))
}

/// Every slot the current screen's key line can hold, each flagged with whether
/// this selection earns it. [`key_hints`] is this list filtered, so the two can
/// never disagree about which keys the line advertises — which is what lets the
/// drift test check the whole set against [`key_map`] from one App.
fn hint_candidates(app: &App) -> Vec<(&'static str, &'static str, bool)> {
    // `enter_hint` yields "⏎ <verb>"; split the glyph from the verb so the
    // glyph renders as the bold key and the verb as the dim label.
    let enter = app.enter_hint().and_then(|h| h.split_once(' '));
    let enter = ("⏎", enter.map_or("act", |(_, verb)| verb), enter.is_some());
    match app.screen {
        Screen::Cockpit => vec![
            enter,
            ("d/D", "dispatch", app.selected_can_dispatch()),
            ("r/R", "refine", selection_is_refinable(app)),
            ("C", "cancel refine", app.selected_is_refining()),
            ("s", "state", true),
            ("!", "deep", app.selected_can_go_deep()),
            ("w", "wait", app.selected_can_hand_off()),
            ("o", "open", app.selected_has_a_diff()),
            ("g", "PR", app.selected_is_in_review()),
            ("a/A", "message", app.selected_can_message()),
            ("n/N", "new", true),
            ("e", "edit", true),
            ("?", "keys", true),
            ("tab", "tasks", true),
            ("q", "quit", true),
        ],
        Screen::Tasks => vec![
            enter,
            ("w", "wait", app.selected_can_hand_off()),
            ("o", "open", app.selected_has_a_diff()),
            ("g", "PR", app.selected_is_in_review()),
            ("r/R", "refine", selection_is_refinable(app)),
            ("C", "cancel refine", app.selected_is_refining()),
            ("s", "state", true),
            ("!", "deep", app.selected_can_go_deep()),
            ("a/A", "message", app.selected_can_message()),
            ("n/N", "new", true),
            ("e", "edit", true),
            ("?", "keys", true),
            ("tab", "projects", true),
            ("q", "quit", true),
        ],
        // `a`/`A` and the rest of this screen's uppercase keys are unrelated
        // actions sharing a letter, not variants of one action, so they keep
        // their own slots.
        Screen::Projects => vec![
            ("0-5", "weight", true),
            ("r", "rename", true),
            ("a", "add", true),
            ("A", "archive", true),
            ("d", "delete", true),
            ("v", "viewer", true),
            ("?", "keys", true),
            ("tab", "config", true),
            ("q", "quit", true),
        ],
        Screen::Config => {
            // Deleting is the viewer list's own operation, so it appears only
            // while the selection is in that list; editing is every row's.
            let on_viewer = matches!(
                app.selected_config_row(),
                Some(crate::app::ConfigRow::Viewer(_))
            );
            // While the gate holds (DESIGN.md §9) `tab` cycles the shorter
            // Projects ↔ Config ring, so the slot has to name where it lands.
            let next = if app.projects.is_empty() {
                "projects"
            } else {
                "milestones"
            };
            vec![
                enter,
                ("a", "add viewer", true),
                ("d", "delete", on_viewer),
                ("?", "keys", true),
                ("tab", next, true),
                ("q", "quit", true),
            ]
        }
        Screen::Milestones => milestones::hint_candidates(app, enter),
    }
}

/// The contextual per-screen key line (DESIGN.md §9): the actions that apply on
/// the current screen and selection, as key/label pairs the caller renders
/// key-bold, label-dim. A lowercase/uppercase pair of one action takes a single
/// slot keyed on the pair (`d/D dispatch`), with the uppercase variant's gloss
/// left to `?`. The line carries what changes a task's state or destiny;
/// navigation, display toggles and browsing conveniences live in the key map
/// only, so `?` is always present. Selection-only actions drop out when there
/// is nothing to act on, and the refine keys appear only on a task whose body is
/// still a brief — a proposal or a ready task.
pub(super) fn key_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    hint_candidates(app)
        .into_iter()
        .filter(|(_, _, shown)| *shown)
        .map(|(key, label, _)| (key, label))
        .collect()
}

/// The lowercase/uppercase pairs the key line renders as one slot (DESIGN.md
/// §9). This map is the only place the uppercase variants are glossed, so the
/// three lines are worded to one shape, the one the case convention asks for:
/// the lowercase acts headlessly and stays in the TUI, the uppercase names the
/// surface it opens.
const DISPATCH_KEYS: [(&str, &str); 2] = [
    ("d", "dispatch to the resolved agent"),
    ("D", "dispatch, choosing the agent"),
];

const REFINE_KEYS: [(&str, &str); 2] = [
    ("r", "refine a brief from a note, headless"),
    ("R", "refine a brief in an agent session"),
];

const NEW_KEYS: [(&str, &str); 2] = [
    ("n", "new task, proposed headless"),
    ("N", "new task, planned in an agent session"),
];

/// The uppercase keys DESIGN.md §9 names as standing outside the case
/// convention, because none is the shifted half of a pair: `C` and the projects
/// screen's `A` share a letter with an unrelated action, and `J`/`K` scroll a
/// pane that has no selection to scroll with — the cockpit's card and the Config
/// screen's agents. Every other uppercase binding has to be the interactive half
/// of a pair, which the test below enforces screen by screen.
#[cfg(test)]
const CASE_EXCEPTIONS: [(Screen, &str); 8] = [
    (Screen::Cockpit, "C"),
    (Screen::Cockpit, "J"),
    (Screen::Cockpit, "K"),
    (Screen::Tasks, "C"),
    (Screen::Tasks, "M"),
    (Screen::Projects, "A"),
    (Screen::Config, "J"),
    (Screen::Config, "K"),
];

const MESSAGE_KEYS: [(&str, &str); 2] = [
    ("a", "message the task's session, headless"),
    ("A", "message in person — attach or resume"),
];

/// A titled group of key/label pairs in the key map.
pub(super) type KeySection = (&'static str, Vec<(&'static str, &'static str)>);

/// A screen's complete key map (DESIGN.md §9), grouped into actions,
/// navigation, and screen switching. Unlike [`key_hints`] it is ungated by
/// selection — it is the map, so it lists every key the screen binds, including
/// the ones the line has no room to advertise. `no_projects` is the one thing
/// it does gate on, because a map that advertised a jump the gate refuses would
/// be promising a refusal.
fn key_map(screen: Screen, no_projects: bool) -> Vec<KeySection> {
    let pairs = |set: [(&'static str, &'static str); 2]| set.into_iter();
    let screens = |current: &'static str| {
        let mut keys = vec![("tab", current)];
        if !no_projects {
            keys.push(("alt-1", "cockpit"));
            keys.push(("alt-2", "tasks"));
        }
        keys.push(("alt-3", "projects"));
        keys.push(("alt-4", "config"));
        if !no_projects {
            keys.push(("alt-5", "milestones"));
        }
        ("Screens", keys)
    };
    match screen {
        Screen::Cockpit => {
            let mut actions = vec![("⏎", "act on the selected row")];
            actions.extend(pairs(DISPATCH_KEYS));
            actions.extend(pairs(REFINE_KEYS));
            actions.extend([
                ("0-3", "set the task's priority"),
                ("s", "change state"),
                ("!", "toggle deep — the agent's best model"),
                ("c", "link and unlink documents"),
                ("m", "attach to or detach from milestones"),
                ("C", "cancel a refine in flight"),
                ("x", "fold the score decomposition in"),
                ("h", "fold the task's history in"),
                ("o", "open the local diff in a viewer"),
                ("g", "open the PR on GitHub"),
            ]);
            actions.extend(pairs(MESSAGE_KEYS));
            actions.extend([
                ("u", "nudge capped sessions past their reset"),
                ("l", "page the session log"),
                ("w", "hand a review task off, to wait"),
            ]);
            actions.extend(pairs(NEW_KEYS));
            actions.push(("ctrl-n", "new task, written by hand in $EDITOR"));
            actions.push(("e", "edit the selected task"));
            vec![
                ("Actions", actions),
                (
                    "Navigation",
                    vec![
                        ("j/k", "move the selection"),
                        ("J/K", "scroll the card"),
                        ("PgUp/PgDn", "page the card"),
                        ("ctrl-r", "refresh"),
                        ("?", "this key map"),
                        ("q", "quit"),
                    ],
                ),
                screens("next screen"),
            ]
        }
        Screen::Tasks => {
            let mut actions = vec![("⏎", "open the task's detail, or a fold")];
            actions.extend(pairs(DISPATCH_KEYS));
            actions.extend(pairs(REFINE_KEYS));
            actions.extend([
                ("0-3", "set the task's priority"),
                ("s", "change state"),
                ("!", "toggle deep — the agent's best model"),
                ("c", "link and unlink documents"),
                ("m", "attach to or detach from milestones"),
                ("C", "cancel a refine in flight"),
                ("o", "open the local diff in a viewer"),
                ("g", "open the PR on GitHub"),
            ]);
            actions.extend(pairs(MESSAGE_KEYS));
            actions.extend([
                ("u", "nudge capped sessions past their reset"),
                ("l", "page the session log"),
                ("w", "hand a review task off, to wait"),
            ]);
            actions.extend(pairs(NEW_KEYS));
            actions.push(("ctrl-n", "new task, written by hand in $EDITOR"));
            actions.push(("e", "edit the selected task"));
            vec![
                ("Actions", actions),
                (
                    "Navigation",
                    vec![
                        ("j/k", "move the selection"),
                        ("M", "group by milestone"),
                        ("t", "tree by blockers"),
                        ("space", "toggle a tree fold"),
                        ("ctrl-r", "refresh"),
                        ("?", "this key map"),
                        ("q", "quit"),
                    ],
                ),
                screens("next screen"),
            ]
        }
        Screen::Milestones => milestones::key_map(screens("next screen")),
        Screen::Projects => vec![
            (
                "Actions",
                vec![
                    ("0-5", "set the project's weight"),
                    ("r", "rename or re-path the project"),
                    ("a", "add a project"),
                    ("A", "archive or unarchive the project"),
                    ("d", "delete the project — only when it is empty"),
                    ("v", "pick the project's viewer"),
                ],
            ),
            (
                "Navigation",
                vec![
                    ("j/k", "move the selection"),
                    ("?", "this key map"),
                    ("q", "quit"),
                ],
            ),
            screens("next screen"),
        ],
        Screen::Config => vec![
            (
                "Actions",
                vec![
                    ("⏎/e", "edit the selected setting or viewer"),
                    ("a", "add a viewer"),
                    ("d", "delete the selected viewer"),
                ],
            ),
            (
                "Navigation",
                vec![
                    ("j/k", "move the selection"),
                    ("J/K", "scroll the agents pane"),
                    ("PgUp/PgDn", "page the agents pane"),
                    ("?", "this key map"),
                    ("q", "quit"),
                ],
            ),
            screens("next screen"),
        ],
    }
}

/// The widest key and the widest gloss in a column's sections — the two halves
/// of its natural width.
fn key_map_widths(sections: &[KeySection]) -> (usize, usize) {
    let entries = || sections.iter().flat_map(|(_, entries)| entries.iter());
    let width = |f: fn(&(&'static str, &'static str)) -> &'static str| {
        entries().map(|e| f(e).chars().count()).max().unwrap_or(0)
    };
    (width(|(key, _)| key), width(|(_, label)| label))
}

/// The key map's rows for one column, keys right-aligned to the column's widest
/// and glosses held to `label_w`, over-long ones ending in an ellipsis.
fn key_map_column(sections: &[KeySection], label_w: usize) -> Vec<Vec<Span<'static>>> {
    let (key_w, _) = key_map_widths(sections);
    let mut rows: Vec<Vec<Span<'static>>> = Vec::new();
    for (i, (title, entries)) in sections.iter().enumerate() {
        if i > 0 {
            rows.push(Vec::new());
        }
        rows.push(vec![Span::styled(*title, Style::new().bold())]);
        for (key, label) in entries {
            let label = if label.chars().count() <= label_w {
                (*label).to_string()
            } else {
                let kept: String = label.chars().take(label_w.saturating_sub(1)).collect();
                format!("{kept}…")
            };
            rows.push(vec![
                Span::styled(format!("{key:>key_w$}  "), Style::new().bold()),
                Span::styled(label, Style::new().dim()),
            ]);
        }
    }
    rows
}

/// The `?` overlay: the current screen's whole key map, actions in one column
/// and navigation over screen switching in the other. Both dimensions are the
/// terminal's rather than the content's (DESIGN.md §9) — the Actions glosses
/// give up width before the Navigation column beside them clips, and what will
/// not fit the height moves to a further page `tab` turns to — so every entry
/// stays reachable however many keys the map grows.
pub(super) fn draw_key_map(frame: &mut Frame, app: &App, page: usize) {
    const GAP: usize = 3;
    /// Under this a gloss says nothing whatever it does, so the Actions column
    /// stops giving width up and the overlay clips as any pane does.
    const MIN_LABEL: usize = 8;

    let sections = key_map(app.screen, app.projects.is_empty());
    let (actions, rest) = sections.split_at(1);
    let (key_l, natural_l) = key_map_widths(actions);
    let (key_r, label_r) = key_map_widths(rest);
    let chrome = key_l + 2 + GAP + key_r + 2 + 2;
    let avail = frame.area().width as usize;
    let label_l = if chrome + natural_l + label_r > avail {
        avail.saturating_sub(chrome + label_r).max(MIN_LABEL)
    } else {
        natural_l
    };
    let (left, right) = (
        key_map_column(actions, label_l),
        key_map_column(rest, label_r),
    );
    let width_of =
        |row: &Vec<Span<'static>>| -> usize { row.iter().map(|s| s.content.chars().count()).sum() };
    let left_w = left.iter().map(width_of).max().unwrap_or(0);
    let right_w = right.iter().map(width_of).max().unwrap_or(0);

    let lines: Vec<Line<'static>> = (0..left.len().max(right.len()))
        .map(|i| {
            let mut spans = left.get(i).cloned().unwrap_or_default();
            if let Some(row) = right.get(i) {
                let pad = (left_w + GAP).saturating_sub(width_of(&spans));
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(row.iter().cloned());
            }
            Line::from(spans)
        })
        .collect();

    // Pages are as few as the height allows and then evenly filled, so a map
    // one row too tall splits down the middle rather than stranding that row
    // alone on a second page; the box keeps one height throughout, so turning
    // a page does not resize the overlay under the reader.
    let box_h = (frame.area().height as usize).saturating_sub(2).max(1);
    let pages = lines.len().div_ceil(box_h).max(1);
    let per_page = lines.len().div_ceil(pages).max(1);
    let page = page % pages;
    let shown: Vec<Line<'static>> = lines
        .into_iter()
        .skip(page * per_page)
        .take(per_page)
        .collect();

    let screen = match app.screen {
        Screen::Cockpit => "cockpit",
        Screen::Tasks => "tasks",
        Screen::Projects => "projects",
        Screen::Config => "config",
        Screen::Milestones => "milestones",
    };

    let title = if pages > 1 {
        format!(
            "Keys — {screen} — {}/{pages}, tab pages — any key closes",
            page + 1
        )
    } else {
        format!("Keys — {screen} — any key closes")
    };
    let width = (left_w + GAP + right_w + 2) as u16;
    let area = popup_area(frame, width, per_page as u16 + 2);
    let para = Paragraph::new(shown).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Mode;
    use crate::ui::cockpit::tests::app_in_review_with_everything;
    use crate::ui::draw;
    use crate::ui::tests::{GH_REFUSAL, alt_screen, app_with_status, screen_text};
    use voro_core::{LivenessSource, Priority, TaskState};

    /// The key map may not advertise a jump the gate would refuse (DESIGN.md
    /// §9): with no project registered the Screens section drops `alt-1` and
    /// `alt-2` and `alt-5`, and gets them back the moment one exists.
    #[test]
    fn the_key_map_hides_the_gated_screen_jumps() {
        for screen in Screen::ALL {
            let jumps = |no_projects: bool| -> Vec<&'static str> {
                key_map(screen, no_projects)
                    .into_iter()
                    .flat_map(|(_, entries)| entries)
                    .map(|(key, _)| key)
                    .filter(|key| key.starts_with("alt-"))
                    .collect()
            };
            assert_eq!(jumps(true), vec!["alt-3", "alt-4"], "{screen:?}");
            assert_eq!(
                jumps(false),
                vec!["alt-1", "alt-2", "alt-3", "alt-4", "alt-5"],
                "{screen:?}"
            );
        }
    }

    #[test]
    fn wrap_status_breaks_on_words_and_keeps_every_one() {
        let lines = wrap_status(GH_REFUSAL, 40);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(
                line.chars().count() <= 40,
                "{line:?} is wider than the pane"
            );
        }
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            GH_REFUSAL.split_whitespace().collect::<Vec<_>>(),
        );
    }

    /// A word with nowhere to break — a long path — takes its own line rather
    /// than pushing the rest of the message off the pane.
    #[test]
    fn wrap_status_gives_an_overlong_word_its_own_line() {
        let lines = wrap_status("at /a/very/long/path/that/exceeds/the/pane use `o`", 12);
        assert_eq!(
            lines,
            vec!["at", "/a/very/long/path/that/exceeds/the/pane", "use `o`"]
        );
    }

    /// The bug this fixes: at an ordinary terminal width the closing half of an
    /// error — the part naming what to press instead — was cut off. The cockpit
    /// now grows its status region to fit the whole message.
    #[test]
    fn cockpit_status_wraps_instead_of_truncating() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let app = app_with_status(GH_REFUSAL, 1, 0);
        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();

        let normalised = GH_REFUSAL.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            screen_text(&terminal).contains(&normalised),
            "the whole message should be on screen, got:\n{}",
            screen_text(&terminal)
        );
    }

    /// A cockpit whose queue and running strip are both at their tallest still
    /// shows the message whole — three times the length of the longest real
    /// one, on a narrow screen — the panes above it giving up the rows.
    #[test]
    fn a_crowded_cockpit_still_shows_the_whole_message() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let long = format!("{GH_REFUSAL} {GH_REFUSAL} {GH_REFUSAL}");
        let app = app_with_status(&long, 20, 12);
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();

        let normalised = long.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            screen_text(&terminal).contains(&normalised),
            "the whole message should survive a full cockpit, got:\n{}",
            screen_text(&terminal)
        );
    }

    /// The same holds on every other screen, since each splits its own layout.
    #[test]
    fn every_screen_wraps_a_long_status() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let normalised = GH_REFUSAL.split_whitespace().collect::<Vec<_>>().join(" ");
        for (key, screen) in [
            ('2', Screen::Tasks),
            ('3', Screen::Projects),
            ('4', Screen::Config),
        ] {
            let mut app = app_with_status(GH_REFUSAL, 1, 0);
            alt_screen(&mut app, key);
            assert_eq!(app.screen, screen);
            // Switching screens is free to clear the message; re-arm it.
            app.status = Some(GH_REFUSAL.into());

            let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
            terminal
                .draw(|f| {
                    draw(f, &app);
                })
                .unwrap();
            assert!(
                screen_text(&terminal).contains(&normalised),
                "{screen:?} truncated the message:\n{}",
                screen_text(&terminal)
            );
        }
    }

    /// A short message and the key line still occupy one row, so the panes keep
    /// the space they had whenever there is nothing long to say.
    #[test]
    fn status_region_stays_one_line_for_short_messages() {
        let area = Rect::new(0, 0, 110, 24);
        let mut app = app_with_status("task 9 has no session on record", 1, 0);
        assert_eq!(status_height(&app, area), 1);
        app.status = None;
        assert_eq!(status_height(&app, area), 1);
    }

    /// The region grows only to half the screen: a terminal too small to hold
    /// the message is better served by keeping its lists than by burying them.
    #[test]
    fn status_region_stops_at_half_the_screen() {
        let app = app_with_status(&GH_REFUSAL.repeat(20), 1, 0);
        assert_eq!(status_height(&app, Rect::new(0, 0, 40, 24)), 12);
        assert_eq!(status_height(&app, Rect::new(0, 0, 40, 3)), 1);
    }

    /// The footer names the store only when it is not the operator's own
    /// (DESIGN.md §9), and the comparison is against the production path rather
    /// than the default one — so a dev build, whose default *is* `dev.db`, says
    /// so instead of staying silent (§5).
    #[test]
    fn the_footer_names_every_store_but_the_operator_s() {
        assert_eq!(db_indicator(&Store::production_db_path(), 110), None);
        assert!(
            db_indicator(&Store::dev_db_path(), 110).is_some_and(|text| text.ends_with("dev.db")),
            "a dev build has to name dev.db, got {:?}",
            db_indicator(&Store::dev_db_path(), 110)
        );
        assert_eq!(
            db_indicator(Path::new("/tmp/scratch/voro.db"), 110),
            Some("/tmp/scratch/voro.db".to_string())
        );
    }

    /// A store under the home directory is shown against `~`, which is both
    /// shorter and how the operator refers to it.
    #[test]
    fn a_store_under_home_is_shown_against_a_tilde() {
        assert_eq!(
            shorten_store_path(
                Path::new("/home/op/.local/share/voro/dev.db"),
                Some(Path::new("/home/op")),
                110
            )
            .as_deref(),
            Some("~/.local/share/voro/dev.db")
        );
        // No home to compare against leaves the path as it is.
        assert_eq!(
            shorten_store_path(Path::new("/srv/voro/voro.db"), None, 110).as_deref(),
            Some("/srv/voro/voro.db")
        );
    }

    /// The budget is whatever the key line did not want, so the same store
    /// renders whole beside a short line and gives up its leading directories
    /// beside a long one.
    #[test]
    fn the_store_shortens_into_the_columns_the_key_line_left() {
        let long = Path::new("/home/op/very/deeply/nested/scratch/area/voro.db");
        let home = Some(Path::new("/home/op"));
        assert_eq!(
            shorten_store_path(long, home, 41).as_deref(),
            Some("~/very/deeply/nested/scratch/area/voro.db")
        );
        assert_eq!(
            shorten_store_path(long, home, 40).as_deref(),
            Some("…/deeply/nested/scratch/area/voro.db")
        );
        assert_eq!(
            shorten_store_path(long, home, 20).as_deref(),
            Some("…/area/voro.db")
        );
    }

    /// The bottom of the ladder: the bare filename, which still says "not your
    /// store", and then nothing at all — a fragment of a name identifies no
    /// store, and an empty right margin is the key line's to have back.
    #[test]
    fn a_store_with_no_room_left_shows_its_filename_or_nothing() {
        let long = Path::new("/home/op/very/deeply/nested/scratch/area/voro.db");
        let home = Some(Path::new("/home/op"));
        assert_eq!(
            shorten_store_path(long, home, 7).as_deref(),
            Some("voro.db"),
            "the filename alone fits and is worth saying"
        );
        // One column more and the `…` marking what was dropped fits too.
        assert_eq!(
            shorten_store_path(long, home, 9).as_deref(),
            Some("…/voro.db")
        );
        assert_eq!(shorten_store_path(long, home, 6), None);
        assert_eq!(shorten_store_path(long, home, 0), None);
        // A name too long for the row is never cut mid-word: `…g-store-name.db`
        // names nothing.
        assert_eq!(
            shorten_store_path(Path::new("/tmp/a-very-long-store-name.db"), None, 20),
            None
        );
    }

    /// End-to-end: the indicator reaches the footer of a drawn screen, dim and
    /// right of the key line, without costing the region a row.
    #[test]
    fn the_footer_carries_the_store_on_screen() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_with_status("", 1, 0);
        app.status = None;
        // Wide enough that the cockpit's key line and `dummy_ctx`'s store both
        // fit: the indicator never takes a column the line wanted.
        let mut terminal = Terminal::new(TestBackend::new(140, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let text = screen_text(&terminal);
        // `dummy_ctx`'s store is not the operator's, so the row names it.
        assert!(text.contains("/nonexistent/voro.db"), "{text}");
        assert!(
            text.contains("⏎ act · d/D dispatch"),
            "the key line still starts the row: {text}"
        );
        assert!(text.contains("q quit"), "and still ends it: {text}");

        // A message owns the row alone; nothing competes with it.
        app.status = Some("task 9 has no session on record".into());
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("task 9 has no session on record"), "{text}");
        assert!(!text.contains("/nonexistent/voro.db"), "{text}");
    }

    /// The row the key line spends its whole budget on — a `review` task with a
    /// branch, a PR and a session, on both screens that show it — keeps every
    /// slot at an ordinary width. Whatever the store does with what is left, it
    /// may not cost the line a key.
    #[test]
    fn the_widest_key_line_keeps_its_last_slots() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_in_review_with_everything();
        for (screen, first) in [(Screen::Cockpit, "⏎ review"), (Screen::Tasks, "⏎ view")] {
            app.screen = screen;
            let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
            terminal
                .draw(|f| {
                    draw(f, &app);
                })
                .unwrap();
            let text = screen_text(&terminal);
            assert!(
                text.contains(first),
                "{screen:?} lost its first slot: {text}"
            );
            for slot in ["o open", "g PR", "? keys", "q quit"] {
                assert!(text.contains(slot), "{screen:?} lost `{slot}`: {text}");
            }
        }
    }

    /// The cockpit key line only advertises the selection-only actions while a
    /// task is selected — with an empty queue there is nothing for them to act
    /// on, so they drop out.
    #[test]
    fn cockpit_key_line_drops_the_selection_only_actions_without_a_selection() {
        use crate::app::App;
        use voro_core::{NewTask, Store};

        let ctx = || {
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            ))
        };

        let mut empty = App::new(Store::open_in_memory().unwrap(), ctx()).unwrap();
        // A project-less database opens on Projects (DESIGN.md §9); the cockpit
        // this test is about is the one reached by tabbing back to it.
        empty.screen = Screen::Cockpit;
        assert!(empty.selected_task_id().is_none());
        let labels: Vec<&str> = key_hints(&empty).iter().map(|(_, l)| *l).collect();
        for dropped in ["dispatch", "deep"] {
            assert!(
                !labels.contains(&dropped),
                "empty cockpit should not advertise {dropped}: {labels:?}"
            );
        }

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "a task".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();
        let selected = App::new(store, ctx()).unwrap();
        assert!(selected.selected_task_id().is_some());
        let labels: Vec<&str> = key_hints(&selected).iter().map(|(_, l)| *l).collect();
        for shown in ["dispatch", "deep"] {
            assert!(
                labels.contains(&shown),
                "cockpit with a selection should advertise {shown}: {labels:?}"
            );
        }
    }

    /// The review cluster's gating (DESIGN.md §9): `o` is advertised wherever
    /// there is a diff to look at — a `review` or `running` task carrying a
    /// branch — `g` only on `review`, where the PR is the review, and `!`
    /// nowhere past dispatch. A `ready` row, which has none of the three
    /// states, shows the mirror image.
    #[test]
    fn the_key_line_advertises_the_review_keys_only_where_they_act() {
        use crate::app::App;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = |title: &str| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
            milestone: false,
        };
        store.create_task(task("ready to go")).unwrap();
        let running = store.create_task(task("under way")).unwrap();
        store.apply(running.id, Action::Start).unwrap();
        store
            .set_branch(running.id, Some("feat/under-way"))
            .unwrap();
        let reviewed = store.create_task(task("in review")).unwrap();
        store.apply(reviewed.id, Action::Start).unwrap();
        store.apply(reviewed.id, Action::Complete(None)).unwrap();
        store
            .set_branch(reviewed.id, Some("feat/in-review"))
            .unwrap();

        let mut app = App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();
        app.screen = Screen::Tasks;

        let keys_for = |app: &mut App, title: &str| {
            app.tasks_sel = app
                .all
                .iter()
                .position(|row| row.task.title == title)
                .unwrap_or_else(|| panic!("no {title:?} row"));
            key_hints(app)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };

        let ready = keys_for(&mut app, "ready to go");
        assert!(!ready.contains(&"o"), "{ready:?}");
        assert!(!ready.contains(&"g"), "{ready:?}");
        assert!(ready.contains(&"!"), "{ready:?}");

        let running = keys_for(&mut app, "under way");
        assert!(running.contains(&"o"), "{running:?}");
        assert!(!running.contains(&"g"), "{running:?}");
        assert!(running.contains(&"!"), "{running:?}");

        let review = keys_for(&mut app, "in review");
        assert!(review.contains(&"o"), "{review:?}");
        assert!(review.contains(&"g"), "{review:?}");
        assert!(!review.contains(&"!"), "{review:?}");
    }

    /// The other half of that gating: both review keys are earned by having
    /// something to show, not by the state alone (DESIGN.md §9). A task whose
    /// whole product is its summary reaches `review` with no branch and no PR,
    /// and there the line advertises neither — the same row whose next-action
    /// is *accept* rather than *pr*. A tracked PR earns `g` back on its own;
    /// `o` needs the branch it diffs.
    #[test]
    fn the_review_keys_need_a_branch_or_a_pr_to_show_for() {
        use crate::app::App;
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = |title: &str| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state: TaskState::Ready,
            agent: None,
            human: false,
            deep: false,
            milestone: false,
        };
        let reviewed = |store: &mut Store, title: &str| {
            let t = store.create_task(task(title)).unwrap();
            store.apply(t.id, Action::Start).unwrap();
            store.apply(t.id, Action::Complete(None)).unwrap();
            t.id
        };
        let bare = reviewed(&mut store, "nothing to show");
        let tracked = reviewed(&mut store, "a PR and no branch");
        store
            .set_pr(tracked, Some("https://github.com/o/r/pull/1"))
            .unwrap();
        // A dispatch that has not named a branch yet has nothing to diff either.
        let running = store.create_task(task("under way")).unwrap();
        store.apply(running.id, Action::Start).unwrap();

        let mut app = App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();

        // Both lines read the same selection, so each screen is walked to the
        // row that names the task rather than indexed into directly.
        let keys_for = |app: &mut App, id: i64| {
            let rows = match app.screen {
                Screen::Cockpit => app.cockpit_rows.len(),
                _ => app.all.len(),
            };
            let found = (0..rows).any(|i| {
                match app.screen {
                    Screen::Cockpit => app.cockpit_sel = i,
                    _ => app.tasks_sel = i,
                }
                app.selected_task_id() == Some(id)
            });
            assert!(found, "{:?} has no row for task {id}", app.screen);
            key_hints(app)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };

        for screen in [Screen::Cockpit, Screen::Tasks] {
            app.screen = screen;
            let keys = keys_for(&mut app, bare);
            assert!(!keys.contains(&"o"), "{screen:?}: {keys:?}");
            assert!(!keys.contains(&"g"), "{screen:?}: {keys:?}");
            // The rest of the review row is untouched — the slots freed are
            // exactly the two that could not act.
            assert!(keys.contains(&"w"), "{screen:?}: {keys:?}");

            let keys = keys_for(&mut app, tracked);
            assert!(keys.contains(&"g"), "{screen:?}: {keys:?}");
            assert!(!keys.contains(&"o"), "{screen:?}: {keys:?}");

            let keys = keys_for(&mut app, running.id);
            assert!(!keys.contains(&"o"), "{screen:?}: {keys:?}");
        }
    }

    /// A lowercase/uppercase pair of one action takes a single slot keyed on
    /// the pair, and the line stays short enough to scan: eleven slots or fewer
    /// on every row of a queue holding one of each kind (DESIGN.md §9). Eleven
    /// rather than ten because a `review` row carries the whole review cluster
    /// — `⏎`, `w`, `o`, `g` — the one moment all four are live options; `!`
    /// dropping off that row is what keeps even eleven reachable.
    #[test]
    fn the_key_line_pairs_its_slots_and_stays_at_eleven() {
        use crate::app::App;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{Action, NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        // A proposal earns the refine slot and a review task the hand-off slot;
        // between them every conditional cockpit slot is covered.
        let task = |title: &str, state: TaskState| NewTask {
            project_id: p.id,
            repo_id: None,
            title: title.into(),
            body: String::new(),
            priority: Priority::P2,
            state,
            agent: None,
            human: false,
            deep: false,
            milestone: false,
        };
        store
            .create_task(task("a proposal", TaskState::Proposed))
            .unwrap();
        store
            .create_task(task("ready to go", TaskState::Ready))
            .unwrap();
        // The review task carries a session and a branch, which is the worst
        // case for the line: it earns the hand-off slot and the message slot at
        // once, and the branch is what earns it the two review slots.
        let reviewed = store
            .create_task(task("in review", TaskState::Ready))
            .unwrap();
        store
            .record_dispatch(reviewed.id, "claude", None, LivenessSource::Pid, None)
            .unwrap();
        store.apply(reviewed.id, Action::Complete(None)).unwrap();
        store
            .set_branch(reviewed.id, Some("feat/in-review"))
            .unwrap();
        // ...and a refine in flight for the cancel slot, which rides the strip
        // rather than the queue.
        let refining = store
            .create_task(task("being rewritten", TaskState::Proposed))
            .unwrap();
        store
            .record_refine_launch(
                refining.id,
                "thin body",
                "claude",
                None,
                LivenessSource::Pid,
                None,
            )
            .unwrap();
        let mut app = App::new(
            store,
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();

        let mut seen_refine = false;
        let mut seen_wait = false;
        let mut seen_cancel = false;
        let mut seen_message = false;
        let mut seen_review_keys = false;
        for screen in Screen::ALL {
            app.screen = screen;
            let mut i = 0;
            loop {
                let rows = match screen {
                    // Re-read each time: folding a digest open below adds rows.
                    Screen::Cockpit => app.cockpit_rows.len(),
                    Screen::Tasks => app.all.len(),
                    _ => 1,
                };
                if i >= rows {
                    break;
                }
                match screen {
                    Screen::Cockpit => app.cockpit_sel = i,
                    Screen::Tasks => app.tasks_sel = i,
                    _ => {}
                }
                // Proposals ride as a digest row; fold it open so the proposal
                // itself — the row the refine slot answers on — is selectable.
                if app.enter_hint() == Some("⏎ expand") {
                    app.on_key(KeyEvent::from(KeyCode::Enter));
                }
                let keys: Vec<&str> = key_hints(&app).iter().map(|(k, _)| *k).collect();
                assert!(
                    keys.len() <= 11,
                    "{screen:?} row {i} shows {} slots: {keys:?}",
                    keys.len()
                );
                assert!(keys.contains(&"?"), "{screen:?} must advertise ?: {keys:?}");
                // The task screens pair a lowercase key with its uppercase
                // variant; the other two bind unrelated actions to the same
                // letter, so their slots stay apart.
                if matches!(screen, Screen::Cockpit | Screen::Tasks) {
                    for lone in ["d", "D", "r", "R", "n", "N"] {
                        assert!(
                            !keys.contains(&lone),
                            "{screen:?} should pair {lone} into one slot: {keys:?}"
                        );
                    }
                }
                seen_refine |= keys.contains(&"r/R");
                seen_wait |= keys.contains(&"w");
                seen_cancel |= keys.contains(&"C");
                seen_message |= keys.contains(&"a/A");
                seen_review_keys |= keys.contains(&"o") && keys.contains(&"g");
                // Dispatch is advertised only where it can act, so the line
                // never offers a verb whose only answer is the state it
                // refuses — which is also what buys the message slot its room.
                assert!(
                    !(keys.contains(&"d/D") && keys.contains(&"a/A")),
                    "{screen:?} row {i}: {keys:?}"
                );
                // The refine keys and the cancel are mutually exclusive by
                // state, which is what keeps the line within its eleven slots.
                assert!(
                    !(keys.contains(&"r/R") && keys.contains(&"C")),
                    "{screen:?} row {i}: {keys:?}"
                );
                i += 1;
            }
        }
        assert!(
            seen_refine && seen_wait && seen_cancel && seen_message && seen_review_keys,
            "the conditional slots never showed"
        );
    }

    /// The line may drop a key, but the map may not: every key the hint line
    /// can show is in that screen's key map, so trimming the line never makes
    /// a key undiscoverable (DESIGN.md §9).
    #[test]
    fn every_hinted_key_appears_in_the_key_map() {
        use crate::app::App;

        // A combined slot (`d/D`, `j/k`) stands for its individual keys on both
        // sides, so compare key by key.
        let split = |key: &'static str| key.split('/').collect::<Vec<_>>();
        for screen in Screen::ALL {
            let mut app = App::new(
                voro_core::Store::open_in_memory().unwrap(),
                crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                    "/nonexistent/voro.db",
                )),
            )
            .unwrap();
            app.screen = screen;
            let mapped: Vec<&str> = key_map(screen, app.projects.is_empty())
                .iter()
                .flat_map(|(_, entries)| entries.iter())
                .flat_map(|(key, _)| split(key))
                .collect();
            for (key, ..) in hint_candidates(&app) {
                for one in split(key) {
                    assert!(
                        mapped.contains(&one),
                        "{screen:?} hints {key:?} but its key map omits {one:?}: {mapped:?}"
                    );
                }
            }
        }
    }

    /// The case convention (DESIGN.md §9): an uppercase key is the interactive
    /// half of a lowercase/uppercase pair, or one of the exceptions the doc
    /// names. A new uppercase binding that is neither fails here, which is the
    /// point — it is the prompt to decide which of the two it is.
    #[test]
    fn every_uppercase_key_is_paired_or_a_named_exception() {
        let paired: Vec<&str> = [DISPATCH_KEYS, REFINE_KEYS, NEW_KEYS, MESSAGE_KEYS]
            .iter()
            .flat_map(|set| set.iter())
            .map(|(key, _)| *key)
            .collect();
        for screen in Screen::ALL {
            for no_projects in [false, true] {
                let uppercase = key_map(screen, no_projects)
                    .into_iter()
                    .flat_map(|(_, entries)| entries)
                    .flat_map(|(key, _)| key.split('/').collect::<Vec<_>>())
                    .filter(|key| key.chars().count() == 1 && key.chars().all(char::is_uppercase));
                for key in uppercase {
                    assert!(
                        paired.contains(&key) || CASE_EXCEPTIONS.contains(&(screen, key)),
                        "{screen:?} binds {key:?} uppercase, but it is neither half of a pair \
                         nor a documented exception — see DESIGN.md §9"
                    );
                }
            }
        }
    }

    /// `?` opens the current screen's map from any screen, listing the keys the
    /// line has no room for and the gloss for each uppercase variant; any key
    /// closes it again (DESIGN.md §9).
    #[test]
    fn the_key_map_overlay_opens_on_question_mark_and_closes_on_any_key() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};

        let mut app = App::new(
            voro_core::Store::open_in_memory().unwrap(),
            crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                "/nonexistent/voro.db",
            )),
        )
        .unwrap();
        app.screen = Screen::Projects;
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        assert!(matches!(app.mode, Mode::KeyMap { .. }));

        let width: u16 = 120;
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Keys — projects"), "{rendered}");
        assert!(rendered.contains("archive or unarchive"), "{rendered}");

        // Any key is a dismissal, including one the screen binds.
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// The map has to *hold* what it lists: on the smallest terminal worth
    /// supporting, every entry of every screen is reachable — paged to with
    /// `tab` where one screenful cannot carry it, and never clipped, key and
    /// whole gloss both. Asserting a handful of entries instead is what let the
    /// list outgrow the overlay in the first place: the two that fell off the
    /// bottom were simply not among the ones checked.
    #[test]
    fn every_key_map_entry_is_reachable_on_a_small_terminal() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};

        const W: u16 = 80;
        const H: u16 = 24;

        for projects in [0, 1] {
            let mut store = voro_core::Store::open_in_memory().unwrap();
            for i in 0..projects {
                store.create_project(&format!("p{i}"), "/tmp").unwrap();
            }
            let mut app = App::new(
                store,
                crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
                    "/nonexistent/voro.db",
                )),
            )
            .unwrap();
            let mut terminal = Terminal::new(TestBackend::new(W, H)).unwrap();

            for screen in Screen::ALL {
                app.screen = screen;
                app.on_key(KeyEvent::from(KeyCode::Char('?')));
                // More turns than any screen's map has pages; the page index
                // wraps, so the extra ones re-read a page already seen.
                let mut seen = String::new();
                for _ in 0..8 {
                    terminal
                        .draw(|f| {
                            draw(f, &app);
                        })
                        .unwrap();
                    seen.push_str(
                        &terminal
                            .backend()
                            .buffer()
                            .content()
                            .chunks(W as usize)
                            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                    seen.push('\n');
                    app.on_key(KeyEvent::from(KeyCode::Tab));
                }
                app.on_key(KeyEvent::from(KeyCode::Esc));

                for (title, entries) in key_map(screen, app.projects.is_empty()) {
                    assert!(seen.contains(title), "{screen:?}: no {title:?}:\n{seen}");
                    for (key, label) in entries {
                        // Keys are right-aligned with two trailing spaces, so
                        // the pair is one substring of the row it renders on.
                        let row = format!("{key}  {label}");
                        assert!(
                            seen.contains(&row),
                            "{screen:?} with {projects} project(s): {row:?} is in the key map but \
                             no page of it at {W}x{H} shows that entry whole — the map has \
                             outgrown its overlay (DESIGN.md §9):\n{seen}"
                        );
                    }
                }
            }
        }
    }
}
