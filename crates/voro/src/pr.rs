//! The `gh` shell-outs for a task's tracked GitHub PR (DESIGN.md §11c):
//! opening the PR in a browser, pulling its review comments into a
//! reject-with-feedback body, and creating a PR on review. Parsing and
//! formatting live in `voro_core::pr`; this is the network-facing seam that
//! invokes `gh`, kept in the `voro` crate so `voro_core` stays free of
//! process I/O.

use std::process::{Command, Stdio};

use voro_core::{
    Mergeability, PrPlan, PrRef, PrRevisions, Project, ReviewDiff, ReviewMedium, Store,
    format_review_feedback, parse_mergeable, parse_pr_revisions, plan_pr, plan_review_diff,
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
/// §11c), narrowed to what the rework added when the operator has already
/// reviewed and sent this task back (§8). A first review, and every state but
/// `review`, opens the PR whole exactly as before.
pub fn open(store: &Store, task_id: i64) -> Result<String, String> {
    let pr = tracked_pr(store, task_id)?;
    match review_diff(store, task_id, &pr) {
        ReviewDiff::Since { since, head } => open_url(&pr.range_url(&since, &head))
            .map(|opened| format!("{opened} — the changes since your last review")),
        ReviewDiff::Full { notice } => {
            let opened = open_url(&pr.url)?;
            Ok(match notice {
                Some(notice) => format!("{opened} — {notice}"),
                None => opened,
            })
        }
    }
}

/// What a look at this task's PR should show (DESIGN.md §8). Only a `review`
/// task that carries a recorded reviewed revision pays for the `gh` round-trip
/// — everything else short-circuits to the full diff, so a first review is
/// network-identical to what it was.
fn review_diff(store: &Store, task_id: i64, pr: &PrRef) -> ReviewDiff {
    let in_review = store
        .task(task_id)
        .is_ok_and(|t| t.state == voro_core::TaskState::Review);
    let reviewed = match in_review {
        true => store.last_reviewed(task_id).ok().flatten(),
        false => None,
    };
    let Some(reviewed) = reviewed else {
        return ReviewDiff::Full { notice: None };
    };
    let revisions = pr_revisions(pr);
    plan_review_diff(
        Some(&reviewed),
        revisions.head.as_deref(),
        revisions.contains(&reviewed),
    )
}

/// A PR's head and commit list (`gh pr view --json headRefOid,commits`), which
/// together say whether the revision the operator reviewed is still on the
/// branch. Any failure — missing `gh`, no network, an unreadable answer — reads
/// as no revisions, which degrades the review to the full diff rather than
/// erroring.
fn pr_revisions(pr: &PrRef) -> PrRevisions {
    let output = Command::new("gh")
        .args(["pr", "view", &pr.url, "--json", "headRefOid,commits"])
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => parse_pr_revisions(&String::from_utf8_lossy(&o.stdout)),
        _ => PrRevisions::default(),
    }
}

/// The viewer medium's half of the same decision (DESIGN.md §8), answered from
/// the checkout rather than from GitHub: the branch's local tip, and whether the
/// reviewed revision is still an ancestor of it. Feeds `{base}` in the viewer
/// template, so `git diff {base}` shows the rework alone.
pub fn local_review_diff(store: &Store, task_id: i64, repo_path: &str, branch: &str) -> ReviewDiff {
    let Some(reviewed) = store.last_reviewed(task_id).ok().flatten() else {
        return ReviewDiff::Full { notice: None };
    };
    let head = rev_parse(repo_path, branch);
    plan_review_diff(
        Some(&reviewed),
        head.as_deref(),
        is_ancestor(repo_path, &reviewed, branch),
    )
}

/// Where the revision an operator has just reviewed can be read from: the
/// task's tracked PR, and the local tip of its branch as the fallback. Holds
/// owned strings and no `Store` handle, so it can move into the background
/// thread that resolves it (`crate::probe`, DESIGN.md §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewedSource {
    pr_url: Option<String>,
    /// The checkout the branch lives in, and the branch — a task dispatched
    /// into a second repo has its branch there, not in the project's own.
    branch: Option<(String, String)>,
}

#[cfg(test)]
impl ReviewedSource {
    /// A source with nothing to read, which resolves to `None` without running
    /// a subprocess — enough to exercise the runner around it without `gh`.
    pub fn empty() -> Self {
        ReviewedSource {
            pr_url: None,
            branch: None,
        }
    }
}

/// The store half of a reviewed-revision capture (DESIGN.md §8): a few cheap
/// SQLite reads, so it runs on the TUI event loop. `None` when the task has
/// neither a tracked PR nor a branch in a resolvable checkout, which is the
/// case that starts no capture at all.
pub fn reviewed_source(store: &Store, task_id: i64) -> Option<ReviewedSource> {
    let task = store.task(task_id).ok()?;
    let branch = task.branch.as_deref().and_then(|branch| {
        let repo = store.repo_for_task(&task).ok()?;
        Some((repo.path, branch.to_string()))
    });
    let source = ReviewedSource {
        pr_url: task.pr_url,
        branch,
    };
    (source.pr_url.is_some() || source.branch.is_some()).then_some(source)
}

/// The subprocess half of the capture (DESIGN.md §8), which is why it takes a
/// [`ReviewedSource`] rather than a task: the TUI runs it off the event loop,
/// where no `Store` is in reach. A tracked PR's head is what was actually
/// reviewed, a task without one falls back to the local tip of its branch, and
/// neither being readable yields `None` — an unreadable revision costs a full
/// diff next time, never a failed reject.
pub fn resolve_reviewed(source: &ReviewedSource) -> Option<String> {
    let from_pr = source
        .pr_url
        .as_deref()
        .and_then(|url| PrRef::parse(url).ok())
        .and_then(|pr| pr_head(&pr));
    from_pr.or_else(|| {
        let (repo_path, branch) = source.branch.as_ref()?;
        rev_parse(repo_path, branch)
    })
}

/// A PR's head revision (`gh pr view --json headRefOid`). The capture wants the
/// head alone; the commit list [`pr_revisions`] also fetches answers a
/// reachability question only the read path puts.
fn pr_head(pr: &PrRef) -> Option<String> {
    let output = Command::new("gh")
        .args(["pr", "view", &pr.url, "--json", "headRefOid"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_pr_revisions(&String::from_utf8_lossy(&output.stdout)).head
}

/// Record the revision the operator has just reviewed and sent back (DESIGN.md
/// §8), so the next look at this task is narrowed to the rework. Blocking, which
/// is what a one-shot CLI verb should be; the TUI runs the same two halves
/// either side of its event loop instead (`crate::probe::ReviewedCapture`).
pub fn record_reviewed(store: &mut Store, task_id: i64) {
    let Some(source) = reviewed_source(store, task_id) else {
        return;
    };
    if let Some(sha) = resolve_reviewed(&source) {
        let _ = store.record_reviewed(task_id, &sha);
    }
}

/// Resolve a revision to a full SHA in a checkout, or `None` when git cannot.
fn rev_parse(repo_path: &str, rev: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", "--verify", &format!("{rev}^{{commit}}")])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Whether `ancestor` is still reachable from `descendant` — false once a
/// force-push or rebase has rewritten it away, which is what sends a re-review
/// back to the full diff.
fn is_ancestor(repo_path: &str, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Open a PR URL in a browser, taking the URL rather than a task so a
/// just-created PR can be shown without re-reading the store (DESIGN.md §8).
/// Spawned detached and reaped like the viewer in `dispatch::open`, so nothing
/// lingers as a zombie.
pub fn open_url(url: &str) -> Result<String, String> {
    let mut child = Command::new("gh")
        .args(["pr", "view", "--web", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run `gh` to open the PR: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(format!("opening {url} in the browser"))
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
/// tracked; a tracked one jumps to [`open`] instead. Returns the canonical URL
/// so a caller can chain into [`open_url`] without re-reading the task.
pub fn create(store: &mut Store, task_id: i64) -> Result<String, String> {
    let plan = plan(store, task_id)?;
    let task = store.task(task_id).map_err(|e| e.to_string())?;
    // The task's own checkout: a task dispatched into a second repo has its
    // branch there, so that is where the push and `gh pr create` must run.
    let repo = store.repo_for_task(&task).map_err(|e| e.to_string())?;
    let url = open_pr_on_github(&repo.path, &plan)?;
    // Canonicalise before storing, so the tracked URL matches what `set --pr`
    // would record and a later jump-to-PR is addressable.
    let pr = PrRef::parse(&url)
        .map_err(|e| format!("gh opened a PR but its URL was unusable ({url}): {e}"))?;
    store
        .set_pr(task_id, Some(&pr.url))
        .map_err(|e| e.to_string())?;
    Ok(pr.url)
}

/// Ask GitHub whether a review task's tracked PR still merges cleanly with its
/// base (DESIGN.md §8) — a purely informational signal that the branch has gone
/// stale. One `gh pr view <url> --json mergeable` call for the one task, run on
/// demand from `voro show`, where blocking is fine. A task with no tracked PR
/// degrades to [`Mergeability::Unknown`], as does every failure in
/// [`conflict_status_url`].
pub fn conflict_status(store: &Store, task_id: i64) -> Mergeability {
    let Ok(task) = store.task(task_id) else {
        return Mergeability::Unknown;
    };
    let Some(url) = task.pr_url.as_deref() else {
        return Mergeability::Unknown;
    };
    conflict_status_url(url)
}

/// The blocking `gh` call behind [`conflict_status`], taking the PR URL alone so
/// the TUI can run it on a background thread with no store handle in reach
/// (`crate::probe`). The URL carries the host, so an enterprise instance is
/// reached without `--hostname`. An unparseable URL or any `gh` failure
/// (missing, unauthenticated, network) degrades to [`Mergeability::Unknown`]: no
/// signal, so the marker shows only on a definite `CONFLICTING`. The verdict
/// itself is decided by `voro_core::parse_mergeable`.
pub fn conflict_status_url(url: &str) -> Mergeability {
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
/// The action stays per-project; the probe runs against `checkout`, the task's
/// resolved repo, so a multi-repo project answers per task (DESIGN.md §8).
pub fn resolve_medium(project: &Project, checkout: &str) -> ReviewMedium {
    let on_github = project.review_action.needs_probe() && is_github_repo(checkout);
    project.review_action.resolve(on_github)
}

/// The probe behind the `auto` review action: whether `gh` can address the
/// checkout as a GitHub repository. A missing or unauthenticated `gh` reads
/// as "not GitHub" — the viewer is then the only workable medium — while the
/// explicit `pr` action still reports the failure via [`ensure_github_repo`].
fn is_github_repo(repo_path: &str) -> bool {
    Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner"])
        .current_dir(repo_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The forge-specific half of [`create`] (DESIGN.md §8): push the branch and
/// open a ready PR against the repo's default branch, returning the PR URL. A
/// non-GitHub checkout gets a clear error pointing at the viewer medium.
fn open_pr_on_github(repo_path: &str, plan: &PrPlan) -> Result<String, String> {
    ensure_github_repo(repo_path)?;
    push_branch(repo_path, &plan.branch)?;
    gh_pr_create(repo_path, plan)
}

/// Refuse a non-GitHub checkout before pushing anything, pointing at the
/// viewer medium instead — reached when a project's review action is pinned
/// to `pr` (or resolves there) but the checkout cannot take one (DESIGN.md §8).
fn ensure_github_repo(repo_path: &str) -> Result<(), String> {
    let output = Command::new("gh")
        .args(["repo", "view", "--json", "nameWithOwner"])
        .current_dir(repo_path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run `gh` in {repo_path}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{repo_path} is not a GitHub repository, so `pr` cannot open a pull request there; \
         use `voro open <task-id>` to see its diff in a viewer, or point the project at one \
         with `voro project action <project> viewer`"
    ))
}

/// Push the task's branch to `origin` so `gh pr create --head` can find it. The
/// dispatched agent deliberately has no push permission; `pr` is
/// operator-invoked, so this push is on the operator's behalf (DESIGN.md §8).
fn push_branch(repo_path: &str, branch: &str) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["push", "origin", branch])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git in {repo_path}: {e}"))?;
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
fn gh_pr_create(repo_path: &str, plan: &PrPlan) -> Result<String, String> {
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
        .current_dir(repo_path)
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
