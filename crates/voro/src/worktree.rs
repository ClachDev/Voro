//! Tearing down the git worktree a dispatched agent left behind when its task
//! closes (DESIGN.md §8). A dispatched agent does its work in a throwaway
//! worktree of the project checkout; nothing else prunes those, so the terminal
//! transition that closes the task (`Accept`, `Abandon`) removes the one on the
//! task's branch. Like dispatch, the git/`gh` process and filesystem I/O lives
//! here in the `voro` crate — `voro-core` stays pure of it.
//!
//! Everything here is best-effort and safe by construction: the worktree is
//! removed non-forced, so git refuses a dirty one and the transition still
//! stands; the branch is only deleted when its work is verifiably upstream (a
//! merged PR, or a plain `git branch -d` that git itself accepts). Nothing is
//! removed without the operator being shown what and why first — the caller
//! confirms before calling [`Cleanup::execute`].

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use voro_core::Task;

/// A planned worktree teardown for a just-closed task: the worktree on the
/// task's branch that will be removed, and how its branch will then be deleted.
/// Built by [`Cleanup::plan`] before the operator confirms, so [`describe`] can
/// state exactly what is about to happen.
///
/// [`describe`]: Cleanup::describe
pub struct Cleanup {
    project_path: String,
    branch: String,
    worktree: PathBuf,
    branch_plan: BranchPlan,
}

/// How the branch will be deleted once its worktree is gone. Squash-merging
/// (this repo's convention) leaves the branch tip a non-ancestor of `main`, so
/// `git branch -d` cannot see it as merged — a merged PR is the reliable signal
/// that authorises the force-delete.
enum BranchPlan {
    /// A merged PR authorises `git branch -D`.
    ForceDelete,
    /// `git branch -d`, which git refuses for an unmerged branch — so an
    /// unmerged branch simply survives.
    SafeDelete,
}

impl Cleanup {
    /// Plan the teardown for a task that has just reached a terminal state, or
    /// `None` when there is nothing to do — the task carries no branch, or no
    /// worktree of the project checkout is on that branch. The `gh` merge check
    /// is only run once a worktree is found, so the common no-op path stays
    /// local and cheap.
    pub fn plan(task: &Task, project_path: &str) -> Result<Option<Cleanup>, String> {
        let Some(branch) = task.branch.clone() else {
            return Ok(None);
        };
        let Some(worktree) = worktree_on_branch(project_path, &branch)? else {
            return Ok(None);
        };
        let branch_plan = match task.pr_url.as_deref() {
            Some(url) if pr_is_merged(url) => BranchPlan::ForceDelete,
            _ => BranchPlan::SafeDelete,
        };
        Ok(Some(Cleanup {
            project_path: project_path.to_string(),
            branch,
            worktree,
            branch_plan,
        }))
    }

    /// The worktree that will be removed — for a caller reporting a declined
    /// cleanup without running it.
    pub fn worktree(&self) -> &Path {
        &self.worktree
    }

    /// A one-line account of exactly what [`execute`](Cleanup::execute) will
    /// remove and why the branch is (or isn't) judged safe to delete, shown to
    /// the operator before they confirm.
    pub fn describe(&self) -> String {
        let branch_note = match self.branch_plan {
            BranchPlan::ForceDelete => {
                format!("and delete branch `{}` (its PR is merged)", self.branch)
            }
            BranchPlan::SafeDelete => {
                format!(
                    "and delete branch `{}` if git confirms it merged",
                    self.branch
                )
            }
        };
        format!("remove worktree {} {branch_note}", self.worktree.display())
    }

    /// Remove the worktree and then delete the branch per the plan, returning a
    /// summary of what actually happened. The removal is non-forced: a dirty
    /// worktree makes git refuse, which is reported (branch left alone) rather
    /// than forced — the caller's transition has already committed and stands
    /// regardless.
    pub fn execute(self) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.project_path)
            .args(["worktree", "remove"])
            .arg(&self.worktree)
            .stdin(Stdio::null())
            .output();
        let output = match output {
            Ok(output) => output,
            Err(e) => {
                return format!(
                    "could not run git to remove {}: {e}",
                    self.worktree.display()
                );
            }
        };
        if !output.status.success() {
            return format!(
                "left worktree {} in place (branch `{}` kept): {}",
                self.worktree.display(),
                self.branch,
                String::from_utf8_lossy(&output.stderr).trim(),
            );
        }
        format!(
            "removed worktree {}{}",
            self.worktree.display(),
            self.delete_branch()
        )
    }

    /// Delete the now-unchecked-out branch. Force-deletes only a
    /// PR-merged branch; otherwise a plain `-d` that git may safely refuse.
    fn delete_branch(&self) -> String {
        let flag = match self.branch_plan {
            BranchPlan::ForceDelete => "-D",
            BranchPlan::SafeDelete => "-d",
        };
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.project_path)
            .args(["branch", flag, &self.branch])
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.success() => format!("; deleted branch `{}`", self.branch),
            Ok(output) => match self.branch_plan {
                BranchPlan::SafeDelete => {
                    format!("; kept branch `{}` (git sees it unmerged)", self.branch)
                }
                BranchPlan::ForceDelete => format!(
                    "; could not delete branch `{}`: {}",
                    self.branch,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            },
            Err(e) => format!(
                "; could not run git to delete branch `{}`: {e}",
                self.branch
            ),
        }
    }
}

/// The worktree of `project_path` whose checked-out branch is `branch`, if any.
/// The primary worktree (the project checkout itself, always listed first) is
/// never a candidate — it is not a throwaway dispatch worktree, and git would
/// refuse to remove it anyway. Shared with `dispatch::open`, which opens a
/// review diff in the same worktree cleanup later tears down (DESIGN.md §8).
pub(crate) fn worktree_on_branch(
    project_path: &str,
    branch: &str,
) -> Result<Option<PathBuf>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["worktree", "list", "--porcelain"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git in {project_path}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`git worktree list` failed in {project_path}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let target = format!("refs/heads/{branch}");
    for (i, block) in stdout
        .split("\n\n")
        .filter(|b| !b.trim().is_empty())
        .enumerate()
    {
        let primary = i == 0;
        let mut path = None;
        let mut on_branch = false;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if line.strip_prefix("branch ") == Some(target.as_str()) {
                on_branch = true;
            }
        }
        if !primary && on_branch {
            return Ok(path);
        }
    }
    Ok(None)
}

/// Whether `gh` reports the PR at `url` as merged. Best-effort: any failure to
/// run `gh`, reach the host, or read a `MERGED` state reads as "not merged", so
/// an unverifiable branch falls back to the safe `git branch -d` path rather
/// than a force-delete. `gh` infers the host from the URL, so an enterprise PR
/// is reached without extra flags.
fn pr_is_merged(url: &str) -> bool {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            url,
            "--json",
            "state,mergedAt",
            "--jq",
            ".state",
        ])
        .stdin(Stdio::null())
        .output();
    matches!(output, Ok(o) if o.status.success()
        && String::from_utf8_lossy(&o.stdout).trim() == "MERGED")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use voro_core::{NewTask, Priority, Store, TaskState};

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A git repo at `<root>/project` with one commit on `main` and a
    /// `git`-configured identity, ready to grow worktrees.
    fn repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "voro-worktree-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["config", "user.email", "t@example.com"]);
        git(&project, &["config", "user.name", "Test"]);
        std::fs::write(project.join("README"), "hello\n").unwrap();
        git(&project, &["add", "-A"]);
        git(&project, &["commit", "-q", "-m", "init"]);
        project
    }

    /// Add a worktree at `<project>/../wt-<branch>` on a fresh `branch`, with an
    /// optional extra commit on it so it diverges from `main`.
    fn add_worktree(project: &Path, branch: &str, commit: bool) -> PathBuf {
        let wt = project.parent().unwrap().join(format!("wt-{branch}"));
        git(
            project,
            &["worktree", "add", "-q", "-b", branch, wt.to_str().unwrap()],
        );
        if commit {
            std::fs::write(wt.join("work.txt"), "work\n").unwrap();
            git(&wt, &["add", "-A"]);
            git(&wt, &["commit", "-q", "-m", "work"]);
        }
        wt
    }

    fn task(project_path: &str, branch: Option<&str>, pr_url: Option<&str>) -> (Store, Task) {
        let mut store = Store::open_in_memory().unwrap();
        let p = store.create_project("proj", project_path).unwrap();
        let id = store
            .create_task(NewTask {
                project_id: p.id,
                title: "t".into(),
                body: "b".into(),
                priority: Priority::P1,
                state: TaskState::Ready,
                agent: None,
                human: false,
            })
            .unwrap()
            .id;
        if let Some(b) = branch {
            store.set_branch(id, Some(b)).unwrap();
        }
        if let Some(url) = pr_url {
            store.set_pr(id, Some(url)).unwrap();
        }
        let task = store.task(id).unwrap();
        (store, task)
    }

    fn branch_exists(project: &Path, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(project)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[test]
    fn no_branch_is_a_no_op() {
        let project = repo();
        let (_store, task) = task(project.to_str().unwrap(), None, None);
        assert!(
            Cleanup::plan(&task, project.to_str().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn branch_without_a_worktree_is_a_no_op() {
        let project = repo();
        // A branch that exists but is checked out in no worktree.
        git(&project, &["branch", "lonely"]);
        let (_store, task) = task(project.to_str().unwrap(), Some("lonely"), None);
        assert!(
            Cleanup::plan(&task, project.to_str().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_primary_worktree_is_never_a_candidate() {
        let project = repo();
        // The project checkout itself is on `main`; a task carrying `main`
        // must not resolve to the primary worktree.
        let (_store, task) = task(project.to_str().unwrap(), Some("main"), None);
        assert!(
            Cleanup::plan(&task, project.to_str().unwrap())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn removes_a_clean_worktree_and_deletes_a_merged_branch() {
        let project = repo();
        let wt = add_worktree(&project, "feat", true);
        // Merge feat into main so a plain `git branch -d` recognises it.
        git(&project, &["merge", "-q", "feat"]);
        let (_store, t) = task(project.to_str().unwrap(), Some("feat"), None);

        let plan = Cleanup::plan(&t, project.to_str().unwrap())
            .unwrap()
            .unwrap();
        let summary = plan.execute();

        assert!(!wt.exists(), "worktree dir should be gone: {summary}");
        assert!(summary.contains("removed worktree"), "{summary}");
        assert!(summary.contains("deleted branch"), "{summary}");
        assert!(!branch_exists(&project, "feat"));
    }

    #[test]
    fn keeps_an_unmerged_branch_after_removing_its_worktree() {
        let project = repo();
        let wt = add_worktree(&project, "feat", true);
        // feat is never merged into main, so `git branch -d` must refuse it.
        let (_store, t) = task(project.to_str().unwrap(), Some("feat"), None);

        let plan = Cleanup::plan(&t, project.to_str().unwrap())
            .unwrap()
            .unwrap();
        let summary = plan.execute();

        assert!(!wt.exists(), "worktree dir should be gone: {summary}");
        assert!(summary.contains("removed worktree"), "{summary}");
        assert!(summary.contains("kept branch"), "{summary}");
        assert!(
            branch_exists(&project, "feat"),
            "unmerged branch must survive"
        );
    }

    #[test]
    fn force_deletes_an_unmerged_branch_when_its_pr_is_merged() {
        let project = repo();
        let wt = add_worktree(&project, "feat", true);
        // feat is unmerged in git terms (mimicking a squash-merge), but we
        // drive the force-delete path directly rather than through `gh`.
        let plan = Cleanup {
            project_path: project.to_str().unwrap().to_string(),
            branch: "feat".to_string(),
            worktree: wt.clone(),
            branch_plan: BranchPlan::ForceDelete,
        };
        let summary = plan.execute();

        assert!(!wt.exists(), "{summary}");
        assert!(summary.contains("deleted branch"), "{summary}");
        assert!(
            !branch_exists(&project, "feat"),
            "force-delete should remove it"
        );
    }

    #[test]
    fn a_dirty_worktree_survives_and_is_reported() {
        let project = repo();
        let wt = add_worktree(&project, "feat", true);
        // Dirty the worktree so a non-forced remove refuses.
        std::fs::write(wt.join("work.txt"), "uncommitted change\n").unwrap();
        let (_store, t) = task(project.to_str().unwrap(), Some("feat"), None);

        let plan = Cleanup::plan(&t, project.to_str().unwrap())
            .unwrap()
            .unwrap();
        let summary = plan.execute();

        assert!(wt.exists(), "dirty worktree must survive: {summary}");
        assert!(summary.contains("left worktree"), "{summary}");
        assert!(branch_exists(&project, "feat"), "branch must survive too");
    }
}
