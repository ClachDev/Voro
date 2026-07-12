//! GitHub issue import (DESIGN.md §10, Milestone C): one-way capture of
//! issues into tasks. Voro's store stays the source of truth for priority
//! and state — this module only maps `gh issue list --json ...` output onto
//! [`NewTask`] and detects issues already captured, so import is idempotent.
//!
//! The `gh` shell-out itself is I/O and lives in the `voro` crate, same as
//! agent dispatch lives outside `voro-core`; everything here is pure enough
//! to test against canned JSON.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::{Priority, TaskState};
use crate::store::NewTask;

/// One issue from `gh issue list --json number,title,body,url`. Those four
/// fields are all gh documents for this command; anything else it emits is
/// ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubIssue {
    pub number: i64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    pub url: String,
}

impl GithubIssue {
    /// Parse the JSON array `gh issue list --json ...` prints on stdout.
    pub fn parse_list(json: &str) -> Result<Vec<GithubIssue>> {
        serde_json::from_str(json)
            .map_err(|e| Error::Invalid(format!("invalid `gh issue list` JSON: {e}")))
    }
}

/// The task body for an imported issue: the URL and issue number stamped at
/// the top (both for human reference and as the idempotency marker checked
/// by [`already_imported`]), followed by the issue body verbatim.
pub fn issue_task_body(issue: &GithubIssue) -> String {
    let mut body = format!("Imported from {} (issue #{})\n", issue.url, issue.number);
    if !issue.body.trim().is_empty() {
        body.push('\n');
        body.push_str(&issue.body);
    }
    body
}

/// An issue mapped to a task, always landing in `proposed`: imports are
/// untriaged like any other machine-generated task, and priority is not
/// guessed from labels in v1 — triage assigns it.
pub fn issue_new_task(project_id: i64, issue: &GithubIssue) -> NewTask {
    NewTask {
        project_id,
        title: issue.title.clone(),
        body: issue_task_body(issue),
        priority: Priority::P2,
        state: TaskState::Proposed,
        agent: None,
        human: false,
    }
}

/// Whether `body` (an existing task's body) already carries this issue's
/// URL — the idempotency check that lets import be run repeatedly without
/// creating duplicate tasks.
pub fn already_imported(body: &str, issue: &GithubIssue) -> bool {
    body.contains(&issue.url)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CANNED: &str = r#"[
        {
            "number": 42,
            "title": "Crash on empty input",
            "body": "Steps to reproduce:\n1. Run with no args",
            "url": "https://github.com/acme/widget/issues/42"
        },
        {
            "number": 7,
            "title": "No body issue",
            "body": "",
            "url": "https://github.com/acme/widget/issues/7"
        }
    ]"#;

    #[test]
    fn parses_the_documented_fields() {
        let issues = GithubIssue::parse_list(CANNED).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 42);
        assert_eq!(issues[0].title, "Crash on empty input");
        assert_eq!(issues[0].url, "https://github.com/acme/widget/issues/42");
        assert!(issues[0].body.contains("Steps to reproduce"));
    }

    #[test]
    fn body_field_defaults_when_absent() {
        let json = r#"[{"number": 1, "title": "T", "url": "https://x/1"}]"#;
        let issues = GithubIssue::parse_list(json).unwrap();
        assert_eq!(issues[0].body, "");
    }

    #[test]
    fn rejects_malformed_json() {
        let err = GithubIssue::parse_list("not json").unwrap_err();
        assert!(err.to_string().contains("invalid"), "{err}");
    }

    #[test]
    fn issue_body_stamps_url_and_number_above_the_issue_text() {
        let issues = GithubIssue::parse_list(CANNED).unwrap();
        let body = issue_task_body(&issues[0]);
        let mut lines = body.lines();
        assert_eq!(
            lines.next().unwrap(),
            "Imported from https://github.com/acme/widget/issues/42 (issue #42)"
        );
        assert!(body.contains("Steps to reproduce"));
    }

    #[test]
    fn empty_issue_body_still_stamps_the_header_only() {
        let issues = GithubIssue::parse_list(CANNED).unwrap();
        let body = issue_task_body(&issues[1]);
        assert_eq!(
            body,
            "Imported from https://github.com/acme/widget/issues/7 (issue #7)\n"
        );
    }

    #[test]
    fn new_task_lands_proposed_with_no_guessed_priority_or_agent() {
        let issues = GithubIssue::parse_list(CANNED).unwrap();
        let task = issue_new_task(9, &issues[0]);
        assert_eq!(task.project_id, 9);
        assert_eq!(task.title, "Crash on empty input");
        assert_eq!(task.state, TaskState::Proposed);
        assert_eq!(task.priority, Priority::P2);
        assert!(task.agent.is_none());
        assert!(task.body.contains(&issues[0].url));
    }

    #[test]
    fn already_imported_detects_the_stamped_url() {
        let issues = GithubIssue::parse_list(CANNED).unwrap();
        let body = issue_task_body(&issues[0]);
        assert!(already_imported(&body, &issues[0]));
        assert!(!already_imported(&body, &issues[1]));
        assert!(!already_imported("unrelated body", &issues[0]));
    }
}
