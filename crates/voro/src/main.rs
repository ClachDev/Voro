mod app;
mod cli;
mod dispatch;
mod editor;
mod import;
mod markdown;
mod pr;
mod probe;
mod reconcile;
mod session_probe;
mod ui;
mod worktree;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;

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

    let mut terminal = init_terminal();
    // The click targets of the frame on screen, rebuilt by every draw, since
    // the key handlers deliberately run without geometry (DESIGN.md §9).
    let mut hits = ui::HitMap::default();
    let result = loop {
        if let Err(e) = terminal.draw(|frame| hits = ui::draw(frame, &app)) {
            break Err(e.into());
        }
        match event::poll(Duration::from_millis(500)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => app.on_key(key),
                Ok(Event::Mouse(m)) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                    app.on_mouse(m.column, m.row, &hits)
                }
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            },
            Ok(false) => {}
            Err(e) => break Err(e.into()),
        }
        if let Err(e) = app.poll_external() {
            break Err(e.into());
        }
        // Advance the selected review task's PR mergeability probe (DESIGN.md
        // §8): collect a verdict a background thread has finished, and start a
        // probe once the selection has rested. Both halves are non-blocking, so
        // the `gh` call never stalls the loop and scrolling spawns nothing.
        app.poll_conflict_probe();
        if let Some(request) = app.pending_editor.take() {
            // $EDITOR owns the terminal for the duration; tear the TUI down
            // around it rather than fighting over raw mode.
            restore_terminal();
            editor_session(&mut app, request);
            terminal = init_terminal();
        }
        if let Some(request) = app.pending_attach.take() {
            // attach/resume are full-screen interactive sessions that own the
            // terminal until the user detaches, same treatment as $EDITOR.
            restore_terminal();
            foreground_session(&mut app, "attach", &request.command, &request.cwd);
            terminal = init_terminal();
        }
        if let Some(launch) = app.pending_plan.take() {
            // a planning session is interactive in the same way; the refresh
            // on return is what makes the task it proposed appear in the queue.
            restore_terminal();
            foreground_session(&mut app, launch.label, &launch.command, &launch.cwd);
            terminal = init_terminal();
        }
        if app.should_quit {
            break Ok(());
        }
    };
    restore_terminal();
    result
}

/// Take the terminal for the TUI: ratatui's own setup plus mouse reporting,
/// which is what turns a click into an event the loop can route (DESIGN.md §9).
/// Every teardown must undo both, so this is paired with [`restore_terminal`] at
/// each of the round-trips that hand the terminal to another program.
fn init_terminal() -> DefaultTerminal {
    let terminal = ratatui::init();
    install_mouse_panic_hook();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    terminal
}

/// Hand the terminal back, mouse reporting first. A terminal left reporting
/// spews escape bytes on every mouse move, so this runs even where the caller
/// only means to borrow the screen for an $EDITOR or attach round-trip.
fn restore_terminal() {
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
}

/// Disable mouse reporting on the way out of a panic too. `ratatui::init`
/// installs a hook that restores the screen but knows nothing of the capture
/// this TUI adds, so wrap it once with one that does.
fn install_mouse_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            hook(info);
        }));
    });
}

/// Run an agent's attach/resume command — or a planning session (DESIGN.md §8)
/// — in the foreground, the terminal already restored by the caller. The
/// command inherits stdio so the agent's own TUI takes over; on return the
/// world may have moved, so refresh before redrawing. `label` names the flow in
/// the launch log's breadcrumbs. On a non-zero exit the outcome is held on
/// screen until a keypress, and every round-trip is appended to the launch log
/// so even a swallowed failure is recoverable.
fn foreground_session(app: &mut App, label: &str, command: &str, cwd: &str) {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .status();

    let succeeded = matches!(&status, Ok(s) if s.success());
    let message = match &status {
        Ok(s) if s.success() => format!("returned from: {command}"),
        Ok(s) => format!("'{command}' exited with {s}"),
        Err(e) => format!("cannot run '{command}' in {cwd}: {e}"),
    };

    let log_line = match &status {
        Ok(s) => format!("{label}: {command} (cwd {cwd}) exited with {s}"),
        Err(e) => format!("{label}: {command} (cwd {cwd}) could not be run: {e}"),
    };
    dispatch::append_launch_log(&app.launch_log_path(), &log_line);

    // Keep the subprocess's own error output readable: print the outcome
    // beneath it and hold until a keypress before the TUI reinitialises.
    if !succeeded {
        println!("\n{message}");
        println!("(press any key to return to voro)");
        wait_for_keypress();
    }

    app.status = Some(message);
    if let Err(e) = app.refresh() {
        app.status = Some(e.to_string());
    }
}

/// Block until the next key press, used to hold a failed foreground command's
/// output on screen. Raw mode is toggled on just for the read so a single key
/// suffices (no Enter); any read error simply returns rather than trapping
/// the user.
fn wait_for_keypress() {
    use ratatui::crossterm::terminal;
    let raw = terminal::enable_raw_mode().is_ok();
    loop {
        match event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    if raw {
        let _ = terminal::disable_raw_mode();
    }
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
            let blocked_by: Vec<i64> = match app.store.deps_of(task_id) {
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
            (editor::template_edit(&task, &blocked_by), false)
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
