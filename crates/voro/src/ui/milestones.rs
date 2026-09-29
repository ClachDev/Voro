//! Rendering for the Milestones tab and the milestone parts of the other
//! screens (DESIGN.md §9): the cockpit column, the browser's fold headers, the
//! picker, and the tab's own key line and key map.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use voro_core::Task;

use super::{HitMap, KeySection, SELECTED, draw_status, status_height, task_ref};
use crate::app::App;

/// The cockpit column's width, fixed so the project and title after it stay
/// aligned whether or not a row has a milestone.
pub(super) const COLUMN_WIDTH: usize = 16;

fn milestone_style() -> Style {
    Style::new().fg(Color::Blue)
}

/// `text` cut to `width` characters, ending in `…` when it had to be cut.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// The cockpit's milestone column: the title of the task's nearest
/// milestone, with `+N` for the others when it has several, padded to
/// [`COLUMN_WIDTH`]. Blank when it has none.
pub(super) fn column_span(app: &App, task_id: i64) -> Span<'static> {
    let milestones = app.milestones_of(task_id);
    let text = match milestones.split_first() {
        None => String::new(),
        Some((first, rest)) => {
            let more = if rest.is_empty() {
                String::new()
            } else {
                format!(" +{}", rest.len())
            };
            let room = COLUMN_WIDTH - more.chars().count();
            format!("{}{more}", truncate(&first.title, room))
        }
    };
    Span::styled(format!("{text:<COLUMN_WIDTH$}"), milestone_style())
}

/// The detail panes' milestone lines: one per nearest milestone, which is
/// what a triage verdict confirms or the picker corrects.
pub(super) fn detail_lines(app: &App, task_id: i64) -> Vec<Line<'static>> {
    app.milestones_of(task_id)
        .into_iter()
        .map(|m| {
            Line::from(Span::styled(
                format!("milestone: #{} {}", m.id, m.title),
                milestone_style(),
            ))
        })
        .collect()
}

/// A browser fold header: the milestone's title, or `unattached`, and the
/// fold's counts. A closed milestone's fold is dimmed like its tasks.
pub(super) fn group_line(app: &App, group: Option<i64>) -> Line<'static> {
    let arrow = if app.open_groups.contains(&group) {
        "▾"
    } else {
        "▸"
    };
    let (open, done) = app.group_counts(group);
    let counts = format!("  {open} open · {done} done");
    match group.and_then(|id| app.milestone(id)) {
        Some(m) => {
            let closed = m.milestone.state.is_terminal();
            let style = if closed {
                Style::new().dim()
            } else {
                milestone_style().bold()
            };
            Line::from(vec![
                Span::styled(
                    format!(
                        "{arrow} {} {}",
                        task_ref(m.milestone.id).trim(),
                        m.milestone.title
                    ),
                    style,
                ),
                Span::styled(counts, Style::new().dim()),
            ])
        }
        None => Line::from(vec![
            Span::styled(format!("{arrow} unattached"), Style::new().bold()),
            Span::styled(counts, Style::new().dim()),
        ]),
    }
}

/// One row of the milestone picker: a tick for the milestones the task blocks
/// directly, then the title, with the owning project's name on a milestone
/// from another project.
pub(super) fn picker_row(app: &App, task_id: i64, milestone: &Task) -> Line<'static> {
    let owner = app
        .all
        .iter()
        .find(|row| row.task.id == task_id)
        .filter(|row| row.task.project_id != milestone.project_id)
        .and_then(|_| app.projects.iter().find(|p| p.id == milestone.project_id))
        .map(|p| format!("[{}] ", p.name))
        .unwrap_or_default();
    let text = format!(
        "{owner}{} {} ({})",
        task_ref(milestone.id).trim(),
        milestone.title,
        milestone.state
    );
    let (mark, style) = if app.blocks_directly(task_id, milestone.id) {
        ("✓ ", milestone_style())
    } else {
        ("  ", Style::new().dim())
    };
    Line::from(vec![Span::styled(mark, style), Span::styled(text, style)])
}

/// The Milestones tab (DESIGN.md §9): one row per milestone in any state, the
/// selected one's body beneath.
pub(super) fn draw(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let list_height = (app.milestones.len() as u16 + 2).clamp(3, 12);
    let [list_area, detail_area, status] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    let items: Vec<ListItem> = app
        .milestones
        .iter()
        .map(|m| {
            let t = &m.milestone;
            let style = if t.state.is_terminal() {
                Style::new().dim()
            } else {
                Style::new()
            };
            let project = app
                .projects
                .iter()
                .find(|p| p.id == t.project_id)
                .map_or("", |p| p.name.as_str());
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(
                        "{} {:11} {:14} {}",
                        task_ref(t.id),
                        t.state.as_str(),
                        project,
                        t.title
                    ),
                    style,
                ),
                Span::styled(
                    format!("  {} open · {} done", m.open(), m.done()),
                    Style::new().dim(),
                ),
            ]))
        })
        .collect();
    let empty = items.is_empty();
    let mut state = ListState::default().with_selected(if empty {
        None
    } else {
        Some(app.milestones_sel)
    });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Milestones"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, list_area, &mut state);
    hits.push_list(
        list_area,
        state.offset(),
        app.milestones.len(),
        super::Hit::MilestoneRow,
    );
    if empty {
        let inner = list_area.inner(ratatui::layout::Margin::new(1, 1));
        frame.render_widget(
            Paragraph::new("no milestones yet — press n to add one").dim(),
            inner,
        );
    }
    draw_detail(frame, app, detail_area);
    draw_status(frame, app, status);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let block = Block::default().borders(Borders::ALL).title("Detail");
    let Some(m) = app.milestones.get(app.milestones_sel) else {
        frame.render_widget(Paragraph::new("").block(block), area);
        return;
    };
    let t = &m.milestone;
    let project = app
        .projects
        .iter()
        .find(|p| p.id == t.project_id)
        .map_or("", |p| p.name.as_str());
    let mut lines = vec![
        Line::from(Span::styled(t.title.clone(), Style::new().bold())),
        Line::from(format!(
            "#{} · {} · {} · {} open · {} done",
            t.id,
            project,
            t.state,
            m.open(),
            m.done()
        )),
        Line::default(),
    ];
    if t.body.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "no acceptance statement yet — e writes one",
            Style::new().dim(),
        )));
    } else {
        lines.extend(crate::markdown::body_lines(&t.body));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
}

/// The tab's key line (DESIGN.md §9).
pub(super) fn hint_candidates(
    app: &App,
    enter: (&'static str, &'static str, bool),
) -> Vec<(&'static str, &'static str, bool)> {
    let selected = app.milestones.get(app.milestones_sel);
    vec![
        enter,
        (
            "s",
            "state",
            selected.is_some_and(|m| !m.milestone.state.is_terminal()),
        ),
        ("n", "new", true),
        ("e", "edit", selected.is_some()),
        ("?", "keys", true),
        ("tab", "cockpit", true),
        ("q", "quit", true),
    ]
}

/// The tab's complete key map (DESIGN.md §9).
pub(super) fn key_map(screens: KeySection) -> Vec<KeySection> {
    vec![
        (
            "Actions",
            vec![
                ("⏎", "browse the milestone's tasks"),
                ("s", "done, or abandon"),
                ("n", "new milestone, parked"),
                ("e", "edit the milestone's body"),
            ],
        ),
        (
            "Navigation",
            vec![
                ("j/k", "move the selection"),
                ("ctrl-r", "refresh"),
                ("?", "this key map"),
                ("q", "quit"),
            ],
        ),
        screens,
    ]
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use voro_core::{DepKind, Priority, Store, TaskState};

    use crate::app::App;
    use crate::app::milestones::tests::{app_from, new_task};

    /// The frame's rows, one string each.
    fn rows(app: &App, width: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|f| {
                crate::ui::draw(f, app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect())
            .collect()
    }

    const LONG_TITLE: &str = "Tune wheel traction control for low-pile carpet under load";

    #[test]
    fn a_member_row_shows_its_milestone_and_forty_characters_of_its_title_at_100_columns() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Carpet crossing under fleet", "", Priority::P2)
            .unwrap()
            .id;
        let t = new_task(&mut store, p, LONG_TITLE, TaskState::Ready);
        store.add_dep(m, t, DepKind::Blocks).unwrap();
        let app = app_from(store);
        let row = rows(&app, 100)
            .into_iter()
            .find(|r| r.contains(&format!("#{t}")))
            .expect("the task has a cockpit row");
        assert!(row.contains("Carpet crossing… voro: "), "{row}");
        let title: String = LONG_TITLE.chars().take(40).collect();
        assert!(row.contains(&title), "{row}");
    }

    #[test]
    fn a_task_with_two_milestones_shows_the_first_and_a_count() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let a = store
            .create_milestone(p, "Dock", "", Priority::P2)
            .unwrap()
            .id;
        let b = store
            .create_milestone(p, "Fleet", "", Priority::P2)
            .unwrap()
            .id;
        let t = new_task(&mut store, p, "shared", TaskState::Ready);
        store.block_tasks(t, &[a, b]).unwrap();
        let app = app_from(store);
        let row = rows(&app, 100)
            .into_iter()
            .find(|r| r.contains(&format!("#{t}")))
            .unwrap();
        assert!(row.contains("Dock +1          voro: shared"), "{row}");
    }

    #[test]
    fn with_no_milestones_the_column_is_blank_and_the_tab_points_at_n() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let t = new_task(&mut store, p, "alone", TaskState::Ready);
        let mut app = app_from(store);
        let row = rows(&app, 100)
            .into_iter()
            .find(|r| r.contains(&format!("#{t}")))
            .unwrap();
        assert!(row.contains(&format!("P2  {:16} voro: alone", "")), "{row}");

        app.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT));
        let screen = rows(&app, 100).join("\n");
        assert!(
            screen.contains("no milestones yet — press n to add one"),
            "{screen}"
        );
    }

    #[test]
    fn the_tab_and_the_browser_folds_carry_the_same_counts() {
        let mut store = Store::open_in_memory().unwrap();
        voro_core::seed::seed(&mut store).unwrap();
        let mut app = app_from(store);
        app.on_key(KeyEvent::new(KeyCode::Char('5'), KeyModifiers::ALT));
        let tab = rows(&app, 110).join("\n");
        app.on_key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT));
        app.on_key(KeyEvent::from(KeyCode::Char('M')));
        let browser = rows(&app, 110).join("\n");
        for m in &app.milestones {
            let counts = format!("{} open · {} done", m.open(), m.done());
            let on = |screen: &str| {
                screen
                    .lines()
                    .any(|l| l.contains(&m.milestone.title) && l.contains(&counts))
            };
            assert!(
                on(&tab),
                "tab lacks {counts} for {}:\n{tab}",
                m.milestone.title
            );
            assert!(
                on(&browser),
                "fold lacks {counts} for {}:\n{browser}",
                m.milestone.title
            );
        }
        assert!(browser.contains("▸ unattached"), "{browser}");
    }

    #[test]
    fn a_proposal_under_triage_names_the_milestone_it_blocks() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Carpet crossing", "", Priority::P2)
            .unwrap()
            .id;
        let proposal = new_task(&mut store, p, "follow-up", TaskState::Proposed);
        store.add_dep(m, proposal, DepKind::Blocks).unwrap();
        let mut app = app_from(store);
        // The proposal rides a digest; ⏎ folds it open and `j` selects it.
        app.on_key(KeyEvent::from(KeyCode::Enter));
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.selected_task_id(), Some(proposal));
        let screen = rows(&app, 100).join("\n");
        assert!(
            screen.contains(&format!("milestone: #{m} Carpet crossing")),
            "{screen}"
        );
    }
}
