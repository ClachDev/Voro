//! Rendering for the Config screen (DESIGN.md §9): the settings, agents and
//! viewers panes.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::status::{draw_status, status_height};
use super::{Hit, HitMap, SELECTED};
use crate::app::App;

/// The Config screen (DESIGN.md §5): the effective `voro.toml` surface. Agents
/// (read-only) with provenance and the default marked, over the viewers — the
/// built-ins and the user's tables, each with its provenance, the user's
/// editable — with the legacy anonymous `[viewer]` shown read-only beneath
/// them. A file that failed to parse is surfaced here rather than rendering
/// empty.
pub(super) fn draw_config(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    let [main, status] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(status_height(app, frame.area())),
    ])
    .areas(frame.area());

    if let Some(err) = &app.config_error {
        let para = Paragraph::new(vec![
            Line::from(Span::styled(
                "voro.toml could not be read:",
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::raw(err.clone())),
            Line::from(Span::styled(
                format!("path: {}", app.config_path().display()),
                Style::new().dim(),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Config"));
        frame.render_widget(para, main);
        draw_status(frame, app, status);
        return;
    }

    // Agents: one line each (default starred, verbs listed), plus a warning line
    // where an override drops built-in verbs (DESIGN.md §8).
    let mut agent_lines: Vec<Line> = app
        .config_warnings
        .iter()
        .map(|warning| {
            Line::from(Span::styled(
                format!("! {warning}"),
                Style::new().fg(Color::Yellow),
            ))
        })
        .collect();
    for a in &app.config_agents {
        let marker = if a.is_default { "* " } else { "  " };
        let verbs = if a.verbs.is_empty() {
            String::new()
        } else {
            format!("  [{}]", a.verbs.join(" "))
        };
        agent_lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<10}", a.name), Style::new().bold()),
            Span::styled(format!(" {:<14}", a.provenance), Style::new().dim()),
            Span::raw(verbs),
        ]));
        // The dispatch command on a dim continuation line (clipped to the pane),
        // so the row shows what each agent actually runs.
        agent_lines.push(Line::from(Span::styled(
            format!("    {}", a.dispatch),
            Style::new().dim(),
        )));
        // What `{model}` resolves to, under the command whose placeholder it
        // fills, so it reads without opening voro.toml. It is a line of its own
        // rather than a tail on the name row above, which the verb list has
        // grown long enough to push off an ordinary terminal (DESIGN.md §5).
        if let Some((model, deep, plan)) = &a.models {
            agent_lines.push(Line::from(Span::styled(
                format!("    {{model}}: {model} · deep {deep} · plan {plan}"),
                Style::new().dim(),
            )));
        }
        if !a.missing_verbs.is_empty() {
            agent_lines.push(Line::from(Span::styled(
                format!("    ! override drops: {}", a.missing_verbs.join(", ")),
                Style::new().fg(Color::Yellow),
            )));
        }
    }
    if agent_lines.is_empty() {
        agent_lines.push(Line::from(Span::styled(
            "no agents configured",
            Style::new().dim(),
        )));
    }

    // The pane takes the height its rows need, yielding to the settings list —
    // which is exactly its rows, neither scrolling nor growing — and to what the
    // viewers list below needs for a border and a row: every pane here is
    // reachable whole, so the split is about which is read without a keypress,
    // and that is this one.
    let settings_h = app.config_settings.len() as u16 + 2;
    let agents_h =
        (agent_lines.len() as u16 + 2).clamp(3, main.height.saturating_sub(settings_h + 3).max(3));
    let [agents_area, settings_area, viewers_area] = Layout::vertical([
        Constraint::Length(agents_h),
        Constraint::Length(settings_h),
        Constraint::Min(3),
    ])
    .areas(main);

    // The rows the pane cannot fit are reached with `J`/`K` and the page keys,
    // the cockpit card's gesture: the pane carries no selection to scroll with,
    // and `j`/`k` here are the viewers list's. The count and the keys ride the
    // bottom border, so a pane that is hiding agents says so.
    let total = agent_lines.len() as u16;
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Agents (read-only — * default)");
    let max_scroll = total.saturating_sub(agents_area.height.saturating_sub(2));
    app.config_agents_max_scroll.set(max_scroll);
    let scroll = app.config_agents_scroll.min(max_scroll);
    let block = if max_scroll > 0 {
        block.title_bottom(
            Line::from(format!(" {scroll}/{max_scroll} ↕ J/K PgDn/PgUp ")).right_aligned(),
        )
    } else {
        block
    };
    frame.render_widget(
        Paragraph::new(agent_lines).scroll((scroll, 0)).block(block),
        agents_area,
    );

    // Settings: the `voro.toml` values this screen edits, each with the value in
    // force and where it came from — the file, or the rule Voro fell back to
    // (DESIGN.md §5). They share the selection with the viewers below, so
    // exactly one of the two lists ever shows a highlight.
    let settings_items: Vec<ListItem> = app
        .config_settings
        .iter()
        .map(|s| {
            ListItem::new(Line::from(vec![
                Span::raw(format!("  {:<16}", s.name)),
                Span::styled(format!("{:<14}", s.value), Style::new().bold()),
                Span::styled(format!("({})", s.source), Style::new().dim()),
            ]))
        })
        .collect();
    let settings_count = app.config_settings.len();
    let mut settings_state = ListState::default()
        .with_selected((app.config_sel < settings_count).then_some(app.config_sel));
    let settings = List::new(settings_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Settings — ⏎ edit"),
        )
        .highlight_style(SELECTED);
    frame.render_stateful_widget(settings, settings_area, &mut settings_state);
    hits.push_list(
        settings_area,
        settings_state.offset(),
        settings_count,
        Hit::ConfigRow,
    );

    // Viewers: every viewer `open` can run — the built-ins with the user's
    // tables layered over them, each carrying its provenance like the agents
    // pane above, the default starred — then the anonymous [viewer] as a
    // read-only trailing note when present. A built-in row is dimmed, since
    // e/d refuse it: it is overridden, not edited.
    let mut viewer_items: Vec<ListItem> = Vec::new();
    for v in &app.config_viewers {
        let marker = if v.is_default { "* " } else { "  " };
        let name = if v.editable {
            Span::raw(format!("{:<14}", v.name))
        } else {
            Span::styled(format!("{:<14}", v.name), Style::new().dim())
        };
        viewer_items.push(ListItem::new(Line::from(vec![
            Span::raw(marker),
            name,
            Span::styled(format!("{:<14}", v.provenance), Style::new().dim()),
            Span::styled(v.cmd.clone(), Style::new().dim()),
        ])));
    }
    let named = app.config_viewers.len();
    if let Some(cmd) = &app.config_anon_viewer {
        viewer_items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("  {:<14}", "[viewer]"), Style::new().dim()),
            Span::styled(
                format!("{cmd}  (anonymous — name it in voro.toml to edit)"),
                Style::new().dim(),
            ),
        ])));
    }
    // The selection only ever lands on a named viewer, never the anonymous note
    // — and only while it is past the settings rows above.
    let selected = match app.config_sel.checked_sub(settings_count) {
        Some(i) if i < named => Some(i),
        _ => None,
    };
    let mut state = ListState::default().with_selected(selected);
    let viewers = List::new(viewer_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Viewers — a add · ⏎ edit · d delete")
                .title_bottom(
                    Line::from(format!(" {} ", app.config_path().display())).right_aligned(),
                ),
        )
        .highlight_style(SELECTED);
    frame.render_stateful_widget(viewers, viewers_area, &mut state);
    // Same rule as the selection: the anonymous note below the named viewers is
    // not selectable, so it is not clickable either.
    hits.push_list(viewers_area, state.offset(), named, |i| {
        Hit::ConfigRow(settings_count + i)
    });

    draw_status(frame, app, status);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Screen;
    use crate::ui::draw;
    use crate::ui::tests::alt_screen;

    /// End-to-end: the Config screen renders the read-only agents (with the
    /// default marked) over the editable named viewers, drawn through the real
    /// screen draw path (DESIGN.md §5).
    #[test]
    fn config_screen_renders_agents_and_viewers() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::Store;

        let dir = tempfile::Builder::new()
            .prefix("voro-ui-config-")
            .tempdir()
            .unwrap()
            .keep();
        let agents_path = dir.join("voro.toml");
        std::fs::write(&agents_path, "[viewers.zed]\ncmd = \"zed {path}\"\n").unwrap();

        let store = Store::open_in_memory().unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '4');
        assert_eq!(app.screen, Screen::Config);

        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
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
        assert!(rendered.contains("Agents"), "{rendered}");
        assert!(rendered.contains("claude"), "{rendered}");
        assert!(rendered.contains("Viewers"), "{rendered}");
        assert!(
            rendered.contains("zed") && rendered.contains("zed {path}"),
            "{rendered}"
        );
        // An ordinary terminal keeps both halves of what the built-in claude
        // row says: every optional verb it defines, and — on its own line under
        // the command whose placeholder it fills — what `{model}` resolves to.
        // Both are read off the row rather than spelled out here, so the test
        // asserts that the width survives the built-in's verbs and model map
        // rather than pinning what they currently are.
        let claude = app
            .config_agents
            .iter()
            .find(|a| a.name == "claude")
            .expect("the built-in claude");
        let verbs = format!("[{}]", claude.verbs.join(" "));
        assert!(
            rendered.contains(&verbs),
            "verbs {verbs} clipped:\n{rendered}"
        );
        let (model, deep, plan) = claude.models.as_ref().expect("claude names a model");
        let models = format!("{{model}}: {model} · deep {deep} · plan {plan}");
        assert!(
            rendered.contains(&models),
            "model map {models} clipped:\n{rendered}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The agents pane sizes to the rows it has: an operator with several
    /// model-carrying agents, each now three lines tall, sees every one of them
    /// without touching the scroll, and the lists below keep their rows
    /// (DESIGN.md §9). What happens when even that is not enough is the scroll
    /// test below.
    #[test]
    fn config_screen_shows_every_agent_it_has() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::Store;

        let dir = tempfile::Builder::new()
            .prefix("voro-ui-config-many-")
            .tempdir()
            .unwrap()
            .keep();
        let agents_path = dir.join("voro.toml");
        let mut toml = String::from("[viewers.zed]\ncmd = \"zed {path}\"\n");
        for n in 1..=4 {
            toml.push_str(&format!(
                "\n[agents.mine{n}]\ndispatch = \"mine{n} run {{prompt_file}} --model {{model}}\"\n\
                 model = \"m{n}\"\n"
            ));
        }
        std::fs::write(&agents_path, toml).unwrap();

        let store = Store::open_in_memory().unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '4');

        // Tall enough for the settings pane the agents now share the screen
        // with: it is a fixed height that neither scrolls nor grows, so it is
        // subtracted before the paragraph takes what its rows need.
        let mut terminal = Terminal::new(TestBackend::new(100, 29)).unwrap();
        // …and it is what the pane fits, not what it can be scrolled to.
        assert_eq!(app.config_agents_scroll, 0);
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
        for n in 1..=4 {
            assert!(rendered.contains(&format!("mine{n}")), "{rendered}");
            assert!(rendered.contains(&format!("{{model}}: m{n}")), "{rendered}");
        }
        // The viewers list keeps a row of its own; what it gave up it can still
        // scroll to.
        assert!(rendered.contains("Viewers"), "{rendered}");
        assert!(rendered.contains("code -n {path}"), "{rendered}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Where the pane cannot fit its rows, the ones past the fold must not
    /// silently go undrawn: the bottom border carries the overflow and the
    /// keys that move it, and `J` walks the hidden agents into view — on a
    /// terminal no larger than 80x24.
    #[test]
    fn config_agents_pane_scrolls_to_the_agents_it_cannot_fit() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::Store;

        let dir = tempfile::Builder::new()
            .prefix("voro-ui-config-scroll-")
            .tempdir()
            .unwrap()
            .keep();
        let agents_path = dir.join("voro.toml");
        let mut toml = String::from("[viewers.zed]\ncmd = \"zed {path}\"\n");
        for n in 1..=6 {
            toml.push_str(&format!(
                "\n[agents.mine{n}]\ndispatch = \"mine{n} run {{prompt_file}} --model {{model}}\"\n\
                 model = \"m{n}\"\n"
            ));
        }
        std::fs::write(&agents_path, toml).unwrap();

        let store = Store::open_in_memory().unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '4');

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let render = |terminal: &mut Terminal<TestBackend>, app: &App| {
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

        let rendered = render(&mut terminal, &app);
        let hidden = app.config_agents_max_scroll.get();
        assert!(
            hidden > 0,
            "eight agents should overflow an 80x24 pane:\n{rendered}"
        );
        assert!(
            rendered.contains(&format!("0/{hidden} ↕ J/K PgDn/PgUp")),
            "the pane hides rows without saying so:\n{rendered}"
        );

        // Every agent is reachable: walk to the bottom a row at a time and the
        // last one — the one the fold ate — is on screen.
        let last = app
            .config_agents
            .last()
            .expect("agents are configured")
            .name
            .clone();
        assert!(!rendered.contains(&last), "{rendered}");
        for _ in 0..hidden {
            app.on_key(KeyEvent::from(KeyCode::Char('J')));
        }
        assert_eq!(app.config_agents_scroll, hidden, "J clamps at the bottom");
        let rendered = render(&mut terminal, &app);
        assert!(rendered.contains(&last), "{rendered}");
        assert!(
            rendered.contains(&format!("{hidden}/{hidden} ↕ J/K PgDn/PgUp")),
            "{rendered}"
        );

        // `K` walks back, and the two lists below keep their own `j`/`k`.
        app.on_key(KeyEvent::from(KeyCode::PageUp));
        assert!(app.config_agents_scroll < hidden);
        let before = app.config_sel;
        app.on_key(KeyEvent::from(KeyCode::Char('j')));
        assert_ne!(app.config_sel, before, "j still moves the row selection");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The settings pane at an ordinary terminal (DESIGN.md §5/§9): every
    /// setting shows its value and where it came from, the viewers list below
    /// still has its border and a row, and the selection the two lists share
    /// highlights exactly one line whichever list it is in.
    #[test]
    fn config_settings_pane_reads_at_80x24() {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::Store;

        let dir = std::env::temp_dir().join(format!(
            "voro-ui-config-settings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let agents_path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();
        // the cap and the default agent are the operator's; the default viewer
        // is whatever voro resolves, so the pane shows both provenances at once
        std::fs::write(
            &agents_path,
            "default_agent = \"claude\"\nmax_running = 2\n\n[viewers.zed]\ncmd = \"zed {path}\"\n",
        )
        .unwrap();

        let store = Store::open_in_memory().unwrap();
        let ctx = crate::dispatch::DispatchCtx {
            db_path: dir.join("voro.db"),
            agents_path,
            runtime_dir: dir.join("sessions"),
            ref_capture_timeout: std::time::Duration::ZERO,
            message_grace: std::time::Duration::from_millis(300),
        };
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '4');

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        // The lines of the frame, and the ones the selection has reversed.
        let read = |terminal: &Terminal<TestBackend>| -> (Vec<String>, Vec<usize>) {
            let buffer = terminal.backend().buffer();
            let mut lines = Vec::new();
            let mut highlighted = Vec::new();
            for y in 0..buffer.area.height {
                let mut line = String::new();
                let mut reversed = false;
                for x in 0..buffer.area.width {
                    let cell = &buffer[(x, y)];
                    line.push_str(cell.symbol());
                    reversed |= cell.modifier.contains(Modifier::REVERSED);
                }
                if reversed {
                    highlighted.push(y as usize);
                }
                lines.push(line);
            }
            (lines, highlighted)
        };
        let draw_now = |terminal: &mut Terminal<TestBackend>, app: &App| {
            terminal
                .draw(|f| {
                    draw(f, app);
                })
                .unwrap();
        };

        draw_now(&mut terminal, &app);
        let (lines, highlighted) = read(&terminal);
        let find = |needle: &str| {
            lines
                .iter()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle}:\n{}", lines.join("\n")))
                .clone()
        };
        assert!(find("Settings").contains("⏎ edit"));
        let cap = find("dispatch cap");
        assert!(cap.contains(" 2 ") && cap.contains("(voro.toml)"), "{cap}");
        let agent = find("default agent");
        assert!(
            agent.contains("claude") && agent.contains("(voro.toml)"),
            "{agent}"
        );
        // resolved rather than chosen: a sole `[viewers.*]` table is the default
        let viewer = find("default viewer");
        assert!(
            viewer.contains("zed") && viewer.contains("(the only viewer configured)"),
            "{viewer}"
        );
        // the viewers list keeps its border and a row of its own
        assert!(find("Viewers").contains("a add · ⏎ edit · d delete"));
        assert!(lines.iter().any(|l| l.contains("zed {path}")));

        // the selection starts on the first setting, and moving it into the
        // viewers list moves the one highlight there — never two at once
        assert_eq!(highlighted.len(), 1, "{highlighted:?}");
        let first = highlighted[0];
        assert!(read(&terminal).0[first].contains("default agent"));

        for _ in 0..app.config_settings.len() {
            app.move_selection(1);
        }
        draw_now(&mut terminal, &app);
        let (lines, highlighted) = read(&terminal);
        assert_eq!(highlighted.len(), 1, "{highlighted:?}");
        assert!(
            lines[highlighted[0]].contains(&app.config_viewers[0].name),
            "{}",
            lines[highlighted[0]]
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
