//! The Milestones tab and the milestone parts of the other screens (DESIGN.md
//! §9): the tab's list and keys, the browser's grouping, and the picker `m`
//! opens. The tab's list and keys live here together so the screen can merge
//! into another as a unit.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use voro_core::{DepKind, MilestoneMembers, Task, TaskState};

use super::{App, BrowserRow, CreateFlow, Filing, Mode, transition_actions};

/// The tab's order: ready first, since a ready milestone is the operator's
/// move, then proposed ones awaiting a verdict, then parked, then closed.
fn tab_order(state: TaskState) -> u8 {
    match state {
        TaskState::Ready => 0,
        TaskState::Proposed => 1,
        TaskState::Parked => 2,
        TaskState::Done => 4,
        TaskState::Rejected => 5,
        _ => 3,
    }
}

impl App {
    /// Reload the milestones, each task's nearest ones, and the browser rows
    /// grouped on them. Runs inside `refresh`, after `all` is loaded.
    pub(super) fn load_milestones(&mut self) -> voro_core::Result<()> {
        let mut milestones = self.store.milestones(true)?;
        milestones.sort_by_key(|m| (tab_order(m.milestone.state), m.milestone.id));
        self.milestones = milestones;
        self.milestone_of = self.store.milestone_ids_by_task()?;
        self.browser_rows = self.build_browser_rows();
        Ok(())
    }

    pub fn milestone(&self, id: i64) -> Option<&MilestoneMembers> {
        self.milestones.iter().find(|m| m.milestone.id == id)
    }

    /// A task's nearest milestones, in id order (DESIGN.md §3).
    pub fn milestones_of(&self, task_id: i64) -> Vec<&Task> {
        self.milestone_of
            .get(&task_id)
            .map_or(&[][..], |ids| ids)
            .iter()
            .filter_map(|id| Some(&self.milestone(*id)?.milestone))
            .collect()
    }

    /// The members of one browser fold, as indices into `all` in browse
    /// order. The unattached fold holds every task that belongs to no
    /// milestone and heads none.
    pub fn group_members(&self, group: Option<i64>) -> Vec<usize> {
        match group {
            Some(id) => {
                let Some(m) = self.milestone(id) else {
                    return Vec::new();
                };
                let mut rows: Vec<usize> = m
                    .members
                    .iter()
                    .filter_map(|t| self.all.iter().position(|r| r.task.id == t.id))
                    .collect();
                rows.sort_unstable();
                rows
            }
            None => {
                let attached: std::collections::HashSet<i64> = self
                    .milestones
                    .iter()
                    .flat_map(|m| m.members.iter().map(|t| t.id))
                    .collect();
                self.all
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| !r.task.milestone && !attached.contains(&r.task.id))
                    .map(|(i, _)| i)
                    .collect()
            }
        }
    }

    /// A fold's `open` and `done` counts. A milestone's are its members' as
    /// the tab and `voro milestones` count them.
    pub fn group_counts(&self, group: Option<i64>) -> (usize, usize) {
        if let Some(m) = group.and_then(|id| self.milestone(id)) {
            return (m.open(), m.done());
        }
        let members = self.group_members(group);
        let count = |f: fn(TaskState) -> bool| {
            members
                .iter()
                .filter(|&&i| f(self.all[i].task.state))
                .count()
        };
        (count(|s| !s.is_terminal()), count(|s| s == TaskState::Done))
    }

    fn build_browser_rows(&self) -> Vec<BrowserRow> {
        if !self.browse_by_milestone {
            return (0..self.all.len()).map(BrowserRow::Task).collect();
        }
        let groups = self
            .milestones
            .iter()
            .map(|m| Some(m.milestone.id))
            .chain([None]);
        let mut rows = Vec::new();
        for group in groups {
            rows.push(BrowserRow::Group(group));
            if self.open_groups.contains(&group) {
                rows.extend(self.group_members(group).into_iter().map(BrowserRow::Task));
            }
        }
        rows
    }

    /// Rebuild the browser rows and keep the selection on the row it was on:
    /// the same task, or the same fold.
    fn rebuild_browser(&mut self) {
        let current = self.browser_rows.get(self.tasks_sel).map(|row| match row {
            BrowserRow::Task(i) => BrowserRow::Task(*i),
            BrowserRow::Group(g) => BrowserRow::Group(*g),
        });
        self.browser_rows = self.build_browser_rows();
        self.tasks_sel = current
            .and_then(|row| self.browser_rows.iter().position(|r| *r == row))
            .unwrap_or(0)
            .min(self.browser_rows.len().saturating_sub(1));
    }

    /// `M` on the browser: group by milestone, folds closed, or flatten again.
    pub(super) fn toggle_browse_by_milestone(&mut self) {
        self.browse_by_milestone = !self.browse_by_milestone;
        self.open_groups.clear();
        self.rebuild_browser();
    }

    pub(super) fn toggle_group(&mut self, group: Option<i64>) {
        if !self.open_groups.remove(&group) {
            self.open_groups.insert(group);
        }
        self.rebuild_browser();
    }

    /// ⏎ on the tab: the browser, grouped by milestone, with the selected
    /// milestone's fold the one open and under the cursor.
    fn open_milestone_browser(&mut self) {
        let Some(id) = self.selected_task_id() else {
            return;
        };
        self.browse_by_milestone = true;
        self.open_groups = [Some(id)].into();
        self.browser_rows = self.build_browser_rows();
        self.tasks_sel = self
            .browser_rows
            .iter()
            .position(|r| *r == BrowserRow::Group(Some(id)))
            .unwrap_or(0);
        self.screen = super::Screen::Tasks;
    }

    /// The Milestones tab's keys (DESIGN.md §9). Movement, `?` and the screen
    /// keys are `key_normal`'s. The create keys are the other screens' three,
    /// filing a milestone; ⏎ on a proposed milestone opens the triage menu, as
    /// it does on a proposal in the queue.
    pub(super) fn key_milestones(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('r') if ctrl => {
                let result = self.refresh();
                self.report(result);
            }
            KeyCode::Enter => {
                let proposed = self
                    .milestones
                    .get(self.milestones_sel)
                    .is_some_and(|m| m.milestone.state == TaskState::Proposed);
                if proposed {
                    self.open_milestone_menu();
                } else {
                    self.open_milestone_browser();
                }
            }
            KeyCode::Char('n') if ctrl => self.new_task(CreateFlow::Editor(Filing::Milestone)),
            KeyCode::Char('n') => self.new_task(CreateFlow::Quick(Filing::Milestone)),
            KeyCode::Char('N') => self.new_task(CreateFlow::Plan(Filing::Milestone)),
            KeyCode::Char('e') => {
                if let Some(task_id) = self.selected_task_id() {
                    self.pending_editor = Some(super::EditorRequest::Edit { task_id });
                }
            }
            KeyCode::Char('s') => self.open_milestone_menu(),
            _ => {}
        }
    }

    /// The selected milestone's transition menu: triage on a proposal, done or
    /// abandon once triaged.
    fn open_milestone_menu(&mut self) {
        let Some(m) = self.milestones.get(self.milestones_sel) else {
            return;
        };
        let actions = transition_actions(&m.milestone);
        if actions.is_empty() {
            self.status = Some(format!(
                "milestone is {} — nowhere to go",
                m.milestone.state
            ));
        } else {
            self.mode = Mode::Transition {
                task_id: m.milestone.id,
                actions,
                sel: 0,
            };
        }
    }

    /// The transition menu's done on a ready milestone.
    pub(super) fn close_milestone(&mut self, id: i64) {
        let result = self.store.close_milestone(id);
        if self.report(result).is_some() {
            let result = self.refresh();
            self.report(result);
        }
    }

    /// Whether a task blocks a milestone directly — what the picker ticks.
    /// Read from the per-refresh dependency map.
    pub fn blocks_directly(&self, task_id: i64, milestone_id: i64) -> bool {
        self.dependents.get(&task_id).is_some_and(|deps| {
            deps.iter()
                .any(|d| d.kind == DepKind::Blocks && d.id == milestone_id)
        })
    }

    /// Open the milestone picker on a task (DESIGN.md §9): every open
    /// milestone but the task itself, the task's own project's first. Returns
    /// whether it opened, as the document picker's opener does.
    pub(super) fn open_milestone_picker(&mut self, task_id: i64, back: Option<u16>) -> bool {
        let Some(project_id) = self
            .all
            .iter()
            .find(|r| r.task.id == task_id)
            .map(|r| r.task.project_id)
        else {
            return false;
        };
        let mut milestones: Vec<Task> = self
            .milestones
            .iter()
            .map(|m| &m.milestone)
            .filter(|m| m.id != task_id && !m.state.is_terminal())
            .cloned()
            .collect();
        if milestones.is_empty() {
            self.status = Some("no open milestones — press alt-5, then n to add one".into());
            return false;
        }
        milestones.sort_by_key(|m| (m.project_id != project_id, m.id));
        self.mode = Mode::MilestonePicker {
            task_id,
            milestones,
            sel: 0,
            back,
        };
        true
    }

    /// Drive the milestone picker: ⏎ adds or removes the task's `blocks` edge
    /// to the highlighted milestone, and the picker stays open.
    pub(super) fn key_milestone_picker(
        &mut self,
        key: KeyEvent,
        task_id: i64,
        milestones: Vec<Task>,
        mut sel: usize,
        back: Option<u16>,
    ) {
        match key.code {
            KeyCode::Esc => {
                if let Some(scroll) = back {
                    self.mode = Mode::Detail { task_id, scroll };
                }
                return;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                sel = (sel + 1).min(milestones.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => sel = sel.saturating_sub(1),
            KeyCode::Enter => self.toggle_milestone(task_id, &milestones[sel]),
            _ => {}
        }
        self.mode = Mode::MilestonePicker {
            task_id,
            milestones,
            sel,
            back,
        };
    }

    fn toggle_milestone(&mut self, task_id: i64, milestone: &Task) {
        let attached = self.blocks_directly(task_id, milestone.id);
        let result = if attached {
            self.store
                .remove_dep(milestone.id, task_id, DepKind::Blocks)
        } else {
            self.store.add_dep(milestone.id, task_id, DepKind::Blocks)
        }
        .and_then(|_| self.refresh());
        if self.report(result).is_some() {
            let verb = if attached { "detached" } else { "attached" };
            self.status = Some(format!(
                "{verb} task {task_id} {} milestone #{} {}",
                if attached { "from" } else { "to" },
                milestone.id,
                milestone.title
            ));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::Screen;
    use voro_core::{Action, NewTask, Priority, Store};

    fn key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::from(code));
    }

    fn alt_key(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::ALT));
    }

    pub(crate) fn app_from(store: Store) -> App {
        let ctx = crate::dispatch::DispatchCtx::without_config(std::path::Path::new(
            "/nonexistent/voro.db",
        ));
        App::new(store, ctx).unwrap()
    }

    fn seeded() -> App {
        let mut store = Store::open_in_memory().unwrap();
        voro_core::seed::seed(&mut store).unwrap();
        app_from(store)
    }

    pub(crate) fn new_task(
        store: &mut Store,
        project_id: i64,
        title: &str,
        state: TaskState,
    ) -> i64 {
        store
            .create_task(NewTask {
                project_id,
                repo_id: None,
                title: title.into(),
                body: String::new(),
                priority: Priority::P2,
                state,
                agent: None,
                human: false,
                deep: false,
                milestone: false,
            })
            .unwrap()
            .id
    }

    /// One project, one milestone, and one ready task blocking it.
    fn one_milestone() -> (App, i64, i64) {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Carpet crossing", "", Priority::P2, TaskState::Parked)
            .unwrap()
            .id;
        let t = new_task(&mut store, p, "tune traction", TaskState::Ready);
        store.add_dep(m, t, DepKind::Blocks).unwrap();
        (app_from(store), m, t)
    }

    fn select_cockpit_task(app: &mut App, id: i64) {
        app.screen = Screen::Cockpit;
        app.cockpit_sel = (0..app.cockpit_rows.len())
            .find(|&i| {
                app.cockpit_sel = i;
                app.selected_task_id() == Some(id)
            })
            .expect("the task is on the cockpit");
    }

    #[test]
    fn the_tab_lists_the_seeded_milestones_with_the_cli_counts() {
        let app = seeded();
        let listed = app.store.milestones(true).unwrap();
        assert_eq!(app.milestones.len(), 2);
        for m in &listed {
            let shown = app.milestone(m.milestone.id).unwrap();
            assert_eq!((shown.open(), shown.done()), (m.open(), m.done()));
        }
    }

    #[test]
    fn the_tab_orders_ready_then_parked_then_closed() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let gone = store
            .create_milestone(p, "gone", "", Priority::P2, TaskState::Parked)
            .unwrap();
        store.apply(gone.id, Action::Abandon).unwrap();
        let parked = store
            .create_milestone(p, "parked", "", Priority::P2, TaskState::Parked)
            .unwrap();
        let ready = store
            .create_milestone(p, "ready", "", Priority::P2, TaskState::Parked)
            .unwrap();
        store.apply(ready.id, Action::Unpark).unwrap();
        let app = app_from(store);
        let order: Vec<i64> = app.milestones.iter().map(|m| m.milestone.id).collect();
        assert_eq!(order, vec![ready.id, parked.id, gone.id]);
    }

    #[test]
    fn the_create_keys_file_a_milestone_through_the_ordinary_flows() {
        let mut app = seeded();
        alt_key(&mut app, KeyCode::Char('5'));
        assert_eq!(app.screen, Screen::Milestones);
        for (code, modifiers, flow) in [
            (
                KeyCode::Char('n'),
                KeyModifiers::NONE,
                CreateFlow::Quick(Filing::Milestone),
            ),
            (
                KeyCode::Char('N'),
                KeyModifiers::SHIFT,
                CreateFlow::Plan(Filing::Milestone),
            ),
            (
                KeyCode::Char('n'),
                KeyModifiers::CONTROL,
                CreateFlow::Editor(Filing::Milestone),
            ),
        ] {
            app.on_key(KeyEvent::new(code, modifiers));
            assert!(
                matches!(app.mode, Mode::PickProject { flow: f, .. } if f == flow),
                "several projects, so the picker asks which: {flow:?}"
            );
            key(&mut app, KeyCode::Esc);
        }
        let before = app.store.tasks().unwrap().len();
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        key(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.pending_editor,
            Some(crate::app::EditorRequest::Create {
                filing: Filing::Milestone,
                ..
            })
        ));
        assert_eq!(app.store.tasks().unwrap().len(), before, "no row yet");
    }

    #[test]
    fn enter_on_a_proposed_milestone_opens_triage_and_ready_queues_it() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Dock", "", Priority::P2, TaskState::Proposed)
            .unwrap()
            .id;
        let mut app = app_from(store);
        alt_key(&mut app, KeyCode::Char('5'));
        assert_eq!(app.selected_task_id(), Some(m));
        assert_eq!(app.enter_hint(), Some("⏎ triage"));
        key(&mut app, KeyCode::Enter);
        let Mode::Transition { actions, .. } = &app.mode else {
            panic!("⏎ opens the triage menu");
        };
        assert_eq!(
            actions,
            &Store::legal_actions(TaskState::Proposed, true),
            "the verdicts a proposal gets"
        );
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(m).unwrap().state, TaskState::Ready);
        assert!(app.queue_task_ids().contains(&m));
    }

    #[test]
    fn refine_is_refused_on_a_milestone() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Dock", "", Priority::P2, TaskState::Proposed)
            .unwrap()
            .id;
        let mut app = app_from(store);
        alt_key(&mut app, KeyCode::Char('2'));
        app.tasks_sel = app
            .browser_rows
            .iter()
            .position(|r| matches!(r, BrowserRow::Task(i) if app.all[*i].task.id == m))
            .unwrap();
        key(&mut app, KeyCode::Char('r'));
        assert!(matches!(app.mode, Mode::Normal));
        let status = app.status.as_deref().unwrap_or("");
        assert!(status.contains("acceptance statement"), "{status}");
    }

    #[test]
    fn a_ready_milestone_joins_the_queue_as_a_do_row_and_the_menu_closes_it() {
        let (mut app, m, t) = one_milestone();
        assert!(!app.queue_task_ids().contains(&m));
        for action in [Action::Start, Action::Complete(None), Action::Accept] {
            app.store.apply(t, action).unwrap();
        }
        app.refresh().unwrap();
        assert!(app.queue_task_ids().contains(&m));
        let row = app
            .queue
            .rows
            .iter()
            .find_map(|row| match row {
                voro_core::QueueRow::Action(row) if row.candidate.task.id == m => Some(row),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            row.candidate.task.next_action(),
            Some(voro_core::NextAction::Do)
        );

        select_cockpit_task(&mut app, m);
        key(&mut app, KeyCode::Enter);
        let Mode::Transition { actions, .. } = &app.mode else {
            panic!("⏎ opens the transition menu");
        };
        assert_eq!(
            actions,
            &vec![Action::Complete(None), Action::Park, Action::Abandon]
        );
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(m).unwrap().state, TaskState::Done);
        assert!(!app.queue_task_ids().contains(&m));
    }

    #[test]
    fn the_tab_state_key_offers_a_parked_milestone_unpark_and_abandon() {
        let (mut app, m, _) = one_milestone();
        alt_key(&mut app, KeyCode::Char('5'));
        key(&mut app, KeyCode::Char('s'));
        let Mode::Transition { actions, .. } = &app.mode else {
            panic!("s opens the transition menu");
        };
        assert_eq!(actions, &vec![Action::Unpark, Action::Abandon]);
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(m).unwrap().state, TaskState::Rejected);
    }

    /// A milestone parked with nothing blocking it is not stuck: the operator
    /// unparks it from the tab, and parks it again from there.
    #[test]
    fn an_unblocked_parked_milestone_unparks_and_parks_from_the_tab() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let m = store
            .create_milestone(p, "Dock", "", Priority::P2, TaskState::Parked)
            .unwrap()
            .id;
        let mut app = app_from(store);
        alt_key(&mut app, KeyCode::Char('5'));
        key(&mut app, KeyCode::Char('s'));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(m).unwrap().state, TaskState::Ready);
        key(&mut app, KeyCode::Char('s'));
        key(&mut app, KeyCode::Char('j'));
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.store.task(m).unwrap().state, TaskState::Parked);
    }

    #[test]
    fn grouping_the_browser_starts_every_fold_closed_with_the_tab_counts() {
        let mut app = seeded();
        alt_key(&mut app, KeyCode::Char('2'));
        key(&mut app, KeyCode::Char('M'));
        assert!(app.browse_by_milestone);
        assert_eq!(app.browser_rows.len(), app.milestones.len() + 1);
        assert!(
            app.browser_rows
                .iter()
                .all(|r| matches!(r, BrowserRow::Group(_)))
        );
        assert_eq!(app.browser_rows.last(), Some(&BrowserRow::Group(None)));
        for m in &app.milestones {
            assert_eq!(app.group_counts(Some(m.milestone.id)), (m.open(), m.done()));
        }

        // ⏎ on a fold opens it onto its members; `M` again flattens.
        key(&mut app, KeyCode::Enter);
        let first = app.milestones[0].members.len();
        assert_eq!(app.browser_rows.len(), app.milestones.len() + 1 + first);
        key(&mut app, KeyCode::Char('M'));
        assert_eq!(app.browser_rows.len(), app.all.len());
    }

    #[test]
    fn a_task_with_two_milestones_sits_in_both_folds_and_not_in_unattached() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let a = store
            .create_milestone(p, "a", "", Priority::P2, TaskState::Parked)
            .unwrap()
            .id;
        let b = store
            .create_milestone(p, "b", "", Priority::P2, TaskState::Parked)
            .unwrap()
            .id;
        let shared = new_task(&mut store, p, "shared", TaskState::Ready);
        let loose = new_task(&mut store, p, "loose", TaskState::Ready);
        store.block_tasks(shared, &[a, b]).unwrap();
        let app = app_from(store);
        let ids = |group| -> Vec<i64> {
            app.group_members(group)
                .into_iter()
                .map(|i| app.all[i].task.id)
                .collect()
        };
        assert_eq!(ids(Some(a)), vec![shared]);
        assert_eq!(ids(Some(b)), vec![shared]);
        assert_eq!(ids(None), vec![loose]);
    }

    #[test]
    fn enter_on_the_tab_opens_the_browser_on_that_milestones_fold() {
        let mut app = seeded();
        alt_key(&mut app, KeyCode::Char('5'));
        key(&mut app, KeyCode::Char('j'));
        let id = app.selected_task_id().unwrap();
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Tasks);
        assert!(app.browse_by_milestone);
        assert_eq!(
            app.browser_rows.get(app.tasks_sel),
            Some(&BrowserRow::Group(Some(id)))
        );
        assert_eq!(app.enter_hint(), Some("⏎ collapse"));
        let members = app.milestone(id).unwrap().members.len();
        assert_eq!(app.browser_rows.len(), app.milestones.len() + 1 + members);
    }

    #[test]
    fn m_attaches_and_detaches_a_task_in_place() {
        let (mut app, m, _) = one_milestone();
        let p = app.projects[0].id;
        let loose = new_task(&mut app.store, p, "loose", TaskState::Ready);
        app.refresh().unwrap();
        assert!(app.milestones_of(loose).is_empty());

        select_cockpit_task(&mut app, loose);
        key(&mut app, KeyCode::Char('m'));
        assert!(matches!(app.mode, Mode::MilestonePicker { .. }));
        key(&mut app, KeyCode::Enter);
        assert!(app.blocks_directly(loose, m));
        let ids: Vec<i64> = app.milestones_of(loose).iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![m]);
        assert!(
            matches!(app.mode, Mode::MilestonePicker { .. }),
            "the picker stays open"
        );
        key(&mut app, KeyCode::Enter);
        assert!(!app.blocks_directly(loose, m));
        assert!(app.milestones_of(loose).is_empty());
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn the_picker_from_the_detail_popup_returns_to_it() {
        let (mut app, _, t) = one_milestone();
        alt_key(&mut app, KeyCode::Char('2'));
        app.tasks_sel = app
            .browser_rows
            .iter()
            .position(|r| matches!(r, BrowserRow::Task(i) if app.all[*i].task.id == t))
            .unwrap();
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Char('m'));
        assert!(matches!(
            app.mode,
            Mode::MilestonePicker { back: Some(0), .. }
        ));
        key(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Detail { task_id, .. } if task_id == t));
    }

    #[test]
    fn m_with_no_open_milestone_says_where_to_make_one() {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap().id;
        let t = new_task(&mut store, p, "alone", TaskState::Ready);
        let mut app = app_from(store);
        select_cockpit_task(&mut app, t);
        key(&mut app, KeyCode::Char('m'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_deref().unwrap().contains("alt-5"));
    }
}
