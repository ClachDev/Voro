//! The operator-invoked git half of the conflict-resolution action (DESIGN.md
//! §8): updating the base branch in the project's primary checkout — never the
//! task's worktree — before the task's session is continued with the canned
//! conflict feedback. The precondition check and the feedback body are
//! `voro_core::plan_rebase`; the transition and continuation are the same
//! `RejectWork` + continue path `reject` uses. Kept beside the other git/`gh`
//! seams so `voro-core` stays free of process I/O.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::dispatch::append_launch_log;

/// Bring the checkout's local `base` up to date with `origin` so the agent has
/// something concrete to rebase onto. When the checkout is sitting on `base`,
/// a fast-forward-only pull updates the working tree too; on any other branch
/// (or a detached HEAD) a `fetch origin base:base` moves only the ref, leaving
/// the working tree untouched. Either command is breadcrumbed to the launch
/// log before it runs, and a failure — a diverged local base, no `origin`, a
/// non-fast-forward — is surfaced verbatim rather than swallowed.
pub fn update_base(project_path: &str, base: &str, launch_log: &Path) -> Result<String, String> {
    let refspec = format!("{base}:{base}");
    let args: Vec<&str> = if current_branch(project_path).as_deref() == Some(base) {
        vec!["pull", "--ff-only", "origin", base]
    } else {
        vec!["fetch", "origin", &refspec]
    };
    let display = format!("git {}", args.join(" "));
    append_launch_log(
        launch_log,
        &format!("update-base: {display} (cwd {project_path})"),
    );
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git in {project_path}: {e}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        let failure = format!("`{display}` failed in {project_path}: {}", detail.trim());
        append_launch_log(launch_log, &format!("update-base: {failure}"));
        return Err(failure);
    }
    Ok(format!("updated `{base}` in {project_path} ({display})"))
}

/// The branch the checkout currently has checked out, `None` on a detached
/// HEAD or any git failure — callers only compare it against the base name.
fn current_branch(project_path: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(project_path)
        .args(["symbolic-ref", "--short", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

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

    fn rev(dir: &Path, what: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["rev-parse", what])
            .output()
            .unwrap();
        assert!(output.status.success(), "rev-parse {what} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare `origin` with one commit on `main`, a `checkout` clone of it,
    /// and a second commit pushed to `origin/main` from elsewhere — the exact
    /// "main moved while the task sat in review" shape.
    fn fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "voro-rebase-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let origin = root.join("origin.git");
        let seed = root.join("seed");
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);

        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        git(&seed, &["config", "user.email", "t@example.com"]);
        git(&seed, &["config", "user.name", "t"]);
        std::fs::write(seed.join("a.txt"), "one\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "one"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&seed, &["push", "-q", "origin", "main"]);

        git(
            root.as_path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );

        // main moves on origin after the clone
        std::fs::write(seed.join("a.txt"), "two\n").unwrap();
        git(&seed, &["commit", "-qam", "two"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        (root, checkout)
    }

    #[test]
    fn pulls_when_the_checkout_sits_on_the_base() {
        let (root, checkout) = fixture();
        let log = root.join("launches.log");
        let before = rev(&checkout, "main");

        let note = update_base(checkout.to_str().unwrap(), "main", &log).unwrap();
        assert!(note.contains("pull --ff-only"), "{note}");
        assert_ne!(rev(&checkout, "main"), before, "main must advance");
        assert_eq!(rev(&checkout, "main"), rev(&checkout, "HEAD"));
        assert!(
            std::fs::read_to_string(&log)
                .unwrap()
                .contains("update-base"),
            "the launch log must carry the breadcrumb"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fetches_the_ref_when_the_checkout_is_on_another_branch() {
        let (root, checkout) = fixture();
        let log = root.join("launches.log");
        git(&checkout, &["switch", "-q", "-c", "feat/x"]);
        let before = rev(&checkout, "main");
        let head_before = rev(&checkout, "HEAD");

        let note = update_base(checkout.to_str().unwrap(), "main", &log).unwrap();
        assert!(note.contains("fetch origin main:main"), "{note}");
        assert_ne!(rev(&checkout, "main"), before, "main must advance");
        assert_eq!(
            rev(&checkout, "HEAD"),
            head_before,
            "the checked-out branch must not move"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_git_failure_is_surfaced_and_logged() {
        let (root, checkout) = fixture();
        let log = root.join("launches.log");
        git(&checkout, &["remote", "remove", "origin"]);

        let err = update_base(checkout.to_str().unwrap(), "main", &log).unwrap_err();
        assert!(err.contains("failed"), "{err}");
        assert!(
            std::fs::read_to_string(&log).unwrap().contains("failed"),
            "the failure must leave a breadcrumb"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
