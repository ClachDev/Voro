//! The `gh` shell-outs for a task's tracked GitHub PR (DESIGN.md §11c):
//! opening the PR in a browser, pulling its review comments into a
//! reject-with-feedback body, and creating a PR on review. Parsing and
//! formatting live in `voro_core::pr`; this is the network-facing seam that
//! invokes `gh`, kept in the `voro` crate so `voro_core` stays free of
//! process I/O.

use std::process::{Command, Stdio};

use voro_core::{
    Mergeability, PrPlan, PrRef, Project, ReviewMedium, Store, format_review_feedback,
    parse_mergeable, plan_pr,
};

/// Resolve a task's tracked PR, erroring with a fix-it hint when none is set.
fn tracked_pr(store: &Store, task_id: i64) -> Result<PrRef, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    let url = task.pr_url.ok_or_else(|| {
        format!("task {task_id} has no tracked PR — set one with `voro set {task_id} --pr URL`")
    })?;
    PrRef::parse(&url).map_err(|e| e.to_string())
}

/// Open a task's tracked PR in a browser via `gh pr view --web` (DESIGN.md
/// §11c). Spawned detached and reaped like the viewer in `dispatch::open`, so
/// nothing lingers as a zombie.
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
/// (DESIGN.md §11c). Fetches review summaries and inline diff comments via `gh
/// api`, scoped to the PR's *base* repo so a fork's checkout origin is
/// irrelevant. Errors when the PR has no relayable comments.
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
/// by the CLI and TUI so both name the same gap when a task is not PR-ready.
pub fn plan(store: &Store, task_id: i64) -> Result<PrPlan, String> {
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    let summary = store.latest_summary(task_id).map_err(|e| e.to_string())?;
    plan_pr(&task, summary.as_deref()).map_err(|e| e.to_string())
}

/// Create a ready-for-review GitHub PR for a review task and record its URL
/// (DESIGN.md §8): push the task's branch, open a non-draft PR titled with the
/// task title and bodied with the completion summary, and store the URL. No
/// state change — the task stays in `review` until a human accepts. The
/// operator has already been gated by the caller; `pr` is operator-invoked, so
/// the dispatched agent still cannot publish. Only called when no PR is
/// tracked; a tracked one jumps to [`open`] instead.
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

/// Ask GitHub whether a review task's tracked PR still merges cleanly with its
/// base (DESIGN.md §8) — a purely informational signal that the branch has gone
/// stale. One `gh pr view <url> --json mergeable` call for the one task, run
/// on demand from `voro show` and the TUI detail pane, never per rendered row.
/// The URL carries the host, so an enterprise instance is reached without
/// `--hostname`. A task with no tracked PR, an unparseable URL, or any `gh`
/// failure (missing, unauthenticated, network) degrades to
/// [`Mergeability::Unknown`]: no signal, so the marker shows only on a definite
/// `CONFLICTING`. The verdict itself is decided by `voro_core::parse_mergeable`.
pub fn conflict_status(store: &Store, task_id: i64) -> Mergeability {
    let Ok(task) = store.task(task_id) else {
        return Mergeability::Unknown;
    };
    let Some(url) = task.pr_url.as_deref() else {
        return Mergeability::Unknown;
    };
    let Ok(pr) = PrRef::parse(url) else {
        return Mergeability::Unknown;
    };
    let output = Command::new("gh")
        .args(["pr", "view", &pr.url, "--json", "mergeable"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => parse_mergeable(&String::from_utf8_lossy(&o.stdout)),
        _ => Mergeability::Unknown,
    }
}

/// Resolve a project's review medium (DESIGN.md §8). The decision is
/// `ReviewAction::resolve` in `voro-core`; this seam supplies the one input it
/// cannot compute — whether the checkout is a GitHub repo `gh` can address —
/// and only when the action is `auto`, so pinned media never pay for the probe.
pub fn resolve_medium(project: &Project) -> ReviewMedium {
    let on_github = project.review_action.needs_probe() && is_github_repo(&project.path);
    project.review_action.resolve(on_github)
}

/// The probe behind the `auto` review action: whether `gh` can address the
/// checkout as a GitHub repository. A missing or unauthenticated `gh` reads
/// as "not GitHub" — the viewer is then the only workable medium — while the
/// explicit `pr` action still reports the failure via [`ensure_github_repo`].
fn is_github_repo(project_path: &str) -> bool {
    Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner"])
        .current_dir(project_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The forge-specific half of [`create`] (DESIGN.md §8): push the branch and
/// open a ready PR against the repo's default branch, returning the PR URL. A
/// non-GitHub checkout gets a clear error pointing at the viewer medium.
fn open_pr_on_github(project_path: &str, plan: &PrPlan) -> Result<String, String> {
    ensure_github_repo(project_path)?;
    push_branch(project_path, &plan.branch)?;
    gh_pr_create(project_path, plan)
}

/// Refuse a non-GitHub checkout before pushing anything, pointing at the
/// viewer medium instead — reached when a project's review action is pinned
/// to `pr` (or resolves there) but the checkout cannot take one (DESIGN.md §8).
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
         use `voro open <task-id>` to see its diff in a viewer, or point the project at one \
         with `voro project action <project> viewer`"
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
