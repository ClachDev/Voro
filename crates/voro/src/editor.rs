//! Task create/edit via `$EDITOR`: a plain `key: value` frontmatter above a
//! `---` line, markdown body below. Hand-rolled to keep the dependency tree
//! boring; parse errors are fed back into the file as comments and the
//! editor reopens rather than losing input.

use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use voro_core::{Priority, Task, TaskState};

#[derive(Debug, Clone)]
pub struct TaskForm {
    pub title: String,
    pub priority: Priority,
    pub state: Option<TaskState>,
    pub agent: Option<String>,
    pub blocks: Vec<i64>,
    pub body: String,
}

pub fn template_new() -> String {
    "# New task. The body below --- is the dispatchable prompt.\n\
     # Save an empty file to cancel. state: proposed | parked | ready\n\
     title: \n\
     priority: 2\n\
     state: ready\n\
     agent: \n\
     blocks: \n\
     ---\n"
        .to_string()
}

pub fn template_edit(task: &Task, blocks: &[i64]) -> String {
    let blocks = blocks
        .iter()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "# Editing task {}. State changes go through the transition menu, not here.\n\
         # Save an empty file to cancel.\n\
         title: {}\n\
         priority: {}\n\
         agent: {}\n\
         blocks: {}\n\
         ---\n\
         {}",
        task.id,
        task.title,
        task.priority.as_int(),
        task.agent.as_deref().unwrap_or(""),
        blocks,
        task.body
    )
}

/// Prepend an error to `text` as comments, stripping any previous error
/// block, so the reopened file explains itself.
pub fn with_error(text: &str, error: &str) -> String {
    let rest: String = text
        .lines()
        .skip_while(|l| l.starts_with("# ERROR") || l.starts_with("#   "))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut out = String::new();
    for (i, line) in error.lines().enumerate() {
        let prefix = if i == 0 { "# ERROR: " } else { "#   " };
        out.push_str(&format!("{prefix}{line}\n"));
    }
    out + &rest
}

pub fn parse(text: &str, allow_state: bool) -> Result<TaskForm, String> {
    let mut lines = text.lines();
    let mut form = TaskForm {
        title: String::new(),
        priority: Priority::P2,
        state: None,
        agent: None,
        blocks: Vec::new(),
        body: String::new(),
    };
    let mut saw_separator = false;

    for line in lines.by_ref() {
        if line.trim() == "---" {
            saw_separator = true;
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("not a 'key: value' line: {trimmed}"));
        };
        let value = value.trim();
        match key.trim() {
            "title" => form.title = value.to_string(),
            "priority" => {
                let n: i64 = value
                    .trim_start_matches(['p', 'P'])
                    .parse()
                    .map_err(|_| format!("priority must be 0-3, got '{value}'"))?;
                form.priority = Priority::from_int(n).map_err(|e| e.to_string())?;
            }
            "state" => {
                if !allow_state {
                    return Err("state is changed via the transition menu, not the editor".into());
                }
                let state = TaskState::parse(value).map_err(|e| e.to_string())?;
                if !matches!(
                    state,
                    TaskState::Proposed | TaskState::Parked | TaskState::Ready
                ) {
                    return Err(format!("a task cannot be created in state '{state}'"));
                }
                form.state = Some(state);
            }
            "agent" => form.agent = (!value.is_empty()).then(|| value.to_string()),
            "blocks" => {
                for id in value.split([',', ' ']).filter(|s| !s.trim().is_empty()) {
                    form.blocks.push(
                        id.trim()
                            .parse()
                            .map_err(|_| format!("blocks must be task ids, got '{id}'"))?,
                    );
                }
            }
            other => return Err(format!("unknown field '{other}'")),
        }
    }

    if !saw_separator {
        return Err("missing '---' separator between fields and body".into());
    }
    if form.title.is_empty() {
        return Err("title is required".into());
    }
    form.body = lines.collect::<Vec<_>>().join("\n").trim().to_string();
    Ok(form)
}

/// Run `$EDITOR` (fallback `$VISUAL`, then `vi`) on a temp file seeded with
/// `initial`; returns the saved contents. The caller has already left the
/// alternate screen.
pub fn run_editor(initial: &str) -> Result<String, String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path: PathBuf =
        std::env::temp_dir().join(format!("voro-task-{}-{stamp}.md", std::process::id()));
    std::fs::write(&path, initial).map_err(|e| format!("cannot write {path:?}: {e}"))?;

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$VORO_EDIT_FILE\""))
        .env("VORO_EDIT_FILE", &path)
        .status()
        .map_err(|e| format!("cannot launch editor '{editor}': {e}"))?;

    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read back: {e}"));
    let _ = std::fs::remove_file(&path);
    if !status.success() {
        return Err(format!("editor exited with {status}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "title: Fix parser\npriority: 1\nstate: parked\nagent: codex\n\
                         blocks: 3, 7\n---\nThe parser crashes on empty input.\n";

    #[test]
    fn parses_all_fields() {
        let form = parse(VALID, true).unwrap();
        assert_eq!(form.title, "Fix parser");
        assert_eq!(form.priority, Priority::P1);
        assert_eq!(form.state, Some(TaskState::Parked));
        assert_eq!(form.agent.as_deref(), Some("codex"));
        assert_eq!(form.blocks, vec![3, 7]);
        assert_eq!(form.body, "The parser crashes on empty input.");
    }

    #[test]
    fn state_rejected_when_editing() {
        assert!(parse(VALID, false).is_err());
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let form = parse("# a comment\n\ntitle: T\n---\n", true).unwrap();
        assert_eq!(form.title, "T");
        assert!(form.state.is_none());
        assert!(form.agent.is_none());
        assert!(form.blocks.is_empty());
    }

    #[test]
    fn errors_are_specific() {
        assert!(parse("title: T\n", true).unwrap_err().contains("---"));
        assert!(parse("---\n", true).unwrap_err().contains("title"));
        assert!(
            parse("title: T\nnope\n---\n", true)
                .unwrap_err()
                .contains("key: value")
        );
        assert!(
            parse("title: T\npriority: 9\n---\n", true)
                .unwrap_err()
                .contains("0-3")
        );
        assert!(
            parse("title: T\nstate: done\n---\n", true)
                .unwrap_err()
                .contains("done")
        );
        assert!(
            parse("title: T\nblocks: x\n---\n", true)
                .unwrap_err()
                .contains("task ids")
        );
        assert!(
            parse("title: T\nwat: 1\n---\n", true)
                .unwrap_err()
                .contains("unknown")
        );
    }

    #[test]
    fn error_prefix_replaces_previous_one() {
        let with_one = with_error("title: T\n---\n", "first problem");
        assert!(with_one.starts_with("# ERROR: first problem\n"));
        let with_two = with_error(&with_one, "second\nproblem");
        assert_eq!(with_two.matches("# ERROR").count(), 1);
        assert!(with_two.contains("second"));
        assert!(with_two.contains("title: T"));
    }

    #[test]
    fn round_trip_edit_template() {
        let task = Task {
            id: 4,
            project_id: 1,
            title: "Fix parser".into(),
            body: "Body text.".into(),
            priority: Priority::P1,
            state: TaskState::Ready,
            agent: None,
            question: None,
            pr_url: None,
            state_since: String::new(),
            created_at: String::new(),
            closed_at: None,
        };
        let form = parse(&template_edit(&task, &[9]), false).unwrap();
        assert_eq!(form.title, task.title);
        assert_eq!(form.priority, task.priority);
        assert_eq!(form.blocks, vec![9]);
        assert_eq!(form.body, task.body);
    }
}
