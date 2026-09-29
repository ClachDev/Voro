//! Rendering for the modal modes (DESIGN.md §9): popups, pickers, forms and
//! the text-entry prompts.

use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};

use super::milestones;
use super::rows::{
    branch_span, deep_line, dep_lines, doc_lines, history_lines, human_line, pr_span,
    question_summary, repo_span, score_lines, session_lines,
};
use super::status::draw_key_map;
use super::{Hit, HitMap, SELECTED, popup_area};
use crate::app::{App, Mode, ViewerFormState, ViewerOption, viewer_label};

pub(super) fn draw_mode(frame: &mut Frame, app: &App, hits: &mut HitMap) {
    match &app.mode {
        Mode::Normal => {}
        Mode::KeyMap { page } => draw_key_map(frame, app, *page),
        Mode::AddProject {
            name,
            path,
            on_path,
            editing,
        } => {
            let area = popup_area(frame, 56, 4);
            let field = |label: &str, value: &str, active: bool| {
                let style = if active {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Line::from(vec![
                    Span::raw(format!("{label}: ")),
                    Span::styled(format!("{value}▏"), style),
                ])
            };
            let title = match editing {
                Some(id) => format!("Edit project #{id} — tab to switch, ⏎ to save"),
                None => "New project — tab to switch, ⏎ to save".to_string(),
            };
            let para = Paragraph::new(vec![
                field("name", name, !*on_path),
                field("path", path, *on_path),
            ])
            .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
        }
        Mode::PickProject { sel, flow } => {
            // Weight first, then name, as the projects screen lists them
            // — the order is weightiest-first (DESIGN.md §9), which reads as
            // arbitrary unless the number it sorts on is on the row.
            let items: Vec<ListItem> = app
                .creatable_projects()
                .iter()
                .map(|p| ListItem::new(format!("{:>2}  {}", p.weight, p.name)))
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 48, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            use crate::app::{CreateFlow, Filing};
            let title = match flow {
                CreateFlow::Quick(Filing::Task) => "Project to propose a task in",
                CreateFlow::Editor(Filing::Task) => "Project for the new task",
                CreateFlow::Plan(Filing::Task) => "Project to plan a task in",
                CreateFlow::Quick(Filing::Milestone) => "Project to propose a milestone in",
                CreateFlow::Editor(Filing::Milestone) => "Project for the new milestone",
                CreateFlow::Plan(Filing::Milestone) => "Project to plan a milestone in",
            };
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(title))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::Transition {
            task_id,
            actions,
            sel,
        } => {
            let milestone = app
                .all
                .iter()
                .any(|r| r.task.id == *task_id && r.task.milestone);
            let items: Vec<ListItem> = actions
                .iter()
                .map(|a| ListItem::new(crate::app::transition_label(a, milestone)))
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 48, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Transition #{task_id}")),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::Prompt { kind, buffer, .. } => draw_text_entry_popup(
            frame,
            format!("{} — ⏎ to submit, esc to cancel", kind.title()),
            buffer,
        ),
        Mode::LinkPr { buffer, .. } => draw_text_entry_popup(
            frame,
            "Link PR (URL or owner/repo#n) — ⏎ to submit, esc to cancel".to_string(),
            buffer,
        ),
        Mode::EditMaxRunning { buffer } => draw_text_entry_popup(
            frame,
            "Dispatch cap — how many run at once, 0 for none — ⏎ to save, esc to cancel"
                .to_string(),
            buffer,
        ),
        Mode::QuickCreate {
            project_id,
            filing,
            buffer,
        } => {
            let project = app
                .projects
                .iter()
                .find(|p| p.id == *project_id)
                .map(|p| p.name.as_str())
                .unwrap_or("the project");
            draw_text_entry_popup(
                frame,
                match filing {
                    crate::app::Filing::Task => {
                        format!("New task in {project} — ⏎ to propose, esc to cancel")
                    }
                    crate::app::Filing::Milestone => {
                        format!("New milestone in {project} — ⏎ to propose, esc to cancel")
                    }
                },
                buffer,
            )
        }
        Mode::ConfirmPr {
            task_id,
            branch,
            title,
        } => {
            let lines = vec![
                Line::from(vec![
                    Span::raw("push branch "),
                    Span::styled(format!("`{branch}`"), Style::new().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::raw("create a ready PR titled "),
                    Span::styled(format!("“{title}”"), Style::new().fg(Color::Blue)),
                ]),
                Line::from(Span::raw("and open it in the browser")),
                Line::default(),
                Line::from(Span::styled(
                    "⏎/y confirm · esc/n cancel",
                    Style::new().dim(),
                )),
            ];
            let area = popup_area(frame, 72, lines.len() as u16 + 2);
            let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Open a PR for #{task_id}?")),
            );
            frame.render_widget(para, area);
        }
        Mode::Detail { task_id, scroll } => {
            let Some(row) = app.all.iter().find(|r| r.task.id == *task_id) else {
                return;
            };
            let frame_area = frame.area();
            let width = frame_area.width.saturating_sub(8).clamp(30, 90);
            let height = frame_area.height.saturating_sub(4).clamp(8, 40);
            let area = popup_area(frame, width, height);
            let t = &row.task;
            let mut lines = vec![
                Line::from(Span::styled(t.title.clone(), Style::new().bold())),
                Line::from(Span::styled(
                    format!(
                        "{} · {} · {} · w{}",
                        row.project, t.priority, t.state, row.weight
                    ),
                    Style::new().dim(),
                )),
            ];
            if let Some(q) = &t.question {
                lines.push(Line::from(Span::styled(
                    format!("question: {}", question_summary(q)),
                    Style::new().fg(Color::Cyan),
                )));
            }
            if let Some(pr) = &t.pr_url {
                lines.push(Line::from(pr_span(pr)));
            }
            if let Some(branch) = &t.branch {
                lines.push(Line::from(branch_span(branch)));
            }
            if let Some((name, path)) = app.task_repo(t) {
                lines.push(Line::from(repo_span(&name, &path)));
            }
            if t.human {
                lines.push(human_line());
            }
            if t.deep {
                lines.push(deep_line());
            }
            lines.extend(milestones::detail_lines(app, *task_id));
            if let Some(session) = app.last_sessions.get(task_id) {
                lines.extend(session_lines(session, t.state));
            }
            lines.extend(doc_lines(app, *task_id));
            lines.extend(dep_lines(
                app.deps.get(task_id).map_or(&[][..], |v| v),
                app.dependents.get(task_id).map_or(&[][..], |v| v),
            ));
            if app.show_score
                && let Some(b) = app.score_breakdown(*task_id)
            {
                lines.extend(score_lines(&b));
            }
            lines.push(Line::default());
            lines.extend(crate::markdown::body_lines(&t.body));
            if app.show_history {
                lines.push(Line::default());
                lines.extend(history_lines(&app.task_events(*task_id)));
            }
            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "#{task_id} — ⏎ state · 0-3 priority · ! deep · c docs · m milestones · x score · h history · j/k scroll · esc close"
                )));
            frame.render_widget(para, area);
        }
        Mode::AgentPicker {
            task_id,
            agents,
            resolved,
            sel,
        } => {
            let items: Vec<ListItem> = agents
                .iter()
                .map(|a| {
                    if resolved.as_deref() == Some(a.as_str()) {
                        ListItem::new(format!("{a}  (resolved)"))
                    } else {
                        ListItem::new(a.clone())
                    }
                })
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Dispatch #{task_id} — pick agent, ⏎ dispatch, esc cancel"
                )))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::DocPicker {
            task_id, docs, sel, ..
        } => {
            let items: Vec<ListItem> = docs
                .iter()
                .map(|doc| ListItem::new(doc_picker_row(app, *task_id, doc)))
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            // Resolved locations are absolute paths, so this picker takes what
            // the terminal will give rather than the fixed width the short-row
            // pickers use.
            let width = frame.area().width.saturating_sub(4).max(44);
            let area = popup_area(frame, width, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Documents for #{task_id} — ⏎ link/unlink, esc close"
                )))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::MilestonePicker {
            task_id,
            milestones,
            sel,
            ..
        } => {
            let items: Vec<ListItem> = milestones
                .iter()
                .map(|m| ListItem::new(milestones::picker_row(app, *task_id, m)))
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 64, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "Milestones for #{task_id} — ⏎ attach/detach, esc close"
                )))
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::ViewerPicker {
            options,
            current,
            sel,
            ..
        } => {
            let items: Vec<ListItem> = options
                .iter()
                .map(|o| match o {
                    ViewerOption::Viewer(v) if v == current => {
                        ListItem::new(format!("{}  (current)", viewer_label(v.as_deref())))
                    }
                    ViewerOption::Viewer(v) => {
                        ListItem::new(viewer_label(v.as_deref()).to_string())
                    }
                    ViewerOption::NewViewer => ListItem::new(Line::from(Span::styled(
                        "new viewer…",
                        Style::new().fg(Color::Blue),
                    ))),
                })
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 52, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Viewer — ⏎ set, esc cancel"),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
        Mode::ViewerForm(ViewerFormState {
            name,
            cmd,
            on_cmd,
            editing,
            cmd_tracks_name,
            ..
        }) => {
            let field = |label: &str, value: &str, active: bool| {
                let style = if active {
                    Style::new().add_modifier(Modifier::REVERSED)
                } else {
                    Style::new()
                };
                Line::from(vec![
                    Span::raw(format!("{label:>7}: ")),
                    Span::styled(format!("{value}▏"), style),
                ])
            };
            let area = popup_area(frame, 72, 4);
            let title = if *editing {
                format!("Edit viewer '{name}' — ⏎ to save, esc to cancel")
            } else {
                "New viewer — tab to switch, ⏎ to advance/save, esc to cancel".to_string()
            };
            // The name field is inert on an edit, so dim it to say so.
            let name_line = if *editing {
                Line::from(vec![
                    Span::raw("   name: "),
                    Span::styled(name.clone(), Style::new().dim()),
                ])
            } else {
                field("name", name, !*on_cmd)
            };
            // A command the form wrote is dim, focused or not, so that what was
            // typed and what was filled in never look alike.
            let mut cmd_style = Style::new();
            if *on_cmd {
                cmd_style = cmd_style.add_modifier(Modifier::REVERSED);
            }
            if *cmd_tracks_name {
                cmd_style = cmd_style.dim();
            }
            let cmd_line = Line::from(vec![
                Span::raw("command: "),
                Span::styled(format!("{cmd}▏"), cmd_style),
            ]);
            let para = Paragraph::new(vec![name_line, cmd_line])
                .block(Block::default().borders(Borders::ALL).title(title));
            frame.render_widget(para, area);
        }
        Mode::DefaultPicker {
            kind,
            names,
            current,
            sel,
        } => {
            let items: Vec<ListItem> = names
                .iter()
                .map(|n| {
                    if Some(n) == current.as_ref() {
                        ListItem::new(format!("{n}  (current)"))
                    } else {
                        ListItem::new(n.clone())
                    }
                })
                .collect();
            let count = items.len();
            let height = items.len() as u16 + 2;
            let area = popup_area(frame, 44, height.max(3));
            let mut state = ListState::default().with_selected(Some(*sel));
            let what = match kind {
                crate::app::DefaultKind::Agent => "default agent",
                crate::app::DefaultKind::Viewer => "default viewer",
            };
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Pick {what} — ⏎ set, esc cancel")),
                )
                .highlight_style(SELECTED);
            frame.render_stateful_widget(list, area, &mut state);
            hits.push_list(area, state.offset(), count, Hit::PickerOption);
        }
    }
}

/// One row of the document picker (DESIGN.md §8): a tick for the documents the
/// task already cites, then the same title-and-location pair the detail panes
/// show, so a link made here reads back identically. A document owned by
/// another project carries that project's name, since the list spans them all
/// and two plans can share a filename.
fn doc_picker_row(app: &App, task_id: i64, doc: &voro_core::Doc) -> Line<'static> {
    let linked = app.doc_linked(task_id, doc.id);
    let owner = app
        .all
        .iter()
        .find(|row| row.task.id == task_id)
        .filter(|row| row.task.project_id != doc.project_id)
        .and_then(|_| app.projects.iter().find(|p| p.id == doc.project_id))
        .map(|p| format!("[{}] ", p.name))
        .unwrap_or_default();
    let location = app
        .doc_locations
        .get(&doc.id)
        .cloned()
        .unwrap_or_else(|| doc.location.clone());
    let text = match &doc.title {
        Some(title) => format!("{owner}{title} — {location}"),
        None => format!("{owner}{location}"),
    };
    let (mark, style) = if linked {
        ("✓ ", Style::new().fg(Color::Magenta))
    } else {
        ("  ", Style::new().dim())
    };
    Line::from(vec![Span::styled(mark, style), Span::styled(text, style)])
}

/// A bordered popup holding one typed buffer, with the cursor at its end. The
/// box grows with the wrapped text and, past the clamp, scrolls to the tail.
fn draw_text_entry_popup(frame: &mut Frame, title: String, buffer: &str) {
    const WIDTH: u16 = 72;
    const MIN_ROWS: u16 = 3;
    const MAX_ROWS: u16 = 20;

    // The buffer is usually one line, but a RejectWork prompt can be pre-filled
    // with a PR's multi-line review comments (DESIGN.md §11c), so render every
    // line and grow the box to fit.
    let mut lines: Vec<Line> = buffer
        .split('\n')
        .map(|l| Line::from(l.to_string()))
        .collect();
    match lines.last_mut() {
        Some(last) => last.spans.push(Span::raw("▏")),
        None => lines.push(Line::from("▏")),
    }
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(title));
    // Typed text never contains a newline (⏎ submits), so the box has to be
    // sized from the *wrapped* line count at the popup's inner width —
    // counting `\n`s would leave a one-row box with the tail of a long note
    // wrapped out of sight. `line_count` counts both, and includes the block's
    // two border rows.
    let width = WIDTH.min(frame.area().width);
    let rendered_rows = para.line_count(width.saturating_sub(2)) as u16;
    let area = popup_area(frame, width, rendered_rows.clamp(MIN_ROWS, MAX_ROWS));
    // Past the clamp the box stops growing, so scroll to the end of the text:
    // the cursor lives there, and the tail is what is being typed.
    let overflow = rendered_rows
        .saturating_sub(2)
        .saturating_sub(area.height.saturating_sub(2));
    frame.render_widget(para.scroll((overflow, 0)), area);
}

#[cfg(test)]
mod tests {
    use crate::ui::draw;
    use crate::ui::tests::{alt_screen, app_with_status, screen_text};
    use voro_core::{Priority, TaskState};

    /// The picker carries the weight it sorts on, so weightiest-first reads as
    /// an order rather than a shuffle, and an archived project — which could
    /// not take the task anyway (DESIGN.md §5) — is not on it at all.
    #[test]
    fn the_project_picker_shows_weights_and_hides_the_archived() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = app_with_status("", 0, 0);
        let heavy = app.store.create_project("zeta", "/tmp/zeta").unwrap();
        app.store.set_weight(heavy.id, 4).unwrap();
        let retired = app.store.create_project("retired", "/tmp/retired").unwrap();
        app.store.set_weight(retired.id, 5).unwrap();
        app.store.set_archived(retired.id, true).unwrap();
        app.refresh().unwrap();
        app.mode = crate::app::Mode::PickProject {
            sel: 0,
            flow: crate::app::CreateFlow::Quick(crate::app::Filing::Task),
        };

        let mut terminal = Terminal::new(TestBackend::new(110, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let text = screen_text(&terminal);
        assert!(text.contains("4 zeta"), "{text}");
        assert!(text.contains("3 voro"), "{text}");
        assert!(!text.contains("retired"), "{text}");
    }

    /// End-to-end: on the tasks screen the same sections fold into the Detail
    /// popup — `x`/`h` inside the popup drive the same shared flags — so score
    /// and history render inline on this screen too, never as separate popups.
    #[test]
    fn tasks_detail_popup_folds_in_score_and_history_when_toggled() {
        use crate::app::{App, Mode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        store.set_weight(p.id, 3).unwrap();
        store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "a task".into(),
                body: "body".into(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        alt_screen(&mut app, '2'); // tasks screen
        app.on_key(KeyEvent::from(KeyCode::Enter)); // open the Detail popup
        app.on_key(KeyEvent::from(KeyCode::Char('x')));
        app.on_key(KeyEvent::from(KeyCode::Char('h')));
        assert!(matches!(app.mode, Mode::Detail { .. }));

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
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
            rendered.contains("base w×(p+s+u)"),
            "score decomposition should fold into the Detail popup: {rendered}"
        );
        assert!(
            rendered.contains("History") && rendered.contains("created"),
            "history should fold into the Detail popup: {rendered}"
        );
    }

    /// The create-PR modal spells out every consequence of confirming, the
    /// browser jump included (DESIGN.md §8), so the key's second half is not a
    /// surprise.
    #[test]
    fn confirm_pr_modal_announces_the_browser_jump() {
        use crate::app::{App, Mode};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use voro_core::{NewTask, Store};

        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        let task = store
            .create_task(NewTask {
                project_id: p.id,
                repo_id: None,
                title: "ship it".into(),
                body: String::new(),
                priority: Priority::P2,
                state: TaskState::Ready,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap();

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.mode = Mode::ConfirmPr {
            task_id: task.id,
            branch: "feat/ship".into(),
            title: "ship it".into(),
        };

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
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
            rendered.contains("open it in the browser"),
            "modal should name the browser jump: {rendered}"
        );
    }

    /// A prompt buffer wider than the popup wraps, and the box grows with the
    /// wrapped text so the tail being typed — and the cursor — stay on screen.
    #[test]
    fn prompt_popup_grows_with_wrapped_text() {
        let buffer = format!("HEADMARK {}TAILMARK", "padding ".repeat(12));
        let rendered = render_prompt(&buffer);
        assert!(
            rendered.contains("HEADMARK"),
            "the start of a wrapped note stays visible: {rendered}"
        );
        assert!(
            rendered.contains("TAILMARK▏"),
            "the tail and the cursor stay visible: {rendered}"
        );
    }

    /// Past the height clamp the popup stops growing, so it scrolls to the end
    /// of the text: the tail shows, the head scrolls away.
    #[test]
    fn prompt_popup_scrolls_to_the_tail_when_it_overflows() {
        let buffer = format!("HEADMARK {}TAILMARK", "padding ".repeat(200));
        let rendered = render_prompt(&buffer);
        assert!(
            rendered.contains("TAILMARK▏"),
            "the tail and the cursor stay visible: {rendered}"
        );
        assert!(
            !rendered.contains("HEADMARK"),
            "the head scrolls out of the clamped box: {rendered}"
        );
    }

    /// The quick-create popup shares the prompt's box, so a long intent wraps
    /// and the popup grows to fit it rather than clipping at three rows.
    #[test]
    fn quick_create_popup_grows_with_wrapped_text() {
        let buffer = format!("HEADMARK {}TAILMARK", "padding ".repeat(12));
        let rendered = render_quick_create(&buffer);
        assert!(
            rendered.contains("HEADMARK"),
            "the start of a wrapped intent stays visible: {rendered}"
        );
        assert!(
            rendered.contains("TAILMARK▏"),
            "the tail and the cursor stay visible: {rendered}"
        );
    }

    /// And past the clamp it scrolls to the tail, as the prompt does.
    #[test]
    fn quick_create_popup_scrolls_to_the_tail_when_it_overflows() {
        let buffer = format!("HEADMARK {}TAILMARK", "padding ".repeat(200));
        let rendered = render_quick_create(&buffer);
        assert!(
            rendered.contains("TAILMARK▏"),
            "the tail and the cursor stay visible: {rendered}"
        );
        assert!(
            !rendered.contains("HEADMARK"),
            "the head scrolls out of the clamped box: {rendered}"
        );
    }

    /// Draws a `Mode::Prompt` over an otherwise empty app.
    fn render_prompt(buffer: &str) -> String {
        use crate::app::{Mode, PromptKind};
        use voro_core::Store;

        render_modal(
            Store::open_in_memory().unwrap(),
            Mode::Prompt {
                task_id: 1,
                kind: PromptKind::RefineNote,
                buffer: buffer.to_string(),
            },
        )
    }

    /// Draws a `Mode::QuickCreate` over an app whose store holds the project the
    /// popup's title names.
    fn render_quick_create(buffer: &str) -> String {
        use crate::app::Mode;
        use voro_core::Store;

        let mut store = Store::open_in_memory().unwrap();
        let project = store.create_project("voro", "/tmp/voro").unwrap();
        render_modal(
            store,
            Mode::QuickCreate {
                project_id: project.id,
                filing: crate::app::Filing::Task,
                buffer: buffer.to_string(),
            },
        )
    }

    /// Draws a modal over the given store and returns the terminal's cells as
    /// one string.
    fn render_modal(store: voro_core::Store, mode: crate::app::Mode) -> String {
        use crate::app::App;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let ctx = crate::dispatch::DispatchCtx::from_db_path(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        let mut app = App::new(store, ctx).unwrap();
        app.mode = mode;

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    }
}
