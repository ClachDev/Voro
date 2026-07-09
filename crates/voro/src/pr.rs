//! The `gh` shell-outs for a task's tracked GitHub PR (DESIGN.md §11c):
//! opening the PR in a browser (jump-to-PR) and pulling its review comments
//! into a reject-with-feedback body. The parsing and formatting that matters
//! lives in `voro_core::pr` and is tested against canned strings; this module
//! is the thin, network-facing seam that invokes `gh` for real. Kept in the
//! `voro` crate so `voro_core` stays free of process I/O, same as issue import
//! and agent dispatch.

use std::process::{Command, Stdio};

use voro_core::{PrRef, Store, format_review_feedback};

/// Resolve a task's tracked PR, erroring with a fix-it hint when none is set.
fn tracked_pr(store: &Store, task_id: i64) -> Result<PrRef, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    let url = task.pr_url.ok_or_else(|| {
        format!("task {task_id} has no tracked PR — set one with `voro set {task_id} --pr URL`")
    })?;
    PrRef::parse(&url).map_err(|e| e.to_string())
}

/// Open a task's tracked PR in a browser via `gh pr view --web` (DESIGN.md
/// §11c): the review action's jump-to-PR, landing on the page where the diff
/// and its comments already live. Spawned detached and reaped like the viewer
/// in `dispatch::open`, so nothing lingers as a zombie.
pub fn open(store: &Store, task_id: i64) -> Result<String, String> {
    let pr = tracked_pr(store, task_id)?;
    let mut child = Command::new("gh")
        .args(["pr", "view", "--web", &pr.url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run `gh` to open the PR: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(format!("opening {} in the browser", pr.url))
}

/// Pull a task's tracked PR's review comments into a reject-with-feedback body
/// (DESIGN.md §11c). Fetches the PR's review summaries and inline diff comments
/// via `gh api` — scoped to the PR's *base* repo, so a fork's checkout origin
/// is irrelevant — and hands both to `voro_core` to format. Errors when the PR
/// has no relayable comments, rather than returning an empty rejection.
pub fn pull_review_feedback(store: &Store, task_id: i64) -> Result<String, String> {
    let pr = tracked_pr(store, task_id)?;
    let reviews = gh_api(&pr, &pr.api_path("reviews"))?;
    let comments = gh_api(&pr, &pr.api_path("comments"))?;
    let body = format_review_feedback(&pr, &reviews, &comments).map_err(|e| e.to_string())?;
    if body.trim().is_empty() {
        return Err(format!("no review comments to pull from {}", pr.url));
    }
    Ok(body)
}

/// Run `gh api <path>`, targeting the PR's host so an enterprise instance is
/// reached rather than github.com. Returns the raw JSON stdout.
fn gh_api(pr: &PrRef, path: &str) -> Result<String, String> {
    let mut cmd = Command::new("gh");
    cmd.args(["api", path]);
    if pr.host != "github.com" {
        cmd.args(["--hostname", &pr.host]);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run `gh api {path}`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`gh api {path}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}
