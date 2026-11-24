//! Rendering for a project's intent lines (DESIGN.md §3/§9): the drill-down
//! popup and its add/edit form.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{Hit, HitMap, SELECTED, popup_area};
use crate::app::{App, IntentFormState};
use crate::markdown::wrap_lines;

/// The widest label column, so one long label cannot squeeze every sentence.
const LABEL_CAP: usize = 16;

/// The drill-down: each line numbered by the precedence agents read it in,
/// its label in a column, its sentence wrapped beside it. A pending line takes
/// no number, since no agent reads it, and carries a `pending` tag.
pub(super) fn draw_intent(
    frame: &mut Frame,
    app: &App,
    project_id: i64,
    sel: usize,
    hits: &mut HitMap,
) {
    let lines = app.intent_lines(project_id);
    let name = app
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map_or("", |p| p.name.as_str());
    let width = frame.area().width.saturating_sub(4).min(100);
    let inner_width = width.saturating_sub(2) as usize;

    let label_width = lines
        .iter()
        .map(|l| Span::raw(l.label.as_str()).width())
        .max()
        .unwrap_or(0)
        .min(LABEL_CAP);
    let indent = 4 + label_width + 2;
    let sentence_width = inner_width.saturating_sub(indent).max(10);

    // Each item's visual rows, and which item every row belongs to.
    let mut rows: Vec<Line> = Vec::new();
    let mut owner: Vec<usize> = Vec::new();
    let mut rank = 0;
    for (i, line) in lines.iter().enumerate() {
        let number = if line.accepted {
            rank += 1;
            format!("{rank:>2}. ")
        } else {
            "  · ".to_string()
        };
        let mut sentence = vec![Span::raw(line.sentence.clone())];
        if !line.accepted {
            sentence.push(Span::styled("  pending", Style::new().fg(Color::Yellow)));
        }
        let wrapped = wrap_lines(vec![Line::from(sentence)], sentence_width);
        let label_style = if line.accepted {
            Style::new().add_modifier(Modifier::BOLD)
        } else {
            Style::new().add_modifier(Modifier::ITALIC)
        };
        for (k, chunk) in wrapped.into_iter().enumerate() {
            let mut spans = if k == 0 {
                vec![
                    Span::raw(number.clone()),
                    Span::styled(format!("{:label_width$}  ", line.label), label_style),
                ]
            } else {
                vec![Span::raw(" ".repeat(indent))]
            };
            spans.extend(chunk.spans);
            let mut row = Line::from(spans);
            if i == sel {
                row = row.patch_style(SELECTED);
            }
            rows.push(row);
            owner.push(i);
        }
    }
    if lines.is_empty() {
        rows.push(Line::from(Span::styled(
            "no intent lines yet — a adds one; agents propose them with `voro intent propose`",
            Style::new().dim(),
        )));
    }

    let height = (rows.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
    let area = popup_area(frame, width, height.max(3));
    let visible = area.height.saturating_sub(2) as usize;
    // Scroll just far enough that the selected item's rows are on screen.
    let first = owner.iter().position(|&o| o == sel).unwrap_or(0);
    let last = owner.iter().rposition(|&o| o == sel).unwrap_or(0);
    let scroll = if last >= visible {
        (last + 1 - visible).min(first)
    } else {
        0
    };

    let mut keys = "a add · ⏎ edit · d remove · J/K move".to_string();
    if lines.iter().any(|l| !l.accepted) {
        keys.push_str(" · y accept");
    }
    keys.push_str(" · esc close");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!("Intent — {name} · earlier lines win"))
        .title_bottom(Line::from(keys).dim());
    frame.render_widget(
        Paragraph::new(rows).block(block).scroll((scroll as u16, 0)),
        area,
    );

    let inner = area.inner(Margin::new(1, 1));
    for r in 0..visible {
        if let Some(&item) = owner.get(scroll + r) {
            hits.push(
                Rect::new(inner.x, inner.y + r as u16, inner.width, 1),
                Hit::PickerOption(item),
            );
        }
    }
}

/// The add/edit form: a label and a sentence, the sentence wrapping as it
/// grows so the whole line stays in view while it is written.
pub(super) fn draw_intent_form(frame: &mut Frame, form: &IntentFormState) {
    let width = frame.area().width.saturating_sub(4).min(100);
    let inner_width = width.saturating_sub(2) as usize;
    let field = |label: &str, value: &str, active: bool| {
        let style = if active {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new()
        };
        Line::from(vec![
            Span::raw(format!("{label:>8}: ")),
            Span::styled(format!("{value}▏"), style),
        ])
    };
    let mut rows = vec![field("label", &form.label, !form.on_sentence)];
    rows.extend(wrap_lines(
        vec![field("sentence", &form.sentence, form.on_sentence)],
        inner_width,
    ));
    let title = match form.editing {
        Some(_) => "Edit intent line — tab to switch, ⏎ to save, esc to cancel",
        None => "New intent line — tab to switch, ⏎ to save, esc to cancel",
    };
    let area = popup_area(frame, width, rows.len() as u16 + 2);
    frame.render_widget(
        Paragraph::new(rows).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use voro_core::Store;

    use crate::app::Mode;
    use crate::ui::draw;
    use crate::ui::tests::{screen_text, test_app};

    /// Accepted lines are numbered in precedence order; a pending line is
    /// tagged and unnumbered; a long sentence wraps rather than being cut.
    #[test]
    fn the_drill_down_numbers_accepted_lines_and_tags_pending_ones() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        store.add_intent_line(p, "what", "one screen").unwrap();
        store
            .propose_intent_line(p, "glance", "read at a glance")
            .unwrap();
        store
            .add_intent_line(
                p,
                "why",
                "agents produce work faster than Michael can attend to it, so Voro spends \
                 his attention where it unblocks the most work",
            )
            .unwrap();
        let mut app = test_app(store);
        app.mode = Mode::Intent {
            project_id: p,
            sel: 0,
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|f| {
                draw(f, &app);
            })
            .unwrap();
        let out = screen_text(&terminal);
        assert!(out.contains(" 1. what"), "{out}");
        assert!(out.contains("· glance"), "{out}");
        assert!(out.contains("pending"), "{out}");
        assert!(out.contains(" 2. why"), "{out}");
        assert!(out.contains("unblocks the most work"), "{out}");
        assert!(out.contains("y accept"), "{out}");
    }
}
