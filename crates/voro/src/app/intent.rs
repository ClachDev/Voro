//! A project's intent lines (DESIGN.md §3/§9): the drill-down the projects
//! screen opens on ⏎, and the form that adds or edits one line.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::IntentLine;

use super::{App, Mode};

/// The add/edit form over one intent line. `sel` is the drill-down's cursor
/// to return to; `editing` is the line being replaced, `None` for a new one.
#[derive(Clone)]
pub struct IntentFormState {
    pub project_id: i64,
    pub sel: usize,
    pub editing: Option<i64>,
    pub label: String,
    pub sentence: String,
    pub on_sentence: bool,
}

impl App {
    /// A project's lines in precedence order, accepted and pending.
    pub fn intent_lines(&self, project_id: i64) -> &[IntentLine] {
        self.intent.get(&project_id).map_or(&[], Vec::as_slice)
    }

    /// How many of a project's lines wait on the operator's acceptance.
    pub fn intent_pending(&self, project_id: i64) -> usize {
        self.intent_lines(project_id)
            .iter()
            .filter(|l| !l.accepted)
            .count()
    }

    /// Whether the projects screen's selection has any intent line, which is
    /// what puts ⏎ on the key line: a project with none shows nothing new, and
    /// the key map still names ⏎ for writing the first.
    pub fn selected_project_has_intent(&self) -> bool {
        self.projects
            .get(self.projects_sel)
            .is_some_and(|p| !self.intent_lines(p.id).is_empty())
    }

    pub(super) fn open_intent(&mut self, project_id: i64) {
        self.mode = Mode::Intent { project_id, sel: 0 };
    }

    /// The drill-down's keys: `a` adds an accepted line, ⏎ or `e` edits the
    /// selected one, `d` removes it (discarding a proposal is the same
    /// removal), `y` accepts a pending one, and `K`/`J` move it up or down a
    /// place, the cursor following. Esc closes.
    pub(super) fn key_intent(&mut self, key: KeyEvent, project_id: i64, mut sel: usize) {
        let lines = self.intent_lines(project_id).to_vec();
        let selected = lines.get(sel).cloned();
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(lines.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Char('a') => {
                self.mode = Mode::IntentForm(IntentFormState {
                    project_id,
                    sel,
                    editing: None,
                    label: String::new(),
                    sentence: String::new(),
                    on_sentence: false,
                });
                return;
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(line) = selected {
                    self.mode = Mode::IntentForm(IntentFormState {
                        project_id,
                        sel,
                        editing: Some(line.id),
                        label: line.label,
                        sentence: line.sentence,
                        on_sentence: false,
                    });
                    return;
                }
            }
            KeyCode::Char('d') => {
                if let Some(line) = selected {
                    let result = self
                        .store
                        .remove_intent_line(line.id)
                        .and_then(|_| self.refresh());
                    if self.report(result).is_some() {
                        let verb = if line.accepted {
                            "removed"
                        } else {
                            "discarded"
                        };
                        self.status = Some(format!("{verb} intent line '{}'", line.label));
                    }
                    sel = sel.min(self.intent_lines(project_id).len().saturating_sub(1));
                }
            }
            KeyCode::Char('y') => {
                if let Some(line) = selected {
                    let result = self
                        .store
                        .accept_intent_line(line.id)
                        .and_then(|_| self.refresh());
                    if self.report(result).is_some() {
                        self.status =
                            Some(format!("accepted '{}' — agents now read it", line.label));
                    }
                }
            }
            KeyCode::Char(c @ ('K' | 'J')) => {
                if let Some(line) = selected {
                    let up = c == 'K';
                    let result = self.store.move_intent_line(line.id, up);
                    match result {
                        Ok(true) => {
                            sel = if up { sel - 1 } else { sel + 1 };
                            let result = self.refresh();
                            self.report(result);
                        }
                        Ok(false) => {
                            self.status = Some(format!(
                                "'{}' is already {}",
                                line.label,
                                if up { "first" } else { "last" }
                            ));
                        }
                        Err(e) => self.status = Some(e.to_string()),
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::Intent { project_id, sel };
    }

    /// The form's keys: tab switches field, ⏎ on the label advances to the
    /// sentence and ⏎ on the sentence saves. Esc returns to the drill-down
    /// without saving; a refused save keeps the form open with what was typed.
    pub(super) fn key_intent_form(&mut self, key: KeyEvent, mut form: IntentFormState) {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Intent {
                    project_id: form.project_id,
                    sel: form.sel,
                };
                return;
            }
            KeyCode::Tab => form.on_sentence = !form.on_sentence,
            KeyCode::Enter if !form.on_sentence => form.on_sentence = true,
            KeyCode::Enter => {
                let result = match form.editing {
                    Some(id) => self.store.edit_intent_line(id, &form.label, &form.sentence),
                    None => {
                        self.store
                            .add_intent_line(form.project_id, &form.label, &form.sentence)
                    }
                };
                match result {
                    Ok(line) => {
                        let result = self.refresh();
                        self.report(result);
                        let sel = self
                            .intent_lines(form.project_id)
                            .iter()
                            .position(|l| l.id == line.id)
                            .unwrap_or(form.sel);
                        self.mode = Mode::Intent {
                            project_id: form.project_id,
                            sel,
                        };
                        return;
                    }
                    Err(e) => self.status = Some(e.to_string()),
                }
            }
            KeyCode::Backspace => {
                if form.on_sentence {
                    form.sentence.pop();
                } else {
                    form.label.pop();
                }
            }
            KeyCode::Char(c) => {
                if form.on_sentence {
                    form.sentence.push(c);
                } else {
                    form.label.push(c);
                }
            }
            _ => {}
        }
        self.mode = Mode::IntentForm(form);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Screen;
    use crate::app::tests::{alt_key, app_with, key, type_str};

    fn open(app: &mut App) -> i64 {
        alt_key(app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        key(app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Intent { .. }));
        app.projects[0].id
    }

    fn add(app: &mut App, label: &str, sentence: &str) {
        key(app, KeyCode::Char('a'));
        type_str(app, label);
        key(app, KeyCode::Enter);
        type_str(app, sentence);
        key(app, KeyCode::Enter);
        assert!(
            matches!(app.mode, Mode::Intent { .. }),
            "saving returns to the list: {:?}",
            app.status
        );
    }

    fn labels(app: &App, project: i64) -> Vec<String> {
        app.store
            .intent_lines(project)
            .unwrap()
            .into_iter()
            .map(|l| l.label)
            .collect()
    }

    /// Every edit the operator can make — add, edit, reorder, remove — is
    /// reachable from the projects screen without the CLI.
    #[test]
    fn the_drill_down_adds_edits_reorders_and_removes() {
        let mut app = app_with(&[]);
        let p = open(&mut app);

        add(&mut app, "what", "one screen");
        add(&mut app, "why", "attention");
        add(&mut app, "control", "never decides");
        assert_eq!(labels(&app, p), ["what", "why", "control"]);
        assert!(matches!(app.mode, Mode::Intent { sel: 2, .. }));

        key(&mut app, KeyCode::Char('K'));
        assert_eq!(labels(&app, p), ["what", "control", "why"]);
        assert!(matches!(app.mode, Mode::Intent { sel: 1, .. }));
        key(&mut app, KeyCode::Char('J'));
        key(&mut app, KeyCode::Char('J'));
        assert_eq!(labels(&app, p), ["what", "why", "control"]);
        assert!(app.status.as_deref().unwrap().contains("already last"));

        // Edit the first line's sentence.
        key(&mut app, KeyCode::Char('k'));
        key(&mut app, KeyCode::Char('k'));
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Tab);
        for _ in 0.."one screen".len() {
            key(&mut app, KeyCode::Backspace);
        }
        type_str(&mut app, "one terminal screen");
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.store.intent_lines(p).unwrap()[0].render(),
            "what: one terminal screen"
        );

        key(&mut app, KeyCode::Char('d'));
        assert_eq!(labels(&app, p), ["why", "control"]);

        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.screen, Screen::Projects);
    }

    #[test]
    fn a_pending_line_is_accepted_with_y() {
        let mut app = app_with(&[]);
        let p = app.projects[0].id;
        app.store.add_intent_line(p, "what", "one screen").unwrap();
        app.store
            .propose_intent_line(p, "glance", "at a glance")
            .unwrap();
        app.refresh().unwrap();
        assert_eq!(app.intent_pending(p), 1);

        open(&mut app);
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Char('y'));
        assert_eq!(app.intent_pending(p), 0);
        assert_eq!(app.store.accepted_intent(p).unwrap().len(), 2);
    }

    #[test]
    fn a_refused_save_keeps_the_form_and_what_was_typed() {
        let mut app = app_with(&[]);
        open(&mut app);
        add(&mut app, "why", "attention");
        key(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "WHY");
        key(&mut app, KeyCode::Enter);
        type_str(&mut app, "again");
        key(&mut app, KeyCode::Enter);
        match &app.mode {
            Mode::IntentForm(form) => assert_eq!(form.sentence, "again"),
            _ => panic!("a refused save should keep the form open"),
        }
        assert!(app.status.as_deref().unwrap().contains("already has"));
    }
}
