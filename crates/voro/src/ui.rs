use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::Clear;

use crate::app::{App, Mode, Screen};

mod cockpit;
mod config;
mod milestones;
mod modes;
mod projects;
mod rows;
mod status;
mod tasks;
mod tree;

use cockpit::draw_cockpit;
use config::draw_config;
use modes::draw_mode;
use projects::draw_projects;
use tasks::draw_tasks;

const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// What a click at some point of the last-drawn frame means (DESIGN.md §9).
/// Each variant carries the index the click selects, counted in the same space
/// the key handlers count in — so routing a click is setting the field `j`/`k`
/// would set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A cockpit row, queue strip and running strip alike: an index into
    /// `App::cockpit_rows`, the one space `cockpit_sel` counts in.
    CockpitRow(usize),
    TaskRow(usize),
    ProjectRow(usize),
    /// A Config screen row, settings list and viewers list alike: an index into
    /// `App::config_rows`, the one space `config_sel` counts in.
    ConfigRow(usize),
    MilestoneRow(usize),
    /// An option of whichever modal picker is open.
    PickerOption(usize),
}

/// The click targets of one drawn frame. Built by [`draw`] and read by
/// `App::on_mouse`, which is what keeps the key handlers free of geometry: the
/// layout is resolved where it is already known, at draw time. Anything no rect
/// covers — the detail pane, the header, the status line, empty space — is a
/// dead zone where a click does nothing.
#[derive(Debug, Default)]
pub struct HitMap(Vec<(Rect, Hit)>);

impl HitMap {
    /// The rows of a bordered list, mapped through `hit`. `offset` is the scroll
    /// ratatui computed while rendering, so a scrolled list still maps a visible
    /// line to the item on it; lines past `count` items — the trailing read-only
    /// note some lists carry — get no target. Rows are one line tall throughout.
    fn push_list(&mut self, area: Rect, offset: usize, count: usize, hit: impl Fn(usize) -> Hit) {
        let inner = area.inner(Margin::new(1, 1));
        for line in 0..inner.height {
            let item = offset + line as usize;
            if item >= count {
                break;
            }
            self.0.push((
                Rect::new(inner.x, inner.y + line, inner.width, 1),
                hit(item),
            ));
        }
    }

    /// Drop every target, for when a modal takes the pointer.
    fn clear(&mut self) {
        self.0.clear();
    }

    pub fn at(&self, col: u16, row: u16) -> Option<Hit> {
        self.0
            .iter()
            .find(|(rect, _)| rect.contains((col, row).into()))
            .map(|(_, hit)| *hit)
    }
}

/// The canonical rendering of a task identifier, right-aligned for list columns.
fn task_ref(id: i64) -> String {
    format!("{:>4}", format!("#{id}"))
}

pub fn draw(frame: &mut Frame, app: &App) -> HitMap {
    let mut hits = HitMap::default();
    match app.screen {
        Screen::Cockpit => draw_cockpit(frame, app, &mut hits),
        Screen::Tasks => draw_tasks(frame, app, &mut hits),
        Screen::Projects => draw_projects(frame, app, &mut hits),
        Screen::Config => draw_config(frame, app, &mut hits),
        Screen::Milestones => milestones::draw(frame, app, &mut hits),
    }
    // A modal owns the pointer: the screen behind it keeps drawing but stops
    // being clickable, so only the popup's own options are targets.
    if !matches!(app.mode, Mode::Normal) {
        hits.clear();
    }
    draw_mode(frame, app, &mut hits);
    hits
}

/// A centred popup rect, cleared of what is beneath it.
pub fn popup_area(frame: &mut Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    };
    frame.render_widget(Clear, rect);
    rect
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::CockpitRow;
    use voro_core::{LivenessSource, Priority, TaskState};

    /// The refusal `g` gives on a checkout `gh` cannot address (DESIGN.md §8) —
    /// the shape every wrapping test here cares about: long, and closing on the
    /// key to press instead.
    pub(super) const GH_REFUSAL: &str = "/home/michael/Projects/demoproj is not a GitHub repository, \
         so there is no pull request to open — use `o` to see this task's diff in a viewer";

    /// Jump to a screen the way the operator does, with the alt-digit binding
    /// (DESIGN.md §9) — a bare digit is a weight or a priority.
    pub(super) fn alt_screen(app: &mut crate::app::App, digit: char) {
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        app.on_key(KeyEvent::new(KeyCode::Char(digit), KeyModifiers::ALT));
    }

    /// Every screen's status region, read back as one whitespace-normalised
    /// string, so a message that wrapped across rows reads as itself again.
    pub(super) fn screen_text(
        terminal: &ratatui::Terminal<ratatui::backend::TestBackend>,
    ) -> String {
        let buffer = terminal.backend().buffer();
        let area = buffer.area;
        let rows: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        rows.join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// An app showing `status`, over `queued` ready tasks and `running`
    /// dispatched ones — the two counts are what decide how much of the cockpit
    /// the queue and the running strip claim.
    pub(super) fn app_with_status(status: &str, queued: usize, running: usize) -> crate::app::App {
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let mut task = |title: String| {
            store
                .create_task(NewTask {
                    project_id: p.id,
                    repo_id: None,
                    title,
                    body: String::new(),
                    priority: Priority::P2,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                    milestone: false,
                })
                .unwrap()
                .id
        };
        for i in 0..queued {
            task(format!("queued {i}"));
        }
        let live: Vec<i64> = (0..running).map(|i| task(format!("live {i}"))).collect();
        for id in live {
            store
                .record_dispatch(id, "claude", None, LivenessSource::Listing, None)
                .unwrap();
        }
        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = crate::app::App::new(store, ctx).unwrap();
        app.status = Some(status.into());
        app
    }

    // --- mouse (DESIGN.md §9) ---
    //
    // Every click test goes through the real draw, so what it clicks is the
    // geometry the operator sees: the row is found by the text on it rather
    // than by a coordinate the test picked.

    type TestTerminal = ratatui::Terminal<ratatui::backend::TestBackend>;

    /// Draw a frame and keep the hit-map it built — the pair the event loop
    /// holds between a draw and the next click.
    fn frame(app: &crate::app::App, terminal: &mut TestTerminal) -> HitMap {
        let mut hits = HitMap::default();
        terminal.draw(|f| hits = draw(f, app)).unwrap();
        hits
    }

    /// One drawn row, cell by cell — the column of a cell is its index, which
    /// is what a click is addressed in.
    fn cells_at(terminal: &TestTerminal, y: u16) -> Vec<String> {
        let buf = terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn line_at(terminal: &TestTerminal, y: u16) -> String {
        cells_at(terminal, y).concat()
    }

    /// Where on screen some text was drawn, as the point a click on it would
    /// land — so a test aims at what the operator sees rather than at a
    /// coordinate it worked out for itself.
    fn point_of(terminal: &TestTerminal, needle: &str) -> (u16, u16) {
        let height = terminal.backend().buffer().area.height;
        for y in 0..height {
            let cells = cells_at(terminal, y);
            for x in 0..cells.len() {
                if cells[x..].concat().starts_with(needle) {
                    return (x as u16, y);
                }
            }
        }
        panic!("'{needle}' is not on screen");
    }

    fn row_of(terminal: &TestTerminal, needle: &str) -> u16 {
        point_of(terminal, needle).1
    }

    pub(super) fn ready_task(
        store: &mut voro_core::Store,
        project_id: i64,
        title: &str,
    ) -> voro_core::Task {
        store
            .create_task(voro_core::NewTask {
                project_id,
                repo_id: None,
                title: title.into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap()
    }

    pub(super) fn test_app(store: voro_core::Store) -> crate::app::App {
        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        crate::app::App::new(store, ctx).unwrap()
    }

    /// A click on a cockpit row selects it — in the queue and in the running
    /// strip alike, which are one selection despite being two panes — and the
    /// detail pane follows, since it reads the same selection the keys move.
    #[test]
    fn cockpit_click_selects_the_row_under_the_pointer() {
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let first = ready_task(&mut store, p.id, "first");
        let second = ready_task(&mut store, p.id, "second");
        let third = ready_task(&mut store, p.id, "third");
        let live = ready_task(&mut store, p.id, "in flight");
        store
            .record_dispatch(live.id, "claude", None, LivenessSource::Pid, None)
            .unwrap();

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        let hits = frame(&app, &mut terminal);
        assert_eq!(app.selected_task_id(), Some(first.id));

        // A queue row two below the selection: the click moves the selection
        // there and nowhere else — no transition menu, no dispatch.
        let (x, y) = point_of(&terminal, &task_ref(third.id));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_task_id(), Some(third.id));
        assert!(matches!(app.mode, Mode::Normal));

        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, &task_ref(second.id));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_task_id(), Some(second.id));

        // The running strip is a separate pane over the same row space.
        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, &task_ref(live.id));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_task_id(), Some(live.id));
        assert!(matches!(
            app.cockpit_rows[app.cockpit_sel],
            CockpitRow::Running(_)
        ));
    }

    /// The panes that show rather than list — the detail card, the header, the
    /// status line — are dead zones, so a click in one changes nothing.
    #[test]
    fn clicks_outside_a_list_do_nothing() {
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let first = ready_task(&mut store, p.id, "first");
        ready_task(&mut store, p.id, "second");

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        let hits = frame(&app, &mut terminal);

        // The header, the detail pane's own border and body, the status line.
        let detail = row_of(&terminal, "Detail");
        for y in [0, detail, detail + 2, 23] {
            app.on_mouse(5, y, &hits);
            assert_eq!(
                app.selected_task_id(),
                Some(first.id),
                "a click at row {y} should not move the selection"
            );
        }
        assert!(matches!(app.mode, Mode::Normal));
    }

    /// A full-screen list scrolled past its first page still maps the line
    /// under the pointer to the item drawn on it, because the hit-map is built
    /// from the scroll offset ratatui itself computed while rendering.
    #[test]
    fn tasks_browser_click_selects_the_right_row_when_scrolled() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        for i in 0..40 {
            ready_task(&mut store, p.id, &format!("task {i}"));
        }

        let mut app = test_app(store);
        alt_screen(&mut app, '2');
        assert_eq!(app.screen, Screen::Tasks);
        // Walk the selection off the bottom of the pane so the list scrolls.
        for _ in 0..35 {
            app.on_key(KeyEvent::from(KeyCode::Char('j')));
        }

        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        let hits = frame(&app, &mut terminal);
        // The first row on screen is no longer the first task, so a hit-map
        // that ignored the offset would be off by exactly that much.
        assert!(
            !line_at(&terminal, 1).contains(&task_ref(app.all[0].task.id)),
            "the list should have scrolled: {}",
            line_at(&terminal, 1)
        );

        let expected = app.all[30].task.id;
        let (x, y) = point_of(&terminal, &task_ref(expected));
        app.on_mouse(x, y, &hits);
        assert_eq!(app.all[app.tasks_sel].task.id, expected);
        assert!(matches!(app.mode, Mode::Normal), "a click opens no popup");
    }

    /// The other two screens list the same way and click the same way.
    #[test]
    fn projects_and_config_clicks_select_rows() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let dir = tempfile::Builder::new()
            .prefix("voro-ui-mouse-")
            .tempdir()
            .unwrap()
            .keep();
        let agents_path = dir.join("voro.toml");
        std::fs::write(
            &agents_path,
            "[viewers.zed]\ncmd = \"zed {path}\"\n\n[viewers.diff]\ncmd = \"git diff {base}\"\n",
        )
        .unwrap();

        let mut store = Store::open_in_memory().unwrap();
        store.create_project("alpha", "/tmp/alpha").unwrap();
        store.create_project("beta", "/tmp/beta").unwrap();
        store.create_project("gamma", "/tmp/gamma").unwrap();

        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = crate::app::App::new(store, ctx).unwrap();
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();

        alt_screen(&mut app, '3');
        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, "gamma");
        app.on_mouse(x, y, &hits);
        assert_eq!(app.projects[app.projects_sel].name, "gamma");

        // A bare digit on the projects screen is a weight, so leave by the keys
        // that are not: tab across to Config.
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Config);
        let hits = frame(&app, &mut terminal);
        let target = app.config_viewers[1].name.clone();
        let (x, y) = point_of(&terminal, &target);
        app.on_mouse(x, y, &hits);
        assert_eq!(
            app.selected_viewer().map(|v| v.name.clone()),
            Some(target.clone())
        );

        // The settings list above clicks the same way — the two lists share one
        // row space, so a click in either lands the single selection.
        let (x, y) = point_of(&terminal, "dispatch cap");
        app.on_mouse(x, y, &hits);
        assert_eq!(app.selected_setting().map(|s| s.name), Some("dispatch cap"));
        assert!(app.selected_viewer().is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// In a picker a click moves the cursor, and a second click on the option
    /// already under it confirms — the same ⏎ the keyboard would send, so the
    /// transition runs through the state machine unchanged.
    #[test]
    fn picker_click_selects_then_a_second_click_confirms() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = ready_task(&mut store, p.id, "startable");

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();
        app.on_key(KeyEvent::from(KeyCode::Enter));
        // ready → [start, park, abandon]; park is the one below the cursor.
        assert!(matches!(app.mode, Mode::Transition { sel: 0, .. }));

        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, "park → parked");
        app.on_mouse(x, y, &hits);
        assert!(
            matches!(app.mode, Mode::Transition { sel: 1, .. }),
            "the first click should only move the cursor"
        );
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Ready);

        let hits = frame(&app, &mut terminal);
        let (x, y) = point_of(&terminal, "park → parked");
        app.on_mouse(x, y, &hits);
        assert!(matches!(app.mode, Mode::Normal), "the picker should close");
        assert_eq!(app.store.task(task.id).unwrap().state, TaskState::Parked);
    }

    /// A popup owns the pointer: a click beside it is not a dismiss, and the
    /// list it covers is not clickable through it. Text-entry modes have no
    /// options to click, so they ignore the mouse entirely.
    #[test]
    fn clicks_outside_a_popup_and_in_text_entry_modes_do_nothing() {
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let first = ready_task(&mut store, p.id, "first");
        let second = ready_task(&mut store, p.id, "second");

        let mut app = test_app(store);
        let mut terminal = TestTerminal::new(ratatui::backend::TestBackend::new(100, 24)).unwrap();

        // The row the popup does not cover would have been a target a moment ago.
        let hits = frame(&app, &mut terminal);
        let (x_second, y_second) = point_of(&terminal, &task_ref(second.id));
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Transition { .. }));

        let hits_modal = frame(&app, &mut terminal);
        assert!(hits.at(x_second, y_second).is_some());
        for (col, row) in [(0, 0), (x_second, y_second), (99, 23)] {
            app.on_mouse(col, row, &hits_modal);
        }
        assert!(
            matches!(app.mode, Mode::Transition { sel: 0, .. }),
            "clicks outside the popup should neither dismiss nor move it"
        );
        assert_eq!(app.selected_task_id(), Some(first.id));

        // A text prompt: nothing in the frame is a click target at all.
        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.mode = Mode::Prompt {
            task_id: first.id,
            kind: crate::app::PromptKind::Ask,
            buffer: "half typed".into(),
        };
        let hits = frame(&app, &mut terminal);
        for y in 0..24 {
            for x in [0, 5, 50, 99] {
                assert!(
                    hits.at(x, y).is_none(),
                    "a text prompt should offer no click target at ({x}, {y})"
                );
            }
        }
    }
}
