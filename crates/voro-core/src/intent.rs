//! A project's intent (DESIGN.md §3/§5): ordered lines, each a label and one
//! sentence, that the operator owns. Position is precedence — where two lines
//! pull apart the earlier wins — so reordering is an edit in its own right. A
//! line an agent proposes is stored unaccepted and reaches no agent until the
//! operator accepts it.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, params};

use crate::error::{Error, Result};
use crate::store::{Store, log_global_event};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentLine {
    pub id: i64,
    pub project_id: i64,
    /// How a prompt, a proposal or a rejection refers to the line; unique
    /// within the project, case folded.
    pub label: String,
    pub sentence: String,
    /// Sort key only: lower comes first. Gaps are harmless.
    pub position: i64,
    /// `false` for a line an agent proposed and the operator has not yet
    /// accepted; such a line is shown to no agent.
    pub accepted: bool,
}

impl IntentLine {
    /// The line as agents read it: `label: sentence`.
    pub fn render(&self) -> String {
        format!("{}: {}", self.label, self.sentence)
    }
}

const INTENT_COLUMNS: &str = "id, project_id, label, sentence, position, accepted";

fn intent_from_row(row: &Row) -> rusqlite::Result<IntentLine> {
    Ok(IntentLine {
        id: row.get(0)?,
        project_id: row.get(1)?,
        label: row.get(2)?,
        sentence: row.get(3)?,
        position: row.get(4)?,
        accepted: row.get(5)?,
    })
}

/// Trim both parts and refuse what would not read back as one `label:
/// sentence` line: an empty part, a line break, or a colon in the label.
fn clean(label: &str, sentence: &str) -> Result<(String, String)> {
    let (label, sentence) = (label.trim(), sentence.trim());
    if label.is_empty() || sentence.is_empty() {
        return Err(Error::Invalid(
            "an intent line needs both a label and a sentence".into(),
        ));
    }
    if label.contains(['\n', '\r']) || sentence.contains(['\n', '\r']) {
        return Err(Error::Invalid(
            "an intent line is one line — drop the line break".into(),
        ));
    }
    if label.contains(':') {
        return Err(Error::Invalid(format!(
            "label '{label}' contains a colon, which `label: sentence` cannot read back"
        )));
    }
    Ok((label.to_string(), sentence.to_string()))
}

impl Store {
    /// Every intent line of a project, accepted and pending, in precedence
    /// order.
    pub fn intent_lines(&self, project_id: i64) -> Result<Vec<IntentLine>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {INTENT_COLUMNS} FROM intent_lines WHERE project_id = ?1
             ORDER BY position, id"
        ))?;
        let rows = stmt.query_map([project_id], intent_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The lines agents are shown: accepted ones only, in precedence order.
    pub fn accepted_intent(&self, project_id: i64) -> Result<Vec<IntentLine>> {
        Ok(self
            .intent_lines(project_id)?
            .into_iter()
            .filter(|l| l.accepted)
            .collect())
    }

    pub fn intent_line(&self, id: i64) -> Result<IntentLine> {
        self.conn
            .query_row(
                &format!("SELECT {INTENT_COLUMNS} FROM intent_lines WHERE id = ?1"),
                [id],
                intent_from_row,
            )
            .optional()?
            .ok_or(Error::IntentLineNotFound(id))
    }

    /// Every project's lines in precedence order, keyed by project; projects
    /// with none are absent.
    pub fn all_intent_lines(&self) -> Result<HashMap<i64, Vec<IntentLine>>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {INTENT_COLUMNS} FROM intent_lines ORDER BY project_id, position, id"
        ))?;
        let rows = stmt.query_map([], intent_from_row)?;
        let mut by_project: HashMap<i64, Vec<IntentLine>> = HashMap::new();
        for line in rows {
            let line = line?;
            by_project.entry(line.project_id).or_default().push(line);
        }
        Ok(by_project)
    }

    /// The operator's add: an accepted line, last in precedence.
    pub fn add_intent_line(
        &mut self,
        project_id: i64,
        label: &str,
        sentence: &str,
    ) -> Result<IntentLine> {
        self.insert_intent_line(project_id, label, sentence, true)
    }

    /// An agent's proposal: a pending line, last in precedence. Nothing an
    /// agent calls creates an accepted line.
    pub fn propose_intent_line(
        &mut self,
        project_id: i64,
        label: &str,
        sentence: &str,
    ) -> Result<IntentLine> {
        self.insert_intent_line(project_id, label, sentence, false)
    }

    fn insert_intent_line(
        &mut self,
        project_id: i64,
        label: &str,
        sentence: &str,
        accepted: bool,
    ) -> Result<IntentLine> {
        let project = self.project(project_id)?;
        let (label, sentence) = clean(label, sentence)?;
        self.refuse_taken_label(project_id, &label, None)?;
        let tx = self.conn.transaction()?;
        let position: i64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) + 1 FROM intent_lines WHERE project_id = ?1",
            [project_id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO intent_lines (project_id, label, sentence, position, accepted, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            params![project_id, label, sentence, position, accepted],
        )?;
        let id = tx.last_insert_rowid();
        let kind = if accepted {
            "intent-added"
        } else {
            "intent-proposed"
        };
        log_global_event(
            &tx,
            kind,
            Some(&format!("{}: {label}: {sentence}", project.name)),
        )?;
        tx.commit()?;
        self.intent_line(id)
    }

    /// Replace a line's label and sentence, keeping its place and whether it
    /// is accepted. The event keeps the old text, since this row will not.
    pub fn edit_intent_line(&mut self, id: i64, label: &str, sentence: &str) -> Result<IntentLine> {
        let line = self.intent_line(id)?;
        let project = self.project(line.project_id)?;
        let (label, sentence) = clean(label, sentence)?;
        self.refuse_taken_label(line.project_id, &label, Some(id))?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE intent_lines SET label = ?1, sentence = ?2 WHERE id = ?3",
            params![label, sentence, id],
        )?;
        log_global_event(
            &tx,
            "intent-edited",
            Some(&format!(
                "{}: {} -> {label}: {sentence}",
                project.name,
                line.render()
            )),
        )?;
        tx.commit()?;
        self.intent_line(id)
    }

    /// Remove a line, accepted or pending; discarding a proposal is this.
    pub fn remove_intent_line(&mut self, id: i64) -> Result<IntentLine> {
        let line = self.intent_line(id)?;
        let project = self.project(line.project_id)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM intent_lines WHERE id = ?1", [id])?;
        let kind = if line.accepted {
            "intent-removed"
        } else {
            "intent-discarded"
        };
        log_global_event(
            &tx,
            kind,
            Some(&format!("{}: {}", project.name, line.render())),
        )?;
        tx.commit()?;
        Ok(line)
    }

    /// Accept a pending line where it stands. Accepting an accepted line is
    /// refused, so a repeated key is heard rather than absorbed.
    pub fn accept_intent_line(&mut self, id: i64) -> Result<IntentLine> {
        let line = self.intent_line(id)?;
        if line.accepted {
            return Err(Error::Invalid(format!(
                "intent line '{}' is already accepted",
                line.label
            )));
        }
        let project = self.project(line.project_id)?;
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE intent_lines SET accepted = 1 WHERE id = ?1", [id])?;
        log_global_event(
            &tx,
            "intent-accepted",
            Some(&format!("{}: {}", project.name, line.render())),
        )?;
        tx.commit()?;
        self.intent_line(id)
    }

    /// Swap a line with its neighbour above (`up`) or below, across accepted
    /// and pending lines alike. Returns `false`, changing nothing, when the
    /// line is already at that end.
    pub fn move_intent_line(&mut self, id: i64, up: bool) -> Result<bool> {
        let line = self.intent_line(id)?;
        let lines = self.intent_lines(line.project_id)?;
        let at = lines
            .iter()
            .position(|l| l.id == id)
            .expect("a line is among its own project's lines");
        let to = match (up, at) {
            (true, 0) => return Ok(false),
            (true, _) => at - 1,
            (false, _) if at + 1 == lines.len() => return Ok(false),
            (false, _) => at + 1,
        };
        let other = &lines[to];
        let mut order: Vec<i64> = lines.iter().map(|l| l.id).collect();
        order.swap(at, to);
        let project = self.project(line.project_id)?;
        let tx = self.conn.transaction()?;
        // Renumbering the whole list keeps positions distinct even where
        // earlier data left two equal.
        for (i, line_id) in order.iter().enumerate() {
            tx.execute(
                "UPDATE intent_lines SET position = ?1 WHERE id = ?2",
                params![i as i64 + 1, line_id],
            )?;
        }
        log_global_event(
            &tx,
            "intent-moved",
            Some(&format!(
                "{}: {} {} {}",
                project.name,
                line.label,
                if up { "above" } else { "below" },
                other.label
            )),
        )?;
        tx.commit()?;
        Ok(true)
    }

    fn refuse_taken_label(&self, project_id: i64, label: &str, except: Option<i64>) -> Result<()> {
        let folded = label.to_lowercase();
        if let Some(taken) = self
            .intent_lines(project_id)?
            .into_iter()
            .find(|l| Some(l.id) != except && l.label.to_lowercase() == folded)
        {
            let state = if taken.accepted { "" } else { "pending " };
            return Err(Error::Invalid(format!(
                "this project already has a {state}intent line labelled '{}'",
                taken.label
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_project() -> (Store, i64) {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("voro", "/tmp/voro").unwrap();
        (store, p.id)
    }

    fn labels(store: &Store, project: i64) -> Vec<String> {
        store
            .intent_lines(project)
            .unwrap()
            .into_iter()
            .map(|l| l.label)
            .collect()
    }

    #[test]
    fn lines_keep_the_order_they_were_added_in() {
        let (mut store, p) = store_with_project();
        store.add_intent_line(p, "what", "one screen").unwrap();
        store.add_intent_line(p, "why", "attention").unwrap();
        store
            .add_intent_line(p, "truth", "no silent state")
            .unwrap();
        assert_eq!(labels(&store, p), ["what", "why", "truth"]);
        assert!(store.intent_lines(p).unwrap().iter().all(|l| l.accepted));
        assert_eq!(
            store.accepted_intent(p).unwrap()[0].render(),
            "what: one screen"
        );
    }

    #[test]
    fn a_proposed_line_is_hidden_from_agents_until_accepted() {
        let (mut store, p) = store_with_project();
        store.add_intent_line(p, "what", "one screen").unwrap();
        let proposed = store
            .propose_intent_line(p, "glance", "at a glance")
            .unwrap();
        assert!(!proposed.accepted);
        assert_eq!(store.accepted_intent(p).unwrap().len(), 1);
        assert_eq!(store.all_intent_lines().unwrap()[&p].len(), 2);

        store.accept_intent_line(proposed.id).unwrap();
        assert_eq!(store.accepted_intent(p).unwrap().len(), 2);
        assert!(
            store.all_intent_lines().unwrap()[&p]
                .iter()
                .all(|l| l.accepted)
        );
        assert!(store.accept_intent_line(proposed.id).is_err());
    }

    #[test]
    fn moving_swaps_neighbours_and_stops_at_the_ends() {
        let (mut store, p) = store_with_project();
        let a = store.add_intent_line(p, "a", "first").unwrap();
        store.add_intent_line(p, "b", "second").unwrap();
        let c = store.add_intent_line(p, "c", "third").unwrap();

        assert!(!store.move_intent_line(a.id, true).unwrap());
        assert!(!store.move_intent_line(c.id, false).unwrap());

        assert!(store.move_intent_line(c.id, true).unwrap());
        assert_eq!(labels(&store, p), ["a", "c", "b"]);
        assert!(store.move_intent_line(a.id, false).unwrap());
        assert_eq!(labels(&store, p), ["c", "a", "b"]);
    }

    #[test]
    fn a_reorder_reaches_the_accepted_list() {
        let (mut store, p) = store_with_project();
        store.add_intent_line(p, "why", "attention").unwrap();
        let control = store
            .add_intent_line(p, "control", "never decides")
            .unwrap();
        store.move_intent_line(control.id, true).unwrap();
        let order: Vec<_> = store
            .accepted_intent(p)
            .unwrap()
            .into_iter()
            .map(|l| l.label)
            .collect();
        assert_eq!(order, ["control", "why"]);
    }

    #[test]
    fn labels_are_unique_within_a_project_ignoring_case() {
        let (mut store, p) = store_with_project();
        store.add_intent_line(p, "Why", "attention").unwrap();
        let err = store.propose_intent_line(p, "why", "other").unwrap_err();
        assert!(err.to_string().contains("'Why'"), "{err}");

        let other = store.create_project("other", "/tmp/other").unwrap();
        store.add_intent_line(other.id, "why", "fine here").unwrap();
    }

    #[test]
    fn edit_keeps_place_and_acceptance_and_logs_the_old_text() {
        let (mut store, p) = store_with_project();
        store.add_intent_line(p, "what", "one screen").unwrap();
        let pending = store
            .propose_intent_line(p, "glance", "at a glance")
            .unwrap();
        let edited = store
            .edit_intent_line(pending.id, "glance", "read at a glance")
            .unwrap();
        assert_eq!(edited.position, pending.position);
        assert!(!edited.accepted);
        // Renaming a line to its own label in another case is not a clash.
        store.edit_intent_line(pending.id, "Glance", "x").unwrap();

        let detail: String = store
            .conn
            .query_row(
                "SELECT detail FROM events WHERE kind = 'intent-edited' ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            detail,
            "voro: glance: at a glance -> glance: read at a glance"
        );
    }

    #[test]
    fn malformed_lines_are_refused() {
        let (mut store, p) = store_with_project();
        assert!(store.add_intent_line(p, " ", "x").is_err());
        assert!(store.add_intent_line(p, "x", "").is_err());
        assert!(store.add_intent_line(p, "x", "two\nlines").is_err());
        assert!(store.add_intent_line(p, "a:b", "x").is_err());
        assert!(store.add_intent_line(9999, "x", "y").is_err());
        let line = store
            .add_intent_line(p, "  for whom ", " Michael ")
            .unwrap();
        assert_eq!(line.render(), "for whom: Michael");
    }

    #[test]
    fn removing_a_line_and_deleting_its_project() {
        let (mut store, p) = store_with_project();
        let a = store.add_intent_line(p, "a", "x").unwrap();
        store.propose_intent_line(p, "b", "y").unwrap();
        store.remove_intent_line(a.id).unwrap();
        assert_eq!(labels(&store, p), ["b"]);
        assert!(store.remove_intent_line(a.id).is_err());

        store.delete_project(p).unwrap();
        let left: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM intent_lines", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn lines_survive_reopening_the_store() {
        let dir = tempfile::Builder::new()
            .prefix("voro-intent-")
            .tempdir()
            .unwrap()
            .keep();
        let path = dir.join("voro.db");
        {
            let mut store = Store::open(&path).unwrap();
            let p = store.create_project("voro", "/tmp/voro").unwrap();
            store.add_intent_line(p.id, "what", "one screen").unwrap();
            store.add_intent_line(p.id, "why", "attention").unwrap();
        }
        let store = Store::open(&path).unwrap();
        let p = store.projects().unwrap()[0].id;
        assert_eq!(labels(&store, p), ["what", "why"]);
    }
}
