//! The Config screen (DESIGN.md §9): its settings and viewer rows, its keys,
//! and the forms and pickers that edit viewers, defaults and the dispatch cap.

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use voro_core::AgentsConfig;

use super::{
    App, ConfigAgentRow, ConfigRow, ConfigSettingRow, ConfigViewerRow, DETAIL_PAGE_STEP, Mode,
    SettingKind,
};

/// Which global default a [`Mode::DefaultPicker`] is setting (DESIGN.md §5):
/// `default_agent` or `default_viewer`, both pick-from-list on the Config screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultKind {
    Agent,
    Viewer,
}

/// One option in the project's viewer picker (DESIGN.md §8/§11a). Beyond the
/// viewers themselves — `None` for the config default, then each named one —
/// the trailing `NewViewer` entry opens the add-viewer form and pins the
/// project to the viewer it creates, first-time viewer setup without a detour
/// through the Config screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerOption {
    Viewer(Option<String>),
    NewViewer,
}

/// How a project's viewer choice reads on screen and at the shell: the name it
/// pins, or the config default when it names none (DESIGN.md §8).
pub fn viewer_label(viewer: Option<&str>) -> &str {
    viewer.unwrap_or("default viewer")
}

/// The add/edit-viewer form's fields, which always travel together: the two
/// values being edited, which field the cursor is on, whether this is an edit
/// (which locks the name), the project to pin on success, and whether the
/// command is still following the name.
#[derive(Clone)]
pub struct ViewerFormState {
    pub name: String,
    pub cmd: String,
    pub on_cmd: bool,
    pub editing: bool,
    pub review_project: Option<i64>,
    /// Whether the command is still *following* the name — rewritten to
    /// `<name> {path}` on every keystroke in the name field, so the
    /// operator watches the line they are about to save assemble itself
    /// (DESIGN.md §5). Writing in the command field decouples it, and
    /// emptying that field couples it again, which is the whole undo:
    /// nothing they typed is ever overwritten, and nothing they did not
    /// type is ever kept against their will. Always false on an edit,
    /// where the command already exists and is theirs.
    pub cmd_tracks_name: bool,
}

impl App {
    /// Reload the Config screen's `voro.toml` view (DESIGN.md §5). A parse
    /// failure is held in `config_error` and shown on the screen; the agent and
    /// dispatch paths load the file independently, so this only feeds rendering.
    pub(super) fn load_config_view(&mut self, config: voro_core::Result<AgentsConfig>) {
        let config = match config {
            Ok(config) => config,
            Err(e) => {
                self.config_agents.clear();
                self.config_settings.clear();
                self.config_viewers.clear();
                self.config_rows.clear();
                self.config_anon_viewer = None;
                self.config_warnings.clear();
                self.config_error = Some(e.to_string());
                return;
            }
        };
        let default_agent = config.default_name();
        self.config_agents = config
            .entries()
            .map(|(name, template, provenance)| {
                let verbs = template.verbs();
                ConfigAgentRow {
                    name: name.to_string(),
                    dispatch: template.dispatch().to_string(),
                    provenance: provenance.label(),
                    is_default: Some(name) == default_agent.as_deref(),
                    verbs,
                    missing_verbs: config.override_missing_verbs(name),
                    models: template.model().map(|model| {
                        (
                            model.to_string(),
                            template.model_deep().unwrap_or(model).to_string(),
                            template.model_plan().unwrap_or(model).to_string(),
                        )
                    }),
                }
            })
            .collect();
        let default_viewer = config.default_viewer_name();
        self.config_viewers = config
            .viewer_entries()
            .into_iter()
            .map(|(name, cmd, provenance)| ConfigViewerRow {
                is_default: Some(name) == default_viewer.as_deref(),
                name: name.to_string(),
                cmd: cmd.to_string(),
                provenance: provenance.label(),
                editable: provenance != voro_core::Provenance::BuiltIn,
            })
            .collect();
        self.config_anon_viewer = config.anonymous_viewer_cmd().map(str::to_string);
        self.config_settings = Self::settings_rows(&config, default_agent, default_viewer);
        self.config_rows = (0..self.config_settings.len())
            .map(ConfigRow::Setting)
            .chain((0..self.config_viewers.len()).map(ConfigRow::Viewer))
            .collect();
        self.config_warnings = config.warnings();
        self.config_error = None;
    }

    /// The settings rows in the order the screen lists them, each carrying the
    /// value in force and where it came from (DESIGN.md §5). A value the file
    /// names reads `voro.toml`; one it does not names the rule that resolved it,
    /// so a fresh install can see that the cap and the defaults it is running
    /// under are Voro's own rather than something it was never asked about.
    fn settings_rows(
        config: &AgentsConfig,
        default_agent: Option<String>,
        default_viewer: Option<String>,
    ) -> Vec<ConfigSettingRow> {
        // Nothing resolves at all: no agent on PATH, or no viewer anywhere.
        const NONE: &str = "—";

        let (agent_value, agent_source) = match config.default_agent_from_file() {
            Some(name) => (name.to_string(), "voro.toml".to_string()),
            None => match default_agent {
                Some(name) => (name, "first agent found on PATH".to_string()),
                None => (NONE.into(), "no agent found on PATH".to_string()),
            },
        };
        // The same order `AgentsConfig::default_viewer_name` resolves in: the
        // key, then the anonymous table, then a sole named viewer, then the
        // built-ins probed against PATH.
        let (viewer_value, viewer_source) = match config.default_viewer_from_file() {
            Some(name) => (name.to_string(), "voro.toml".to_string()),
            None if config.anonymous_viewer_cmd().is_some() => {
                ("[viewer]".into(), "the anonymous table".to_string())
            }
            None => match default_viewer {
                Some(name) if config.viewer_names().len() == 1 => {
                    (name, "the only viewer configured".to_string())
                }
                Some(name) => (name, "first viewer found on PATH".to_string()),
                None => (NONE.into(), "no viewer found on PATH".to_string()),
            },
        };
        let cap_source = match config.max_running_from_file() {
            Some(_) => "voro.toml",
            None => "voro's default",
        };
        vec![
            ConfigSettingRow {
                name: "default agent",
                value: agent_value,
                source: agent_source,
                kind: SettingKind::Default(DefaultKind::Agent),
            },
            ConfigSettingRow {
                name: "default viewer",
                value: viewer_value,
                source: viewer_source,
                kind: SettingKind::Default(DefaultKind::Viewer),
            },
            ConfigSettingRow {
                name: "dispatch cap",
                value: config.max_running().to_string(),
                source: cap_source.to_string(),
                kind: SettingKind::MaxRunning,
            },
        ]
    }

    /// What the Config screen's selection is on, if anything.
    pub fn selected_config_row(&self) -> Option<ConfigRow> {
        self.config_rows.get(self.config_sel).copied()
    }

    /// The selected viewer, `None` when the selection is on a setting — so the
    /// viewer actions read the row they act on rather than indexing blind.
    pub fn selected_viewer(&self) -> Option<&ConfigViewerRow> {
        match self.selected_config_row()? {
            ConfigRow::Viewer(i) => self.config_viewers.get(i),
            ConfigRow::Setting(_) => None,
        }
    }

    /// The selected setting, `None` when the selection is on a viewer.
    pub fn selected_setting(&self) -> Option<&ConfigSettingRow> {
        match self.selected_config_row()? {
            ConfigRow::Setting(i) => self.config_settings.get(i),
            ConfigRow::Viewer(_) => None,
        }
    }

    /// Scroll the Config screen's agents pane, clamped the same way against the
    /// overflow `draw_config` last measured.
    fn scroll_config_agents(&mut self, delta: i64) {
        let max = self.config_agents_max_scroll.get() as i64;
        self.config_agents_scroll = (self.config_agents_scroll as i64 + delta).clamp(0, max) as u16;
    }

    /// Drive the dispatch-cap entry (DESIGN.md §5/§7). Enter saves, esc cancels,
    /// backspace edits. Only the characters a whole number is spelled with are
    /// taken, so a stray letter never lands in the field — what it cannot refuse
    /// at the keystroke (a lone `-`, a count past `i64`, a negative cap) the
    /// save refuses with the form still open.
    pub(super) fn key_max_running(&mut self, key: KeyEvent, mut buffer: String) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.save_max_running(&buffer);
                return;
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) if c.is_ascii_digit() || c == '-' => buffer.push(c),
            _ => {}
        }
        self.mode = Mode::EditMaxRunning { buffer };
    }

    /// Write the dispatch cap and rebuild the queue on the same keypress, so a
    /// raised cap restores the dispatch rows the capacity line had replaced
    /// (DESIGN.md §7) rather than waiting for the next unrelated refresh. A
    /// refusal — from the parse or from the writer — keeps the entry open with
    /// what was typed intact, the way the link-a-PR prompt does.
    fn save_max_running(&mut self, raw: &str) {
        let trimmed = raw.trim();
        let reopen = |app: &mut App, message: String| {
            app.status = Some(message);
            app.mode = Mode::EditMaxRunning {
                buffer: raw.to_string(),
            };
        };
        let Ok(n) = trimmed.parse::<i64>() else {
            reopen(
                self,
                format!("'{trimmed}' is not a whole number — the dispatch cap counts tasks"),
            );
            return;
        };
        match voro_core::config_edit::set_max_running(&self.dispatch_ctx.agents_path, n) {
            Ok(()) => {
                self.status = Some(if n == 0 {
                    "dispatch cap -> 0 — the queue will offer no dispatches".into()
                } else {
                    format!("dispatch cap -> {n}")
                });
                let result = self.refresh();
                self.report(result);
            }
            Err(e) => reopen(self, e.to_string()),
        }
    }

    /// The Config screen's local keys (DESIGN.md §5): `e`/⏎ edits whatever the
    /// selection is on — a setting or a viewer — while `a` adds a viewer and `d`
    /// deletes one. The two list operations keep letters of their own because
    /// neither is an edit of the selected value: `a` acts with nothing selected,
    /// and `d` is destructive. `J`/`K` and the page keys scroll the agents pane,
    /// which has no selection of its own. Movement and the alt-digit screen
    /// jumps are `key_normal`'s.
    pub(super) fn key_config(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('a') => self.open_viewer_form(None, None),
            KeyCode::Char('e') | KeyCode::Enter => self.activate_config_row(),
            KeyCode::Char('d') => self.delete_selected_viewer(),
            // The agents pane takes the cockpit card's scroll keys for the same
            // reason it has there: `j`/`k` are the list's — here the settings
            // and the viewers' — so the pane above them is driven by the shifted
            // pair and the page keys (DESIGN.md §9).
            KeyCode::Char('J') => self.scroll_config_agents(1),
            KeyCode::Char('K') => self.scroll_config_agents(-1),
            KeyCode::PageDown => self.scroll_config_agents(DETAIL_PAGE_STEP),
            KeyCode::PageUp => self.scroll_config_agents(-DETAIL_PAGE_STEP),
            _ => {}
        }
    }

    /// Edit whatever the Config screen's selection is on: a setting opens its
    /// own editor — the picker for the two defaults, the numeric entry for the
    /// dispatch cap — and a viewer row opens the viewer form.
    fn activate_config_row(&mut self) {
        match self.selected_config_row() {
            Some(ConfigRow::Setting(i)) => match self.config_settings.get(i).map(|s| s.kind) {
                Some(SettingKind::Default(kind)) => self.open_default_picker(kind),
                Some(SettingKind::MaxRunning) => self.open_max_running_entry(),
                None => {}
            },
            Some(ConfigRow::Viewer(_)) => self.edit_selected_viewer(),
            None => self.status = Some("nothing selected — press a to add a viewer".into()),
        }
    }

    /// Open the dispatch-cap entry pre-filled with the cap in force, so raising
    /// or lowering it starts from the number the screen just showed.
    fn open_max_running_entry(&mut self) {
        let buffer = self
            .config_settings
            .iter()
            .find(|s| s.kind == SettingKind::MaxRunning)
            .map(|s| s.value.clone())
            .unwrap_or_default();
        self.mode = Mode::EditMaxRunning { buffer };
    }

    /// Open the add/edit-viewer form. `existing` pre-fills it for an edit (name
    /// locked); `review_project` threads through the quick path so a viewer
    /// created from the viewer picker becomes that project's viewer.
    pub(super) fn open_viewer_form(
        &mut self,
        existing: Option<(String, String)>,
        review_project: Option<i64>,
    ) {
        let (name, cmd, editing) = match existing {
            Some((name, cmd)) => (name, cmd, true),
            None => (String::new(), String::new(), false),
        };
        self.mode = Mode::ViewerForm(ViewerFormState {
            name,
            cmd,
            // An edit starts on the command field, since the name is fixed.
            on_cmd: editing,
            editing,
            review_project,
            // A new viewer's command follows its name until the operator
            // writes one; an edit's is already written.
            cmd_tracks_name: !editing,
        });
    }

    /// The command the form fills in for a name while it is still following it:
    /// the built-in's own line where the name is one, else `<name> {path}`
    /// (DESIGN.md §5). An empty name fills nothing rather than a bare
    /// placeholder, so the field starts empty and stays that way until there is
    /// something to run.
    fn tracked_viewer_cmd(name: &str) -> String {
        match name.trim().is_empty() {
            true => String::new(),
            false => voro_core::config_edit::assumed_viewer_cmd(name),
        }
    }

    /// Edit the selected viewer's command. A built-in has no table to edit —
    /// overriding it is an *add* of the same name — so it is refused with that
    /// named rather than opening a form whose write would fail.
    fn edit_selected_viewer(&mut self) {
        match self.selected_viewer() {
            Some(v) if !v.editable => {
                self.status = Some(format!(
                    "'{}' is built into voro — press a and name it '{}' to override it",
                    v.name, v.name
                ));
            }
            Some(v) => {
                let existing = (v.name.clone(), v.cmd.clone());
                self.open_viewer_form(Some(existing), None);
            }
            None => self.status = Some("no viewer selected — press a to add one".into()),
        }
    }

    /// Delete the selected viewer, refusing when a project still names it
    /// (DESIGN.md §5) — the same refusal as `voro viewer remove`, with
    /// the offending projects named. Deleting the default clears `default_viewer`.
    fn delete_selected_viewer(&mut self) {
        // A setting is changed, never removed: there is no state in which
        // voro has no dispatch cap, so `d` on one has nothing to do.
        if let Some(setting) = self.selected_setting() {
            self.status = Some(format!(
                "'{}' is a setting, not a list — press ⏎ to change it",
                setting.name
            ));
            return;
        }
        let Some(viewer) = self.selected_viewer() else {
            self.status = Some("no viewer selected".into());
            return;
        };
        if !viewer.editable {
            self.status = Some(format!(
                "'{}' is built into voro and cannot be deleted — press a and name it '{}' to \
                 override it",
                viewer.name, viewer.name
            ));
            return;
        }
        let name = viewer.name.clone();
        let referencing =
            voro_core::config_edit::projects_referencing_viewer(&self.projects, &name);
        if !referencing.is_empty() {
            let names: Vec<&str> = referencing.iter().map(|p| p.name.as_str()).collect();
            self.status = Some(format!(
                "'{name}' is the viewer of {} — repoint it first (v on the projects screen)",
                names.join(", ")
            ));
            return;
        }
        match voro_core::config_edit::delete_viewer(&self.dispatch_ctx.agents_path, &name) {
            Ok(cleared) => {
                self.status = Some(if cleared {
                    format!("viewer '{name}' deleted — was the default, default_viewer cleared")
                } else {
                    format!("viewer '{name}' deleted")
                });
                let result = self.refresh();
                self.report(result);
            }
            Err(e) => self.status = Some(e.to_string()),
        }
    }

    /// Open the default-agent/viewer picker (DESIGN.md §5), loading `voro.toml`
    /// fresh so a just-added viewer is offered. An empty set reports what to do.
    fn open_default_picker(&mut self, kind: DefaultKind) {
        let config = match AgentsConfig::load(&self.dispatch_ctx.agents_path) {
            Ok(config) => config,
            Err(e) => {
                self.status = Some(e.to_string());
                return;
            }
        };
        let (names, current) = match kind {
            DefaultKind::Agent => (config.agent_names(), config.default_name()),
            // The built-ins are offered too: a default naming one is exactly
            // what a fresh install wants to pin (DESIGN.md §11a).
            DefaultKind::Viewer => (
                config
                    .viewer_entries()
                    .into_iter()
                    .map(|(name, ..)| name.to_string())
                    .collect(),
                config.default_viewer_name(),
            ),
        };
        if names.is_empty() {
            self.status = Some(match kind {
                DefaultKind::Agent => "no agents are configured".into(),
                DefaultKind::Viewer => "no viewers to pick from — add one with a".into(),
            });
            return;
        }
        let sel = current
            .as_ref()
            .and_then(|c| names.iter().position(|n| n == c))
            .unwrap_or(0);
        self.mode = Mode::DefaultPicker {
            kind,
            names,
            current,
            sel,
        };
    }

    /// Drive the add/edit-viewer form. Tab toggles fields (an edit stays on the
    /// command, its name locked); ⏎ advances name → command on an add, then
    /// submits. A failed write keeps the form open with the error on the status
    /// line so a typo is fixable without retyping.
    pub(super) fn key_viewer_form(&mut self, key: KeyEvent, form: ViewerFormState) {
        let ViewerFormState {
            mut name,
            mut cmd,
            on_cmd,
            editing,
            review_project,
            mut cmd_tracks_name,
        } = form;
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Tab => {
                let on_cmd = if editing { true } else { !on_cmd };
                self.mode = Mode::ViewerForm(ViewerFormState {
                    name,
                    cmd,
                    on_cmd,
                    editing,
                    review_project,
                    cmd_tracks_name,
                });
                return;
            }
            KeyCode::Enter => {
                if !on_cmd && !editing {
                    self.mode = Mode::ViewerForm(ViewerFormState {
                        name,
                        cmd,
                        on_cmd: true,
                        editing,
                        review_project,
                        cmd_tracks_name,
                    });
                    return;
                }
                self.submit_viewer_form(name, cmd, editing, review_project);
                return;
            }
            KeyCode::Backspace => {
                match (on_cmd, cmd_tracks_name) {
                    // Nothing to delete in a command the form is writing: it
                    // is a suggestion, not text the operator put there.
                    (true, true) => {}
                    (true, false) => {
                        cmd.pop();
                    }
                    (false, _) => {
                        name.pop();
                    }
                }
            }
            KeyCode::Char(c) => {
                if on_cmd {
                    // The first character typed over a following command takes
                    // it over whole, rather than landing on the end of a line
                    // the operator never wrote.
                    if cmd_tracks_name {
                        cmd.clear();
                        cmd_tracks_name = false;
                    }
                    cmd.push(c);
                } else if !editing {
                    name.push(c);
                }
            }
            _ => {}
        }
        // Deleting back to an empty command hands it to the name again, so the
        // suggestion is recoverable with the same key that discarded it. While
        // it follows, it *is* the name's — re-derived on every keystroke — and
        // an empty command means the same thing to the writer either way.
        if on_cmd && !cmd_tracks_name && cmd.is_empty() {
            cmd_tracks_name = true;
        }
        if cmd_tracks_name {
            cmd = Self::tracked_viewer_cmd(&name);
        }
        self.mode = Mode::ViewerForm(ViewerFormState {
            name,
            cmd,
            on_cmd,
            editing,
            review_project,
            cmd_tracks_name,
        });
    }

    /// Write the viewer through the shared helper, then — for the quick path —
    /// pin the originating project to it, and refresh.
    fn submit_viewer_form(
        &mut self,
        name: String,
        cmd: String,
        editing: bool,
        review_project: Option<i64>,
    ) {
        // A blank command on an add is the common case, not a slip: it means
        // the obvious line for that name (DESIGN.md §5). Resolved here as well
        // as in the writer so the status line reports what was recorded.
        let cmd = match (editing, cmd.trim().is_empty()) {
            (false, true) => voro_core::config_edit::assumed_viewer_cmd(&name),
            _ => cmd,
        };
        let path = &self.dispatch_ctx.agents_path;
        let result = if editing {
            voro_core::config_edit::edit_viewer(path, &name, &cmd)
        } else {
            voro_core::config_edit::add_viewer(path, &name, &cmd)
        };
        if let Err(e) = result {
            self.status = Some(e.to_string());
            self.mode = Mode::ViewerForm(ViewerFormState {
                name,
                cmd,
                on_cmd: true,
                editing,
                review_project,
                // The command in hand is now what will be saved, whether the
                // form wrote it or the operator did, so a retry edits it
                // rather than watching it change under them.
                cmd_tracks_name: false,
            });
            return;
        }
        let trimmed = name.trim().to_string();
        let mut msg = if editing {
            format!("viewer '{trimmed}' updated")
        } else {
            // Name what was written, since on a blank command the operator
            // never typed it.
            format!("viewer '{trimmed}' added: {}", cmd.trim())
        };
        if voro_core::config_edit::missing_path_placeholder(&cmd) {
            msg.push_str(" (no {path} — runs in the checkout dir)");
        }
        if let Some(project_id) = review_project {
            match self.store.set_viewer(project_id, Some(&trimmed)) {
                Ok(_) => msg.push_str(" — set as this project's viewer"),
                Err(e) => msg = e.to_string(),
            }
        }
        self.status = Some(msg);
        let result = self.refresh();
        self.report(result);
        // Land the selection on what was just written, so `e`/`d` act on it
        // rather than on whichever row the list happens to sort first.
        if let Some(i) = self.config_viewers.iter().position(|v| v.name == trimmed) {
            self.config_sel = self.config_settings.len() + i;
        }
    }

    /// Drive the default-agent/viewer picker: ⏎ writes the choice through the
    /// shared helper and refreshes; esc cancels.
    pub(super) fn key_default_picker(
        &mut self,
        key: KeyEvent,
        kind: DefaultKind,
        names: Vec<String>,
        current: Option<String>,
        mut sel: usize,
    ) {
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(names.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => {
                let chosen = names[sel].clone();
                let path = &self.dispatch_ctx.agents_path;
                let result = match kind {
                    DefaultKind::Agent => voro_core::config_edit::set_default_agent(path, &chosen),
                    DefaultKind::Viewer => {
                        voro_core::config_edit::set_default_viewer(path, &chosen)
                    }
                };
                match result {
                    Ok(()) => {
                        self.status = Some(match kind {
                            DefaultKind::Agent => format!("default agent -> {chosen}"),
                            DefaultKind::Viewer => format!("default viewer -> {chosen}"),
                        });
                        let result = self.refresh();
                        self.report(result);
                    }
                    Err(e) => self.status = Some(e.to_string()),
                }
                return;
            }
            _ => {}
        }
        self.mode = Mode::DefaultPicker {
            kind,
            names,
            current,
            sel,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Screen;
    use crate::app::tests::{alt_key, key, scratch_env, type_str};
    use voro_core::{Action, NewTask, Priority, TaskState, scheduler};

    /// The Config screen's viewer-list index for a viewer. The built-ins share
    /// the list, so a test never assumes where a name sorts.
    fn row(app: &App, name: &str) -> usize {
        app.config_viewers
            .iter()
            .position(|v| v.name == name)
            .unwrap_or_else(|| panic!("no viewer row named {name}"))
    }

    /// …and the selection that lands on it, past the settings rows above.
    fn viewer_sel(app: &App, name: &str) -> usize {
        app.config_settings.len() + row(app, name)
    }

    /// The selection that lands on a named setting.
    fn setting_sel(app: &App, name: &str) -> usize {
        app.config_settings
            .iter()
            .position(|s| s.name == name)
            .unwrap_or_else(|| panic!("no setting row named {name}"))
    }

    /// The command field writes itself from the name until the operator writes
    /// one, and comes back when what they wrote is deleted. The name is
    /// the only thing they must know; the command is a suggestion they can
    /// watch, take over, or undo.
    #[test]
    fn the_form_command_follows_the_name_until_it_is_written() {
        let (store, ctx, _project) = scratch_env("config-follows", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();
        let form = |app: &App| -> (String, String, bool) {
            match &app.mode {
                Mode::ViewerForm(ViewerFormState {
                    name,
                    cmd,
                    cmd_tracks_name,
                    ..
                }) => (name.clone(), cmd.clone(), *cmd_tracks_name),
                _ => panic!("expected the viewer form"),
            }
        };

        alt_key(&mut app, KeyCode::Char('4'));
        key(&mut app, KeyCode::Char('a'));
        // an empty name fills nothing rather than a bare placeholder
        assert_eq!(form(&app), (String::new(), String::new(), true));

        // the command assembles itself keystroke by keystroke, backspace and all
        type_str(&mut app, "zed");
        assert_eq!(form(&app).1, "zed {path}");
        key(&mut app, KeyCode::Backspace);
        assert_eq!(form(&app).1, "ze {path}");

        // a name that is a built-in's follows that built-in's own line
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        type_str(&mut app, "code");
        assert_eq!(form(&app).1, "code -n {path}");

        // on the command field, backspace leaves a suggestion alone — there is
        // nothing there the operator typed
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(form(&app), ("code".into(), "code -n {path}".into(), true));

        // …and the first character typed takes the field over whole
        type_str(&mut app, "x");
        assert_eq!(form(&app), ("code".into(), "x".into(), false));
        type_str(&mut app, "y");
        assert_eq!(form(&app).1, "xy");

        // deleting back to empty hands it to the name again
        key(&mut app, KeyCode::Backspace);
        key(&mut app, KeyCode::Backspace);
        assert_eq!(form(&app), ("code".into(), "code -n {path}".into(), true));

        // a written command is not rewritten by a later name edit
        type_str(&mut app, "mine {path}");
        key(&mut app, KeyCode::Tab);
        type_str(&mut app, "r");
        assert_eq!(form(&app), ("coder".into(), "mine {path}".into(), false));

        // and what is saved is what the form showed (⏎ on the name advances,
        // ⏎ on the command saves)
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.config_viewers[row(&app, "coder")].cmd, "mine {path}");

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Naming an editor is enough: ⏎ through the command field records
    /// `<name> {path}`, and a name that is a built-in's records that built-in's
    /// own line, so an override starts from what it replaces.
    #[test]
    fn a_viewer_added_with_a_blank_command_gets_the_obvious_one() {
        let (store, ctx, _project) = scratch_env("config-blank-cmd", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        alt_key(&mut app, KeyCode::Char('4'));
        key(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "emacsclient");
        key(&mut app, KeyCode::Enter); // name -> command
        key(&mut app, KeyCode::Enter); // submit with the command blank
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(
            app.config_viewers[row(&app, "emacsclient")].cmd,
            "emacsclient {path}"
        );
        // the status names what was written, since it was never typed
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("emacsclient {path}"), "{status}");

        // and overriding a built-in reproduces it rather than guessing at it
        key(&mut app, KeyCode::Char('a'));
        type_str(&mut app, "code");
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Enter);
        let code = &app.config_viewers[row(&app, "code")];
        assert_eq!(code.cmd, "code -n {path}");
        assert_eq!(code.provenance, "user override");
        assert!(code.editable);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The Config screen (DESIGN.md §5): add, edit, set-default, and delete a
    /// viewer entirely through the TUI, each edit landing in `voro.toml` and
    /// reflected on the next refresh.
    #[test]
    fn config_screen_adds_edits_defaults_and_deletes_a_viewer() {
        let (store, ctx, _project) = scratch_env("config-crud", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        alt_key(&mut app, KeyCode::Char('4'));
        assert_eq!(app.screen, Screen::Config);
        // the built-in agents and viewers are both listed, the viewers'
        // built-in rows read-only
        assert!(app.config_agents.iter().any(|a| a.name == "claude"));
        assert!(
            app.config_viewers
                .iter()
                .any(|v| v.name == "code" && !v.editable && v.provenance == "built-in")
        );
        assert!(app.config_viewers.iter().all(|v| !v.editable));

        // e/d on a built-in row refuse, naming the override that replaces it
        app.config_sel = viewer_sel(&app, "code");
        for k in ['d', 'e'] {
            key(&mut app, KeyCode::Char(k));
            let status = app.status.clone().unwrap_or_default();
            assert!(status.contains("built into voro"), "{status}");
            assert!(status.contains("override"), "{status}");
            assert!(matches!(app.mode, Mode::Normal));
        }
        assert!(app.config_viewers.iter().any(|v| v.name == "code"));

        // add: a opens the form, name → Enter → command → Enter submits
        key(&mut app, KeyCode::Char('a'));
        assert!(matches!(
            app.mode,
            Mode::ViewerForm(ViewerFormState { editing: false, .. })
        ));
        type_str(&mut app, "mine");
        key(&mut app, KeyCode::Enter);
        type_str(&mut app, "mine {path}");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        // the selection lands on what was just written
        let selected = app
            .selected_viewer()
            .expect("the selection lands on a viewer");
        assert_eq!(selected.name, "mine");
        assert_eq!(selected.cmd, "mine {path}");
        assert_eq!(selected.provenance, "user");
        assert_eq!(
            AgentsConfig::load(&path)
                .unwrap()
                .viewer_cmd(Some("mine"))
                .unwrap(),
            "mine {path}"
        );

        // edit: e opens the form with the name locked; append to the command
        key(&mut app, KeyCode::Char('e'));
        assert!(matches!(
            app.mode,
            Mode::ViewerForm(ViewerFormState { editing: true, .. })
        ));
        type_str(&mut app, " --wait");
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            app.config_viewers[row(&app, "mine")].cmd,
            "mine {path} --wait"
        );

        // default: ⏎ on the default-viewer setting opens the picker over every
        // viewer, built-ins included; walk to the top and back down to `mine`,
        // since where the cursor starts depends on what is installed
        app.config_sel = setting_sel(&app, "default viewer");
        key(&mut app, KeyCode::Enter);
        let names = match &app.mode {
            Mode::DefaultPicker { names, .. } => names.clone(),
            _ => panic!("expected the default picker to open"),
        };
        assert!(names.iter().any(|n| n == "code"), "{names:?}");
        let target = names.iter().position(|n| n == "mine").unwrap();
        for _ in 0..names.len() {
            key(&mut app, KeyCode::Char('k'));
        }
        for _ in 0..target {
            key(&mut app, KeyCode::Char('j'));
        }
        key(&mut app, KeyCode::Enter);
        assert!(app.config_viewers[row(&app, "mine")].is_default);
        assert_eq!(
            AgentsConfig::load(&path)
                .unwrap()
                .default_viewer_name()
                .as_deref(),
            Some("mine")
        );

        // delete: d removes it and clears the now-dangling default
        app.config_sel = viewer_sel(&app, "mine");
        key(&mut app, KeyCode::Char('d'));
        assert!(app.config_viewers.iter().all(|v| v.name != "mine"));
        let config = AgentsConfig::load(&path).unwrap();
        assert!(config.viewer_names().is_empty());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_viewer")
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// One selection runs over both lists (DESIGN.md §5): `j`/`k` cross from the
    /// settings into the viewers and back without a focus key, ⏎ edits whichever
    /// kind it is on, and `d` — a list operation, not an edit — is refused on a
    /// setting with what to press instead.
    #[test]
    fn one_selection_runs_over_the_settings_and_the_viewers() {
        let toml = "[viewers.zed]\ncmd = \"zed {path}\"\n";
        let (store, ctx, _project) = scratch_env("config-rows", Some(toml));
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();
        alt_key(&mut app, KeyCode::Char('4'));

        // the settings come first, in the order the screen lists them
        let names: Vec<&str> = app.config_settings.iter().map(|s| s.name).collect();
        assert_eq!(names, ["default agent", "default viewer", "dispatch cap"]);
        assert_eq!(
            app.config_rows.len(),
            app.config_settings.len() + app.config_viewers.len()
        );

        // k at the top stays put; j walks off the last setting onto the first
        // viewer and back
        app.config_sel = 0;
        key(&mut app, KeyCode::Char('k'));
        assert_eq!(
            app.selected_setting().map(|s| s.name),
            Some("default agent")
        );
        for _ in 0..app.config_settings.len() {
            key(&mut app, KeyCode::Char('j'));
        }
        assert_eq!(app.selected_config_row(), Some(ConfigRow::Viewer(0)));
        key(&mut app, KeyCode::Char('k'));
        assert_eq!(app.selected_setting().map(|s| s.name), Some("dispatch cap"));

        // ⏎ routes by row kind: picker, picker, numeric entry, viewer form
        for (setting, opens) in [
            ("default agent", "agent picker"),
            ("default viewer", "viewer picker"),
            ("dispatch cap", "cap entry"),
        ] {
            app.config_sel = setting_sel(&app, setting);
            key(&mut app, KeyCode::Enter);
            match (&app.mode, opens) {
                (Mode::DefaultPicker { kind, .. }, "agent picker") => {
                    assert_eq!(*kind, DefaultKind::Agent)
                }
                (Mode::DefaultPicker { kind, .. }, "viewer picker") => {
                    assert_eq!(*kind, DefaultKind::Viewer)
                }
                (Mode::EditMaxRunning { .. }, "cap entry") => {}
                _ => panic!("{setting} did not open the {opens}"),
            }
            key(&mut app, KeyCode::Esc);
        }
        app.config_sel = viewer_sel(&app, "zed");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.mode,
            Mode::ViewerForm(ViewerFormState { editing: true, .. })
        ));
        key(&mut app, KeyCode::Esc);

        // d on a setting is refused: a setting is changed, never removed
        app.config_sel = setting_sel(&app, "dispatch cap");
        key(&mut app, KeyCode::Char('d'));
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("dispatch cap"), "{status}");
        assert!(status.contains("⏎"), "{status}");
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.config_viewers.iter().any(|v| v.name == "zed"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Each settings row says what is in force and where it came from
    /// (DESIGN.md §5), so a fresh install can tell a value it chose from one
    /// Voro fell back to — the distinction the resolved value alone cannot make
    /// when the operator's number happens to equal the default.
    #[test]
    fn the_settings_rows_carry_their_value_and_its_provenance() {
        let (store, ctx, _project) = scratch_env("config-provenance", None);
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();
        alt_key(&mut app, KeyCode::Char('4'));

        let source = |app: &App, name: &str| {
            app.config_settings
                .iter()
                .find(|s| s.name == name)
                .map(|s| (s.value.clone(), s.source.clone()))
                .unwrap()
        };
        // with no file at all, every value is Voro's own and says so
        let (value, from) = source(&app, "dispatch cap");
        assert_eq!(value, scheduler::DEFAULT_MAX_RUNNING.to_string());
        assert_eq!(from, "voro's default");
        assert_ne!(source(&app, "default agent").1, "voro.toml");
        assert_ne!(source(&app, "default viewer").1, "voro.toml");

        // and once the file names them, they are the operator's
        voro_core::config_edit::set_max_running(&path, 5).unwrap();
        voro_core::config_edit::set_default_agent(&path, "codex").unwrap();
        voro_core::config_edit::set_default_viewer(&path, "code").unwrap();
        app.refresh().unwrap();
        assert_eq!(
            source(&app, "dispatch cap"),
            ("5".into(), "voro.toml".into())
        );
        assert_eq!(
            source(&app, "default agent"),
            ("codex".into(), "voro.toml".into())
        );
        assert_eq!(
            source(&app, "default viewer"),
            ("code".into(), "voro.toml".into())
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// The dispatch cap is edited where it is shown (DESIGN.md §5/§7): the entry
    /// opens on the cap in force, a bad number is refused with the form still
    /// open, and a saved one is written *and* re-gates the queue on the same
    /// keypress — so raising it while the capacity line has replaced the
    /// dispatch rows brings them back without restarting the TUI.
    #[test]
    fn saving_the_dispatch_cap_writes_it_and_re_gates_the_queue() {
        let (mut store, ctx, _project) =
            scratch_env("config-cap", Some("# mine\nmax_running = 1\n"));
        let path = ctx.agents_path.clone();
        let project = store.create_project("demo", "/tmp/demo").unwrap();
        let mut ready = |title: &str| {
            store
                .create_task(NewTask {
                    project_id: project.id,
                    repo_id: None,
                    title: title.into(),
                    body: String::new(),
                    priority: Priority::P1,
                    state: TaskState::Ready,
                    agent: None,
                    human: false,
                    deep: false,
                    milestone: false,
                })
                .unwrap()
        };
        let running = ready("in flight");
        let waiting = ready("waiting for room");
        store.apply(running.id, Action::Start).unwrap();

        let mut app = App::new(store, ctx).unwrap();
        alt_key(&mut app, KeyCode::Char('4'));
        // one running against a cap of one: the queue offers no dispatch
        assert!(app.queue.at_capacity.is_some());
        assert!(!app.queue_task_ids().contains(&waiting.id));

        // ⏎ opens the entry on the cap in force
        app.config_sel = setting_sel(&app, "dispatch cap");
        key(&mut app, KeyCode::Enter);
        match &app.mode {
            Mode::EditMaxRunning { buffer } => assert_eq!(buffer, "1"),
            _ => panic!("expected the cap entry"),
        }

        // a number it cannot save is refused with what was typed still there
        key(&mut app, KeyCode::Backspace);
        type_str(&mut app, "-2");
        key(&mut app, KeyCode::Enter);
        let status = app.status.clone().unwrap_or_default();
        assert!(status.contains("cannot be negative"), "{status}");
        match &app.mode {
            Mode::EditMaxRunning { buffer } => assert_eq!(buffer, "-2"),
            _ => panic!("the refusal should keep the entry open"),
        }
        assert_eq!(AgentsConfig::load(&path).unwrap().max_running(), 1);

        // esc leaves the file exactly as it was
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(AgentsConfig::load(&path).unwrap().max_running(), 1);

        // and a good one writes, keeps the operator's comment, and re-gates the
        // queue on the same keypress
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Backspace);
        type_str(&mut app, "4");
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(AgentsConfig::load(&path).unwrap().max_running(), 4);
        assert!(
            std::fs::read_to_string(&path).unwrap().contains("# mine"),
            "the writer preserves what was already in the file"
        );
        assert!(app.queue.at_capacity.is_none());
        assert!(app.queue_task_ids().contains(&waiting.id));
        assert_eq!(
            app.config_settings
                .iter()
                .find(|s| s.name == "dispatch cap")
                .map(|s| (s.value.as_str(), s.source.as_str())),
            Some(("4", "voro.toml"))
        );

        // 0 is a cap, not an absence — the queue offers nothing at all
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Backspace);
        type_str(&mut app, "0");
        key(&mut app, KeyCode::Enter);
        assert_eq!(AgentsConfig::load(&path).unwrap().max_running(), 0);
        assert!(app.queue.at_capacity.is_some());

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Deleting a viewer a project still names is refused on the
    /// Config screen too, naming the project (DESIGN.md §5).
    #[test]
    fn config_screen_refuses_to_delete_a_referenced_viewer() {
        let toml = "[viewers.zed]\ncmd = \"zed {path}\"\n";
        let (mut store, ctx, _project) = scratch_env("config-ref", Some(toml));
        let project = store.create_project("demo2", "/tmp/demo2").unwrap();
        store.set_viewer(project.id, Some("zed")).unwrap();
        let path = ctx.agents_path.clone();
        let mut app = App::new(store, ctx).unwrap();

        alt_key(&mut app, KeyCode::Char('4'));
        app.config_sel = viewer_sel(&app, "zed");
        assert!(app.selected_viewer().is_some_and(|v| v.editable));
        key(&mut app, KeyCode::Char('d'));
        assert!(
            app.status.as_deref().unwrap_or("").contains("demo2"),
            "refusal should name the project: {:?}",
            app.status
        );
        // still there, in the file and the view
        assert!(app.config_viewers.iter().any(|v| v.name == "zed"));
        assert!(
            AgentsConfig::load(&path)
                .unwrap()
                .viewer_names()
                .contains(&"zed".to_string())
        );

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
