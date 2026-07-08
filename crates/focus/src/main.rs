mod app;
mod editor;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, EditorRequest};
use focus_core::Store;

fn db_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--db"
            && let Some(path) = args.next()
        {
            return PathBuf::from(path);
        }
    }
    if let Some(path) = std::env::var_os("FOCUS_DB") {
        return PathBuf::from(path);
    }
    Store::default_db_path()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = db_path();
    let store = Store::open(&path)?;
    let mut app = App::new(store)?;

    let mut terminal = ratatui::init();
    let result = loop {
        if let Err(e) = terminal.draw(|frame| ui::draw(frame, &app)) {
            break Err(e.into());
        }
        match event::poll(Duration::from_millis(500)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => app.on_key(key),
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            },
            Ok(false) => {}
            Err(e) => break Err(e.into()),
        }
        if let Some(request) = app.pending_editor.take() {
            // $EDITOR owns the terminal for the duration; tear the TUI down
            // around it rather than fighting over raw mode.
            ratatui::restore();
            editor_session(&mut app, request);
            terminal = ratatui::init();
        }
        if app.should_quit {
            break Ok(());
        }
    };
    ratatui::restore();
    result
}

/// Run the $EDITOR round-trip until the form parses and applies, feeding
/// errors back into the file. An empty save cancels.
fn editor_session(app: &mut App, request: EditorRequest) {
    let (mut text, allow_state) = match request {
        EditorRequest::Create { .. } => (editor::template_new(), true),
        EditorRequest::Edit { task_id } => {
            let Ok(task) = app.store.task(task_id) else {
                app.status = Some(format!("task {task_id} not found"));
                return;
            };
            let blocks: Vec<i64> = match app.store.deps_of(task_id) {
                Ok(deps) => deps
                    .iter()
                    .filter(|d| d.kind == focus_core::DepKind::Blocks)
                    .map(|d| d.depends_on)
                    .collect(),
                Err(e) => {
                    app.status = Some(e.to_string());
                    return;
                }
            };
            (editor::template_edit(&task, &blocks), false)
        }
    };

    loop {
        match editor::run_editor(&text) {
            Ok(saved) if saved.trim().is_empty() => {
                app.status = Some("cancelled".into());
                return;
            }
            Ok(saved) => match editor::parse(&saved, allow_state) {
                Ok(form) => {
                    let applied = match request {
                        EditorRequest::Create { project_id } => {
                            app.create_from_form(project_id, form)
                        }
                        EditorRequest::Edit { task_id } => app.update_from_form(task_id, form),
                    };
                    match applied {
                        Ok(()) => return,
                        Err(e) => text = editor::with_error(&saved, &e.to_string()),
                    }
                }
                Err(e) => text = editor::with_error(&saved, &e),
            },
            Err(e) => {
                app.status = Some(e);
                return;
            }
        }
    }
}
