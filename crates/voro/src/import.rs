//! The `import` verb's I/O: shelling out to `gh issue list` and turning its
//! output into tasks. The mapping and idempotency logic that matters is in
//! `voro_core::import` and is unit-tested there against canned JSON; this
//! module is the thin, deliberately untested-against-the-network seam that
//! calls `gh` for real and hands its stdout to that logic.

use std::process::Command;

use voro_core::{GithubIssue, Project, Store, Task, already_imported, issue_new_task};

/// The result of one `voro import` run.
pub struct ImportSummary {
    pub imported: Vec<Task>,
    pub skipped: usize,
}

/// Run `gh issue list --json number,title,body,url` in `project_path`,
/// scoped to `repo` if given. This is the only place Voro invokes `gh` — the
/// human runs the verb; nothing else may trigger it.
pub fn fetch_issues(project_path: &str, repo: Option<&str>) -> Result<String, String> {
    let mut cmd = Command::new("gh");
    cmd.args(["issue", "list", "--json", "number,title,body,url"]);
    if let Some(repo) = repo {
        cmd.args(["-R", repo]);
    }
    cmd.current_dir(project_path);
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run `gh issue list`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`gh issue list` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Map `gh issue list` JSON onto tasks in `project`, creating one per issue
/// not already imported. Idempotent: an issue already captured (its URL
/// appears in an existing task's body, this project's or a prior run's) is
/// skipped rather than duplicated. Takes no dependency on `gh` itself, so it
/// is fully testable against canned JSON.
pub fn import_issues(
    store: &mut Store,
    project: &Project,
    json: &str,
) -> Result<ImportSummary, String> {
    let issues = GithubIssue::parse_list(json).map_err(|e| e.to_string())?;
    let mut bodies: Vec<String> = store
        .tasks()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|t| t.project_id == project.id)
        .map(|t| t.body)
        .collect();

    let mut imported = Vec::new();
    let mut skipped = 0;
    for issue in &issues {
        if bodies.iter().any(|b| already_imported(b, issue)) {
            skipped += 1;
            continue;
        }
        let task = store
            .create_task(issue_new_task(project.id, issue))
            .map_err(|e| e.to_string())?;
        bodies.push(task.body.clone());
        imported.push(task);
    }
    Ok(ImportSummary { imported, skipped })
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

    fn project(store: &mut Store) -> Project {
        store.create_project("widget", "/tmp/widget").unwrap()
    }

    #[test]
    fn imports_each_issue_as_a_proposed_task() {
        let mut s = Store::open_in_memory().unwrap();
        let p = project(&mut s);
        let summary = import_issues(&mut s, &p, CANNED).unwrap();
        assert_eq!(summary.imported.len(), 2);
        assert_eq!(summary.skipped, 0);
        for task in &summary.imported {
            assert_eq!(task.state, voro_core::TaskState::Proposed);
        }
        assert!(summary.imported[0].body.contains("issues/42"));
        assert!(summary.imported[1].body.contains("issues/7"));
    }

    #[test]
    fn importing_twice_is_idempotent() {
        let mut s = Store::open_in_memory().unwrap();
        let p = project(&mut s);
        import_issues(&mut s, &p, CANNED).unwrap();
        let second = import_issues(&mut s, &p, CANNED).unwrap();
        assert_eq!(second.imported.len(), 0);
        assert_eq!(second.skipped, 2);
        assert_eq!(s.tasks().unwrap().len(), 2);
    }

    const DOUBLED: &str = r#"[
        {
            "number": 42,
            "title": "Crash on empty input",
            "body": "Steps to reproduce",
            "url": "https://github.com/acme/widget/issues/42"
        },
        {
            "number": 42,
            "title": "Crash on empty input",
            "body": "Steps to reproduce",
            "url": "https://github.com/acme/widget/issues/42"
        }
    ]"#;

    #[test]
    fn duplicate_issues_within_one_batch_are_only_imported_once() {
        let mut s = Store::open_in_memory().unwrap();
        let p = project(&mut s);
        let summary = import_issues(&mut s, &p, DOUBLED).unwrap();
        assert_eq!(summary.imported.len(), 1);
        assert_eq!(summary.skipped, 1);
    }

    #[test]
    fn import_is_scoped_to_the_project() {
        let mut s = Store::open_in_memory().unwrap();
        let a = project(&mut s);
        let b = s.create_project("other", "/tmp/other").unwrap();
        import_issues(&mut s, &a, CANNED).unwrap();
        let summary = import_issues(&mut s, &b, CANNED).unwrap();
        assert_eq!(
            summary.imported.len(),
            2,
            "a different project's tasks don't count as imported"
        );
    }
}
