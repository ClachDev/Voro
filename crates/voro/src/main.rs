mod app;
mod cli;
mod dispatch;
mod editor;
mod reconcile;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::{App, EditorRequest};
use voro_core::Store;

/// Pull `--db PATH` out of the argument list; whatever remains is the CLI
/// verb and its arguments (empty → launch the TUI).
fn split_db(args: Vec<String>) -> (PathBuf, Vec<String>) {
    let mut rest = Vec::new();
    let mut db = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--db" {
            db = it.next().map(PathBuf::from);
        } else {
            rest.push(arg);
        }
    }
    let db = db
        .or_else(|| std::env::var_os("VORO_DB").map(PathBuf::from))
        .unwrap_or_else(Store::default_db_path);
    (db, rest)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (path, verb_args) = split_db(std::env::args().skip(1).collect());
    let mut store = Store::open(&path)?;
    let ctx = dispatch::DispatchCtx::from_db_path(&path);

    if !verb_args.is_empty() {
        match cli::run(&mut store, verb_args, &ctx) {
            Ok(output) => {
                println!("{}", output.trim_end_matches('\n'));
                return Ok(());
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        }
    }

    let mut app = App::new(store, ctx)?;

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
        if let Err(e) = app.poll_external() {
            break Err(e.into());
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
                    .filter(|d| d.kind == voro_core::DepKind::Blocks)
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
