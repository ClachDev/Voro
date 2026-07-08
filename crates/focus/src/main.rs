mod app;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};

use app::{App, CockpitRow, Screen};
use focus_core::Store;

fn db_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--db" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
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
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => on_key(&mut app, key),
                Ok(_) => {}
                Err(e) => break Err(e.into()),
            },
            Ok(false) => {}
            Err(e) => break Err(e.into()),
        }
        if app.should_quit {
            break Ok(());
        }
    };
    ratatui::restore();
    result
}

fn on_key(app: &mut App, key: KeyEvent) {
    app.status = None;
    match key.code {
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Tab => app.toggle_screen(),
        KeyCode::Char('j') | KeyCode::Down => app.move_selection(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_selection(-1),
        KeyCode::Char('r') => {
            let result = app.refresh();
            app.report(result);
        }
        KeyCode::Enter => {
            if app.screen == Screen::Cockpit
                && matches!(
                    app.cockpit_rows.get(app.cockpit_sel),
                    Some(CockpitRow::Triage)
                )
            {
                app.jump_to_proposed();
            }
        }
        _ => {}
    }
}
