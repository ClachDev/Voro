//! The `gh` shell-outs for a task's tracked GitHub PR (DESIGN.md §11c):
//! opening the PR in a browser (jump-to-PR) and pulling its review comments
//! into a reject-with-feedback body. The parsing and formatting that matters
//! lives in `voro_core::pr` and is tested against canned strings; this module
//! is the thin, network-facing seam that invokes `gh` for real. Kept in the
//! `voro` crate so `voro_core` stays free of process I/O, same as issue import
//! and agent dispatch.

use std::process::{Command, Stdio};

use voro_core::{PrPlan, PrRef, Store, format_review_feedback, plan_pr};

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

/// Assemble the plan for opening a PR on a review task (DESIGN.md §8): its
/// branch, title, and summary body, validated in `voro_core::plan_pr`. Shared
/// by the CLI and TUI so both name the same gap when a task is not PR-ready,
/// and so each can quote the branch and title in its confirmation before
/// anything is pushed.
pub fn plan(store: &Store, task_id: i64) -> Result<PrPlan, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    let summary = store.latest_summary(task_id).map_err(|e| e.to_string())?;
    plan_pr(&task, summary.as_deref()).map_err(|e| e.to_string())
}

/// Create a ready-for-review GitHub PR for a review task and record its URL
/// (DESIGN.md §8): push the task's branch, open a non-draft PR whose title is
/// the task title and whose body is the completion summary, and store the URL
/// via the `set --pr` write path. No state change — the task stays in `review`
/// until a human accepts. The caller (CLI confirm, TUI modal) has already
/// gated on the operator; the dispatched agent still cannot publish, since
/// `pr` is operator-invoked. Only ever called when the task has no tracked PR;
/// a tracked one jumps to [`open`] instead.
pub fn create(store: &mut Store, task_id: i64) -> Result<String, String> {
    let plan = plan(store, task_id)?;
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    let project = store.project(task.project_id).map_err(|e| e.to_string())?;
    let url = open_pr_on_github(&project.path, &plan)?;
    // Canonicalise before storing, so the tracked URL matches what `set --pr`
    // would record and a later jump-to-PR is addressable.
    let pr = PrRef::parse(&url)
        .map_err(|e| format!("gh opened a PR but its URL was unusable ({url}): {e}"))?;
    store
        .set_pr(task_id, Some(&pr.url))
        .map_err(|e| e.to_string())?;
    Ok(format!("opened {} for task {task_id}", pr.url))
}

/// The forge-specific half of [`create`] (DESIGN.md §8), kept behind one seam:
/// push the branch and open a ready PR against the repo's default branch,
/// returning the PR URL. This is the only part that knows about GitHub. A
/// project whose checkout is not a GitHub repo gets a clear error pointing at
/// `voro open` — the local diff viewer (#65), which is the natural home for the
/// non-GitHub review medium if the two verbs later merge (DESIGN.md §8/§11).
fn open_pr_on_github(project_path: &str, plan: &PrPlan) -> Result<String, String> {
    ensure_github_repo(project_path)?;
    push_branch(project_path, &plan.branch)?;
    gh_pr_create(project_path, plan)
}

/// Refuse a non-GitHub checkout before pushing anything, pointing at the local
/// diff viewer instead — the seam where `open`'s behaviour for these projects
/// belongs (DESIGN.md §8).
fn ensure_github_repo(project_path: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner"])
        .current_dir(project_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run `gh` in {project_path}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{project_path} is not a GitHub repository, so `pr` cannot open a pull request there; \
         use `voro open <task-id>` to see its diff in the configured viewer instead"
    ))
}

/// Push the task's branch to `origin` so `gh pr create --head` can find it. The
/// dispatched agent deliberately has no push permission; `pr` is
/// operator-invoked, so this push is on the operator's behalf (DESIGN.md §8).
fn push_branch(project_path: &str, branch: &str) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["push", "origin", branch])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git in {project_path}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git push origin {branch}` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// Open the ready (non-draft) PR and return its URL, which `gh pr create`
/// prints on stdout. Title and body come from the plan; the base is the repo's
/// default branch, which `gh` fills in.
fn gh_pr_create(project_path: &str, plan: &PrPlan) -> Result<String, String> {
    let output = Command::new("gh")
        .args([
            "pr",
            "create",
            "--head",
            &plan.branch,
            "--title",
            &plan.title,
            "--body",
            &plan.body,
        ])
        .current_dir(project_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run `gh pr create`: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`gh pr create` failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let url = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.contains("://"))
        .unwrap_or("");
    if url.is_empty() {
        return Err(format!(
            "`gh pr create` succeeded but printed no PR URL: {}",
            stdout.trim()
        ));
    }
    Ok(url.to_string())
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
