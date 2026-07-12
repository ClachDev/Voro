//! Tracking a GitHub PR on a task (DESIGN.md §11c): parsing a PR reference
//! into the pieces a `gh` call needs, and turning a PR's review comments into
//! a reject-with-feedback body. Pure of I/O — the `gh` shell-out lives in the
//! `voro` crate, same as issue import — so everything here is testable against
//! canned strings.

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::{Task, TaskState};

/// A parsed reference to a GitHub pull request. The base repo is recorded
/// explicitly (`owner`/`repo`/`host`) rather than inferred from a checkout's
/// `origin`, so a PR opened from a fork is still addressed against the repo
/// where the diff and its review comments live (DESIGN.md §11c).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    /// Canonical `https://{host}/{owner}/{repo}/pull/{number}` URL.
    pub url: String,
    pub host: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    /// Parse a PR reference from a URL (`https://github.com/o/r/pull/12`, with
    /// or without scheme, extra path segments, or a query/fragment) or the
    /// `owner/repo#number` shorthand. Enterprise hosts are preserved. The URL
    /// is re-emitted in canonical form so tracking is idempotent regardless of
    /// which form the operator pasted.
    pub fn parse(input: &str) -> Result<PrRef> {
        let s = input.trim();
        if s.is_empty() {
            return Err(Error::Invalid("a PR reference is required".into()));
        }

        // `owner/repo#number` shorthand: no scheme, exactly one slash.
        if let Some((repo_part, num_part)) = s.split_once('#')
            && !repo_part.contains("://")
            && repo_part.matches('/').count() == 1
        {
            let (owner, repo) = repo_part.split_once('/').unwrap();
            let number = parse_number(num_part)?;
            return PrRef::build("github.com", owner, repo, number);
        }

        // URL form. Drop the scheme and any query/fragment, then walk the path
        // segments looking for `.../pull/<n>`.
        let no_scheme = s
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(s)
            .trim_start_matches('/');
        let path = no_scheme
            .split(['?', '#'])
            .next()
            .unwrap_or(no_scheme)
            .trim_end_matches('/');
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

        // [host, owner, repo, ("pull"|"pulls"), number, ...]
        if let Some(pos) = segments.iter().position(|s| *s == "pull" || *s == "pulls")
            && pos >= 3
            && let Some(num) = segments.get(pos + 1)
        {
            let host = segments[pos - 3];
            let owner = segments[pos - 2];
            let repo = segments[pos - 1];
            let number = parse_number(num)?;
            return PrRef::build(host, owner, repo, number);
        }

        Err(Error::Invalid(format!(
            "'{input}' is not a GitHub PR reference — expected a URL like \
             https://github.com/owner/repo/pull/12 or the shorthand owner/repo#12"
        )))
    }

    fn build(host: &str, owner: &str, repo: &str, number: u64) -> Result<PrRef> {
        if host.is_empty() || owner.is_empty() || repo.is_empty() {
            return Err(Error::Invalid(
                "a PR reference needs a host, owner, and repo".into(),
            ));
        }
        Ok(PrRef {
            url: format!("https://{host}/{owner}/{repo}/pull/{number}"),
            host: host.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        })
    }

    /// The `owner/repo` slug a `gh -R`/`gh api repos/...` call expects.
    pub fn nwo(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }

    /// The API path segment for this PR's REST resources, e.g.
    /// `repos/owner/repo/pulls/12`.
    pub fn api_path(&self, resource: &str) -> String {
        format!("repos/{}/pulls/{}/{resource}", self.nwo(), self.number)
    }
}

fn parse_number(raw: &str) -> Result<u64> {
    raw.trim()
        .parse()
        .map_err(|_| Error::Invalid(format!("'{raw}' is not a PR number")))
}

#[derive(Debug, Clone, Deserialize)]
struct GhUser {
    #[serde(default)]
    login: String,
}

/// One pull-request review — the summary a reviewer submits alongside (or
/// instead of) inline comments (`gh api repos/o/r/pulls/N/reviews`). Only the
/// fields the feedback body uses are named; anything else gh emits is ignored.
#[derive(Debug, Clone, Deserialize)]
struct PrReview {
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    user: Option<GhUser>,
}

/// One inline review comment on the diff (`gh api repos/o/r/pulls/N/comments`).
#[derive(Debug, Clone, Deserialize)]
struct PrReviewComment {
    #[serde(default)]
    body: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<i64>,
    #[serde(default)]
    user: Option<GhUser>,
}

fn login(user: &Option<GhUser>) -> &str {
    user.as_ref()
        .map(|u| u.login.as_str())
        .filter(|l| !l.is_empty())
        .unwrap_or("unknown")
}

/// Build a reject-with-feedback body from a PR's reviews and inline comments
/// (DESIGN.md §11c), so a GitHub review reaches the agent without retyping.
/// Reviews with an empty body (a bare approval, or one that carries only inline
/// comments) are skipped; the inline comments carry their own text. Returns an
/// empty string when there is nothing to relay, which the caller turns into a
/// "no review comments" message rather than an empty rejection.
pub fn format_review_feedback(
    pr: &PrRef,
    reviews_json: &str,
    comments_json: &str,
) -> Result<String> {
    let reviews: Vec<PrReview> = serde_json::from_str(reviews_json)
        .map_err(|e| Error::Invalid(format!("invalid PR reviews JSON: {e}")))?;
    let comments: Vec<PrReviewComment> = serde_json::from_str(comments_json)
        .map_err(|e| Error::Invalid(format!("invalid PR comments JSON: {e}")))?;

    let mut sections: Vec<String> = Vec::new();
    for r in &reviews {
        if r.body.trim().is_empty() {
            continue;
        }
        let state = if r.state.is_empty() {
            String::new()
        } else {
            format!(" ({})", r.state.to_lowercase())
        };
        sections.push(format!("@{}{state}: {}", login(&r.user), r.body.trim()));
    }
    for c in &comments {
        if c.body.trim().is_empty() {
            continue;
        }
        let loc = match (&c.path, c.line) {
            (Some(path), Some(line)) => format!("`{path}:{line}` — "),
            (Some(path), None) => format!("`{path}` — "),
            _ => String::new(),
        };
        sections.push(format!("{loc}@{}: {}", login(&c.user), c.body.trim()));
    }

    if sections.is_empty() {
        return Ok(String::new());
    }
    let mut body = format!("Review feedback from {}\n", pr.url);
    for section in sections {
        body.push('\n');
        body.push_str(&section);
        body.push('\n');
    }
    Ok(body)
}

/// Everything the forge needs to open a PR for a review task (DESIGN.md §8):
/// the branch to push and the title and body of the pull request. Assembled by
/// [`plan_pr`] once the task is proven PR-ready, so the process-facing create
/// routine in the `voro` crate never has to re-derive or re-validate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrPlan {
    pub branch: String,
    pub title: String,
    pub body: String,
}

/// Validate that a task can have a PR opened from its done-time state, and if
/// so assemble the [`PrPlan`] (DESIGN.md §8). A PR is created from a `review`
/// task that carries both a branch (the work to push) and a completion summary
/// (the PR body, kept as a `summary` event — the caller supplies the latest).
/// Each gap fails naming exactly what is missing — state, branch, or summary —
/// so `pr` can tell the operator what to fix. Pure of I/O, so the forge seam in
/// the `voro` crate is the only part that touches git or `gh`.
pub fn plan_pr(task: &Task, latest_summary: Option<&str>) -> Result<PrPlan> {
    if task.state != TaskState::Review {
        return Err(Error::Invalid(format!(
            "only a review task can have a PR opened from its summary; task {} is {}",
            task.id, task.state
        )));
    }
    let branch = task
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "task {} has no branch to push — record one with `voro done --branch` or \
                 `voro set --branch`",
                task.id
            ))
        })?;
    let body = latest_summary
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::Invalid(format!(
                "task {} has no completion summary for the PR body — record one with \
                 `voro done --summary`",
                task.id
            ))
        })?;
    Ok(PrPlan {
        branch: branch.to_string(),
        title: task.title.clone(),
        body: body.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_url() {
        let pr = PrRef::parse("https://github.com/acme/widget/pull/42").unwrap();
        assert_eq!(pr.host, "github.com");
        assert_eq!(pr.owner, "acme");
        assert_eq!(pr.repo, "widget");
        assert_eq!(pr.number, 42);
        assert_eq!(pr.nwo(), "acme/widget");
        assert_eq!(pr.url, "https://github.com/acme/widget/pull/42");
        assert_eq!(
            pr.api_path("comments"),
            "repos/acme/widget/pulls/42/comments"
        );
    }

    #[test]
    fn parses_url_without_scheme_and_with_extra_segments() {
        let pr = PrRef::parse("github.com/acme/widget/pull/42/files?w=1#discussion").unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://github.com/acme/widget/pull/42");
    }

    #[test]
    fn parses_the_owner_repo_shorthand() {
        let pr = PrRef::parse("acme/widget#7").unwrap();
        assert_eq!(pr.owner, "acme");
        assert_eq!(pr.repo, "widget");
        assert_eq!(pr.number, 7);
        assert_eq!(pr.url, "https://github.com/acme/widget/pull/7");
    }

    #[test]
    fn preserves_an_enterprise_host() {
        let pr = PrRef::parse("https://git.example.com/acme/widget/pull/3").unwrap();
        assert_eq!(pr.host, "git.example.com");
        assert_eq!(pr.url, "https://git.example.com/acme/widget/pull/3");
    }

    #[test]
    fn rejects_non_pr_references() {
        assert!(PrRef::parse("https://github.com/acme/widget/issues/42").is_err());
        assert!(PrRef::parse("https://github.com/acme/widget").is_err());
        assert!(PrRef::parse("not a url").is_err());
        assert!(PrRef::parse("acme/widget/extra#1").is_err());
        assert!(PrRef::parse("").is_err());
        assert!(PrRef::parse("https://github.com/acme/widget/pull/notanumber").is_err());
    }

    fn pr() -> PrRef {
        PrRef::parse("https://github.com/acme/widget/pull/42").unwrap()
    }

    #[test]
    fn formats_reviews_and_inline_comments() {
        let reviews = r#"[
            {"user": {"login": "alice"}, "state": "CHANGES_REQUESTED", "body": "Please fix the parser"},
            {"user": {"login": "bob"}, "state": "APPROVED", "body": ""}
        ]"#;
        let comments = r#"[
            {"user": {"login": "alice"}, "path": "src/lib.rs", "line": 12, "body": "off-by-one here"}
        ]"#;
        let body = format_review_feedback(&pr(), reviews, comments).unwrap();
        assert!(body.contains("Review feedback from https://github.com/acme/widget/pull/42"));
        assert!(body.contains("@alice (changes_requested): Please fix the parser"));
        // the bare approval with no body is skipped
        assert!(!body.contains("@bob"));
        assert!(body.contains("`src/lib.rs:12` — @alice: off-by-one here"));
    }

    #[test]
    fn empty_when_there_is_nothing_to_relay() {
        let body = format_review_feedback(&pr(), "[]", "[]").unwrap();
        assert!(body.is_empty());
        // a lone approval with no comments is also nothing to relay
        let reviews = r#"[{"user": {"login": "bob"}, "state": "APPROVED", "body": ""}]"#;
        assert!(
            format_review_feedback(&pr(), reviews, "[]")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        // a comment with no path/line/user still relays its body
        let comments = r#"[{"body": "a general note"}]"#;
        let body = format_review_feedback(&pr(), "[]", comments).unwrap();
        assert!(body.contains("@unknown: a general note"));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(format_review_feedback(&pr(), "not json", "[]").is_err());
        assert!(format_review_feedback(&pr(), "[]", "not json").is_err());
    }

    // --- plan_pr (DESIGN.md §8: opening a PR from a review task's summary) ---

    use crate::model::Priority;

    fn task(state: TaskState, branch: Option<&str>) -> Task {
        Task {
            id: 82,
            project_id: 1,
            title: "Extend pr to create the PR".into(),
            body: String::new(),
            priority: Priority::P1,
            state,
            agent: None,
            human: false,
            question: None,
            pr_url: None,
            branch: branch.map(str::to_string),
            state_since: "2026-07-10 00:00:00".into(),
            created_at: "2026-07-10 00:00:00".into(),
            closed_at: None,
        }
    }

    #[test]
    fn plans_a_pr_from_a_review_task_with_branch_and_summary() {
        let plan = plan_pr(
            &task(TaskState::Review, Some("feat/pr")),
            Some("Did the thing"),
        )
        .unwrap();
        assert_eq!(plan.branch, "feat/pr");
        assert_eq!(plan.title, "Extend pr to create the PR");
        assert_eq!(plan.body, "Did the thing");
    }

    #[test]
    fn plan_requires_the_review_state() {
        for state in [
            TaskState::Ready,
            TaskState::Running,
            TaskState::NeedsInput,
            TaskState::Done,
        ] {
            let err = plan_pr(&task(state, Some("feat/pr")), Some("summary"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("review"), "{state}: {err}");
        }
    }

    #[test]
    fn plan_names_a_missing_branch() {
        let err = plan_pr(&task(TaskState::Review, None), Some("summary"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("branch"), "{err}");
        // a blank branch is treated as absent
        let err = plan_pr(&task(TaskState::Review, Some("   ")), Some("summary"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("branch"), "{err}");
    }

    #[test]
    fn plan_names_a_missing_summary() {
        let err = plan_pr(&task(TaskState::Review, Some("feat/pr")), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("summary"), "{err}");
        let err = plan_pr(&task(TaskState::Review, Some("feat/pr")), Some("  "))
            .unwrap_err()
            .to_string();
        assert!(err.contains("summary"), "{err}");
    }
}
