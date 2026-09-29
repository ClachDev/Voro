//! Rendering for the projects screen (DESIGN.md §9).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};

use super::status::{draw_status, status_height};
use super::{Hit, HitMap, SELECTED};
use crate::app::App;

/// The projects screen (DESIGN.md §9): one row per project — weight, name,
/// path, open task count, and the viewer when the project names one (§8). The
/// open count is the project's non-terminal tasks, from the loaded task list.
/// An archived project stays on this screen, dim and tagged, so it can be
/// found and unarchived (§5).
pub(super) fn draw_projects(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let [list_area, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    let items: Vec<ListItem> = app
        .projects
        .iter()
        .map(|p| {
            let open = app
                .all
                .iter()
                .filter(|r| r.task.project_id == p.id && !r.task.state.is_terminal())
                .count();
            let style = if p.weight == 0 || p.archived {
                Style::new().dim()
            } else {
                Style::new()
            };
            let viewer = match &p.viewer {
                Some(name) => format!("  [viewer:{name}]"),
                None => String::new(),
            };
            let archived = if p.archived { "  [archived]" } else { "" };
            // The path column shows the default repo, so a single-repo project
            // reads as it always did; extra checkouts are tagged (DESIGN.md §3).
            let extra = match app.repo_count(p.id) {
                0 | 1 => String::new(),
                n => format!("  [+{} repo(s)]", n - 1),
            };
            ListItem::new(Line::from(Span::styled(
                format!(
                    "{:>2}  {:14} {:28} {} open{extra}{viewer}{archived}",
                    p.weight,
                    p.name,
                    app.project_path(p.id),
                    open
                ),
                style,
            )))
        })
        .collect();
    let empty = items.is_empty();
    let mut state =
        ListState::default().with_selected(if empty { None } else { Some(app.projects_sel) });
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Projects"))
        .highlight_style(SELECTED);
    frame.render_stateful_widget(list, list_area, &mut state);
    hits.push_list(
        list_area,
        state.offset(),
        app.projects.len(),
        Hit::ProjectRow,
    );
    if empty {
        let inner = list_area.inner(ratatui::layout::Margin::new(1, 1));
        frame.render_widget(
            Paragraph::new("no projects yet — press a to add one").dim(),
            inner,
        );
    }
    draw_status(frame, app, status);
}

#[cfg(test)]
mod tests {
    use crate::app::Screen;
    use crate::ui::draw;
    use crate::ui::tests::alt_screen;
    use voro_core::{Priority, TaskState};

    /// End-to-end: the projects screen renders one row per project showing its
    /// weight, name, path, and the count of its non-terminal tasks.
    #[test]
    fn projects_screen_renders_weight_name_path_and_open_count() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        // one open task and one terminal task — only the open one is counted
        let new = |title: &str, state: TaskState| NewTask {
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
        store.create_task(new("open", TaskState::Ready)).unwrap();
        let closed = store.create_task(new("closed", TaskState::Ready)).unwrap();
        store
            .apply(closed.id, voro_core::Action::Start)
            .and_then(|_| store.apply(closed.id, voro_core::Action::Complete(None)))
            .and_then(|_| store.apply(closed.id, voro_core::Action::Accept))
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        key_to_projects(&mut app);
        assert_eq!(app.screen, Screen::Projects);

        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(
            rendered.contains("3") && rendered.contains("voro") && rendered.contains("/tmp/voro"),
            "projects row missing weight/name/path: {rendered}"
        );
        assert!(
            rendered.contains("1 open"),
            "projects row should count only the open task: {rendered}"
        );
    }

    /// Drive the app onto the projects screen with the real key handler.
    fn key_to_projects(app: &mut crate::app::App) {
        alt_screen(app, '3');
    }
}
