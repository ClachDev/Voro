//! The projects screen (DESIGN.md §9): its keys, the add-and-rename form, and
//! the per-project viewer picker.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::AgentsConfig;

use super::{App, Mode, ViewerOption, viewer_label};

impl App {
    /// The projects screen's local keys (DESIGN.md §9). `0`–`5` sets the
    /// selected project's weight — the one place a bare digit means weight
    /// rather than a task's priority; `r` opens the AddProject form pre-filled
    /// to rename/re-path, `a` opens it blank, `d` deletes behind the store's own
    /// guard (only projects with no tasks), `v` picks the viewer, `A`
    /// toggles archived (DESIGN.md §5). Movement and screen switching are
    /// handled by `key_normal`.
    pub(super) fn key_projects(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c @ '0'..='5') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let id = project.id;
                    let result = self
                        .store
                        .set_weight(id, c.to_digit(10).unwrap() as i64)
                        .and_then(|_| self.refresh());
                    self.report(result);
                }
            }
            KeyCode::Char('r') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    self.mode = Mode::AddProject {
                        name: project.name.clone(),
                        path: self.project_path(project.id).to_string(),
                        on_path: false,
                        editing: Some(project.id),
                    };
                }
            }
            KeyCode::Char('a') => {
                self.mode = Mode::AddProject {
                    name: String::new(),
                    path: String::new(),
                    on_path: false,
                    editing: None,
                };
            }
            KeyCode::Char('d') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let id = project.id;
                    let result = self.store.delete_project(id).and_then(|_| self.refresh());
                    self.report(result);
                    self.projects_sel =
                        self.projects_sel.min(self.projects.len().saturating_sub(1));
                }
            }
            KeyCode::Char('v') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let (id, current) = (project.id, project.viewer.clone());
                    self.open_viewer_picker(id, current);
                }
            }
            KeyCode::Char('A') => {
                if let Some(project) = self.projects.get(self.projects_sel) {
                    let (id, name, to) = (project.id, project.name.clone(), !project.archived);
                    let result = self.store.set_archived(id, to).and_then(|_| self.refresh());
                    if self.report(result).is_some() {
                        self.status = Some(if to {
                            format!("'{name}' archived — hidden from the cockpit with its tasks")
                        } else {
                            format!("'{name}' unarchived — its tasks are back as they were")
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// Open the viewer picker for a project (DESIGN.md §8/§11a): the default
    /// viewer, then each named viewer from `voro.toml`. The config is loaded
    /// fresh so a just-added `[viewers.*]` table shows up; the cursor starts on
    /// the viewer the project names.
    fn open_viewer_picker(&mut self, project_id: i64, current: Option<String>) {
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let mut options = vec![ViewerOption::Viewer(None)];
        // The built-ins are offered among the named viewers: a project may pin
        // one with no table defining it (DESIGN.md §11a).
        options.extend(
            config
                .viewer_entries()
                .into_iter()
                .map(|(name, ..)| ViewerOption::Viewer(Some(name.to_string()))),
        );
        // The quick path (DESIGN.md §5): a trailing entry that opens the
        // add-viewer form and pins this project to the viewer it creates.
        options.push(ViewerOption::NewViewer);
        let sel = options
            .iter()
            .position(|o| matches!(o, ViewerOption::Viewer(v) if *v == current))
            .unwrap_or(0);
        self.mode = Mode::ViewerPicker {
            project_id,
            options,
            current,
            sel,
        };
    }

    /// Drive the viewer picker: ⏎ stores the highlighted viewer via
    /// `set_viewer` and refreshes so the projects row reflects it;
    /// esc cancels without touching anything.
    pub(super) fn key_viewer_picker(
        &mut self,
        key: KeyEvent,
        project_id: i64,
        options: Vec<ViewerOption>,
        current: Option<String>,
        mut sel: usize,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(options.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                match options.get(sel) {
                    Some(ViewerOption::Viewer(viewer)) => {
                        let viewer = viewer.clone();
                        let result = self
                            .store
                            .set_viewer(project_id, viewer.as_deref())
                            .and_then(|_| self.refresh());
                        if self.report(result).is_some() {
                            self.status =
                                Some(format!("viewer -> {}", viewer_label(viewer.as_deref())));
                        }
                    }
                    // Open the shared add-viewer form; on success it pins this
                    // project to the new viewer (DESIGN.md §5).
                    Some(ViewerOption::NewViewer) => {
                        self.open_viewer_form(None, Some(project_id));
                    }
                    None => {}
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::ViewerPicker {
            project_id,
            options,
            current,
            sel,
        };
    }

    pub(super) fn key_add_project(
        &mut self,
        key: KeyEvent,
        mut name: String,
        mut path: String,
        on_path: bool,
        editing: Option<i64>,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab => {
                self.mode = Mode::AddProject {
                    name,
                    path,
                    on_path: !on_path,
                    editing,
                };
                return;
            }
            KeyCode::Enter => {
                if !on_path {
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path: true,
                        editing,
                    };
                    return;
                }
                if name.trim().is_empty() {
                    self.status = Some("project name is required".into());
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path,
                        editing,
                    };
                    return;
                }
                let result = match editing {
                    Some(id) => self
                        .store
                        .rename_project(id, name.trim())
                        .and_then(|_| self.store.set_default_repo_path(id, path.trim()))
                        .map(|_| ())
                        .and_then(|_| self.refresh()),
                    None => self
                        .store
                        .create_project(name.trim(), path.trim())
                        .and_then(|_| self.refresh()),
                };
                if self.report(result).is_none() {
                    self.mode = Mode::AddProject {
                        name,
                        path,
                        on_path,
                        editing,
                    };
                }
                return;
            }
            KeyCode::Backspace => {
                if on_path {
                    path.pop();
                } else {
                    name.pop();
                }
            }
            KeyCode::Char(c) => {
                if on_path {
                    path.push(c);
                } else {
                    name.push(c);
                }
            }
            _ => {}
        }
        self.mode = Mode::AddProject {
            name,
            path,
            on_path,
            editing,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Screen;
    use crate::app::config::ViewerFormState;
    use crate::app::tests::{alt_key, app_with, key, scratch_env, type_str};
    use voro_core::TaskState;

    /// The quick path (DESIGN.md §5): the projects screen's viewer picker
    /// grows a "new viewer…" entry that opens the add-viewer form and, on
    /// success, pins the project to the viewer it created.
    #[test]
    fn viewer_picker_new_viewer_creates_and_pins_it() {
        let (mut store, ctx, project_path) = scratch_env("config-quickpath", None);
        let project = store
            .create_project("demo", project_path.to_str().unwrap())
            .unwrap();
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        // onto the projects screen, open the viewer picker
        alt_key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);
        key(&mut app, KeyCode::Char('v'));
        let n = match &app.mode {
            Mode::ViewerPicker { options, .. } => options.len(),
            _ => panic!("expected the viewer picker to open"),
        };
        // the last option is "new viewer…"; move to it and select
        for _ in 0..n {
            key(&mut app, KeyCode::Char('j'));
        }
        key(&mut app, KeyCode::Enter);
        assert!(
            matches!(
                app.mode,
                Mode::ViewerForm(ViewerFormState {
                    review_project: Some(_),
                    ..
                })
            ),
            "new viewer… should open the form carrying the project"
        );
        type_str(&mut app, "emacs");
        key(&mut app, KeyCode::Enter);
        type_str(&mut app, "emacsclient {path}");
        key(&mut app, KeyCode::Enter);

        // the viewer exists and the project is now pinned to it
        assert!(
            AgentsConfig::load(&path)
                .unwrap()
                .viewer_names()
                .contains(&"emacs".to_string())
        );
        assert_eq!(
            app.store.project(project.id).unwrap().viewer.as_deref(),
            Some("emacs")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The morning ritual: `0`–`5` on the projects screen sets the selected
    /// project's weight through the store in a single keystroke — and nothing
    /// about that keystroke can be mistaken for a screen jump, which is the
    /// collision this binding scheme exists to remove (DESIGN.md §9).
    #[test]
    fn digit_on_projects_screen_sets_weight_through_the_store() {
        let mut app = app_with(&[]);
        let project_id = app.projects[0].id;
        alt_key(&mut app, KeyCode::Char('3'));
        assert_eq!(app.screen, Screen::Projects);

        key(&mut app, KeyCode::Char('5'));
        assert_eq!(app.projects[0].weight, 5);
        assert_eq!(app.store.project(project_id).unwrap().weight, 5);

        key(&mut app, KeyCode::Char('0'));
        assert_eq!(app.store.project(project_id).unwrap().weight, 0);

        // Every value 0–5 is reachable, and no bare digit leaves the screen.
        for digit in ['1', '2', '3', '4', '5'] {
            key(&mut app, KeyCode::Char(digit));
            assert_eq!(
                app.screen,
                Screen::Projects,
                "bare {digit} should weight the project, not switch screens"
            );
            let expected = digit.to_digit(10).unwrap() as i64;
            assert_eq!(app.store.project(project_id).unwrap().weight, expected);
        }

        // The modifier is the other half of the bargain: it jumps without
        // touching the weight it passes over.
        alt_key(&mut app, KeyCode::Char('1'));
        assert_eq!(app.screen, Screen::Cockpit);
        assert_eq!(app.store.project(project_id).unwrap().weight, 5);
        alt_key(&mut app, KeyCode::Char('3'));
        alt_key(&mut app, KeyCode::Char('2'));
        assert_eq!(app.screen, Screen::Tasks);
        assert_eq!(app.store.project(project_id).unwrap().weight, 5);
    }

    #[test]
    fn projects_screen_rename_prefills_and_saves() {
        let mut app = app_with(&[]);
        let project_id = app.projects[0].id;
        alt_key(&mut app, KeyCode::Char('3'));

        key(&mut app, KeyCode::Char('r'));
        match &app.mode {
            Mode::AddProject {
                name,
                path,
                editing,
                ..
            } => {
                assert_eq!(name, "demo");
                assert_eq!(path, "/tmp/demo");
                assert_eq!(*editing, Some(project_id));
            }
            _ => panic!("r on the projects screen should open the AddProject modal prefilled"),
        }

        for _ in 0.."demo".len() {
            key(&mut app, KeyCode::Backspace);
        }
        for c in "renamed".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter); // move to the path field
        for _ in 0.."/tmp/demo".len() {
            key(&mut app, KeyCode::Backspace);
        }
        for c in "/tmp/moved".chars() {
            key(&mut app, KeyCode::Char(c));
        }
        key(&mut app, KeyCode::Enter); // save

        // saving closes the form, matching the create-project flow
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.projects[0].id, project_id);
        assert_eq!(app.projects[0].name, "renamed");
        // the same form re-paths in one save
        assert_eq!(app.project_path(project_id), "/tmp/moved");
        // tasks reference the project by id, so renaming leaves them intact
        let stored = app.store.project(project_id).unwrap();
        assert_eq!(stored.name, "renamed");
        assert_eq!(
            app.store.default_repo(project_id).unwrap().path,
            "/tmp/moved"
        );
    }

    /// `a` opens a blank AddProject form to add a new project.
    #[test]
    fn projects_screen_add_opens_a_blank_form() {
        let mut app = app_with(&[]);
        alt_key(&mut app, KeyCode::Char('3'));
        key(&mut app, KeyCode::Char('a'));
        match &app.mode {
            Mode::AddProject {
                name,
                path,
                editing,
                ..
            } => {
                assert!(name.is_empty());
                assert!(path.is_empty());
                assert_eq!(*editing, None);
            }
            _ => panic!("a on the projects screen should open a blank AddProject form"),
        }
    }

    /// `A` on the projects screen toggles archived (DESIGN.md §5): the
    /// project's tasks leave the queue and counts wholesale, states untouched,
    /// and toggle back exactly as they were.
    #[test]
    fn projects_screen_archive_toggles_and_the_cockpit_empties() {
        let mut app = app_with(&[TaskState::Ready, TaskState::NeedsInput]);
        let project_id = app.projects[0].id;
        assert_eq!(app.queue.rows.len(), 2);
        alt_key(&mut app, KeyCode::Char('3'));

        key(&mut app, KeyCode::Char('A'));
        assert!(app.store.project(project_id).unwrap().archived);
        assert!(app.projects[0].archived);
        assert!(app.queue.rows.is_empty());
        assert_eq!(app.counts.ready, 0);
        assert_eq!(app.counts.needs_input, 0);
        // the tasks froze rather than transitioned
        assert_eq!(app.all[0].task.state, TaskState::NeedsInput);

        key(&mut app, KeyCode::Char('A'));
        assert!(!app.store.project(project_id).unwrap().archived);
        assert_eq!(app.queue.rows.len(), 2);
        assert_eq!(app.counts.needs_input, 1);
    }

    #[test]
    fn projects_screen_deletes_a_taskless_project() {
        let mut app = app_with(&[]);
        alt_key(&mut app, KeyCode::Char('3'));
        key(&mut app, KeyCode::Char('d'));
        assert!(app.projects.is_empty());
        assert_eq!(app.screen, Screen::Projects);
        assert!(app.status.is_none());
    }

    #[test]
    fn projects_screen_delete_refuses_when_project_has_a_task() {
        let mut app = app_with(&[TaskState::Ready]);
        alt_key(&mut app, KeyCode::Char('3'));
        key(&mut app, KeyCode::Char('d'));
        assert_eq!(app.projects.len(), 1);
        assert!(app.status.as_deref().unwrap_or("").contains("park"));
    }
}
