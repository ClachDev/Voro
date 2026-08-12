//! Re-review after a rejection (DESIGN.md §8): deciding how much of a task's
//! diff the operator should be shown, and pairing a rework's completion summary
//! with the feedback it answers. Pure of I/O — the `gh` and `git` calls that
//! supply the revisions live in the `voro` crate — so every decision here is
//! testable against canned strings.

use serde::Deserialize;

use crate::model::Event;

/// The event kind recording the branch revision the operator reviewed. The
/// `events` table carries it, so nothing about delta re-review needs a column.
pub const REVIEWED_EVENT: &str = "reviewed";

/// How much of a task's work to put in front of the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewDiff {
    /// Everything on the branch. A first review carries no notice; a fallback
    /// from a delta that could not be built explains itself in one.
    Full { notice: Option<String> },
    /// Only what the rework added: the revision reviewed last, and the head it
    /// is compared against.
    Since { since: String, head: String },
}

impl ReviewDiff {
    /// The explanation to show beside the opened diff, if any.
    pub fn notice(&self) -> Option<&str> {
        match self {
            ReviewDiff::Full { notice } => notice.as_deref(),
            ReviewDiff::Since { .. } => None,
        }
    }
}

/// Decide what a review should show (DESIGN.md §8). `last_reviewed` is the
/// revision recorded when the operator rejected, `head` the branch's current
/// tip, and `still_on_branch` whether that recorded revision is still reachable
/// from the head — false after a force-push or a rebase rewrote it away.
///
/// Every gap degrades to the full diff rather than erroring: a task nobody has
/// rejected yet reviews exactly as it always did (no notice, so nothing about a
/// first review changes), and a delta that cannot be built says why.
pub fn plan_review_diff(
    last_reviewed: Option<&str>,
    head: Option<&str>,
    still_on_branch: bool,
) -> ReviewDiff {
    let Some(since) = trimmed(last_reviewed) else {
        return ReviewDiff::Full { notice: None };
    };
    let Some(head) = trimmed(head) else {
        return ReviewDiff::Full {
            notice: Some(format!(
                "could not read the branch head, so the diff since {} is unavailable — \
                 showing the full diff",
                short(since)
            )),
        };
    };
    if same_revision(since, head) {
        return ReviewDiff::Full {
            notice: Some(format!(
                "nothing new since the last review ({}) — showing the full diff",
                short(since)
            )),
        };
    }
    if !still_on_branch {
        return ReviewDiff::Full {
            notice: Some(format!(
                "the revision reviewed last ({}) is no longer on the branch — \
                 showing the full diff",
                short(since)
            )),
        };
    }
    ReviewDiff::Since {
        since: since.to_string(),
        head: head.to_string(),
    }
}

fn trimmed(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|s| !s.is_empty())
}

/// Two revisions naming the same commit. Compared prefix-tolerantly and
/// case-insensitively, since an abbreviated SHA from one source (a viewer
/// template, a hand-set value) has to match a full one from another.
fn same_revision(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    short.len() >= 7
        && long
            .to_ascii_lowercase()
            .starts_with(&short.to_ascii_lowercase())
}

/// Abbreviate a revision for a human-facing notice. Counted in characters
/// rather than bytes, so a value that is not a SHA cannot split one.
fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// A tracked PR's revisions, read from `gh pr view --json headRefOid,commits`:
/// the current head, and the commits the PR contains. The latter is what
/// answers "is the revision I reviewed still on this branch?" — a rework that
/// appends commits keeps it, and a force-push or rebase drops it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrRevisions {
    pub head: Option<String>,
    pub commits: Vec<String>,
}

impl PrRevisions {
    /// Whether `sha` is one of the PR's commits.
    pub fn contains(&self, sha: &str) -> bool {
        self.commits.iter().any(|c| same_revision(c, sha))
    }
}

/// Read a PR's head and commit list out of `gh pr view --json
/// headRefOid,commits` output. Malformed or missing fields degrade to empty —
/// no head and no commits — which [`plan_review_diff`] turns into the full
/// diff, so an unreachable `gh` never blocks a review.
pub fn parse_pr_revisions(json: &str) -> PrRevisions {
    #[derive(Deserialize)]
    struct Commit {
        #[serde(default)]
        oid: String,
    }
    #[derive(Deserialize)]
    struct View {
        #[serde(default)]
        #[serde(rename = "headRefOid")]
        head_ref_oid: String,
        #[serde(default)]
        commits: Vec<Commit>,
    }
    match serde_json::from_str::<View>(json) {
        Ok(view) => PrRevisions {
            head: Some(view.head_ref_oid)
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty()),
            commits: view
                .commits
                .into_iter()
                .map(|c| c.oid.trim().to_string())
                .filter(|o| !o.is_empty())
                .collect(),
        },
        Err(_) => PrRevisions::default(),
    }
}

/// A rework cycle's report: the rejection feedback the operator gave, and the
/// completion summary the agent came back with (DESIGN.md §8). The summary is
/// `None` while the rework is still in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReworkReport {
    pub feedback: String,
    pub summary: Option<String>,
}

/// Pair a task's newest rejection feedback with the summary that answers it,
/// read off its event log. `None` for a task no one has rejected — a first
/// review has nothing to answer, so nothing is rendered against it. A summary
/// logged *before* the feedback belongs to the cycle that was rejected, so only
/// a later one counts as the response.
pub fn rework_report(events: &[Event]) -> Option<ReworkReport> {
    let last_feedback = events
        .iter()
        .rev()
        .find(|e| e.kind == "feedback" && detail(e).is_some())?;
    let summary = events
        .iter()
        .rev()
        .take_while(|e| e.id != last_feedback.id)
        .find(|e| e.kind == "summary")
        .and_then(detail)
        .map(str::to_string);
    Some(ReworkReport {
        feedback: detail(last_feedback)?.to_string(),
        summary,
    })
}

fn detail(event: &Event) -> Option<&str> {
    event
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_first_review_is_the_full_diff_and_says_nothing() {
        assert_eq!(
            plan_review_diff(None, Some(B), true),
            ReviewDiff::Full { notice: None }
        );
        assert_eq!(
            plan_review_diff(Some("   "), Some(B), true),
            ReviewDiff::Full { notice: None }
        );
    }

    #[test]
    fn a_rework_reviews_only_what_it_added() {
        assert_eq!(
            plan_review_diff(Some(A), Some(B), true),
            ReviewDiff::Since {
                since: A.into(),
                head: B.into(),
            }
        );
        assert!(plan_review_diff(Some(A), Some(B), true).notice().is_none());
    }

    #[test]
    fn a_head_that_never_moved_falls_back_with_a_notice() {
        let plan = plan_review_diff(Some(A), Some(A), true);
        assert!(matches!(plan, ReviewDiff::Full { .. }));
        assert!(plan.notice().unwrap().contains("nothing new"), "{plan:?}");
        // an abbreviated head naming the same commit reads the same way
        let plan = plan_review_diff(Some(A), Some(&A[..8]), true);
        assert!(plan.notice().unwrap().contains("nothing new"), "{plan:?}");
    }

    /// The force-push case the acceptance names: the recorded revision is gone,
    /// so the range cannot be built — degrade to the whole diff, never error.
    #[test]
    fn a_rewritten_history_falls_back_with_a_notice() {
        let plan = plan_review_diff(Some(A), Some(B), false);
        assert!(matches!(plan, ReviewDiff::Full { .. }));
        let notice = plan.notice().unwrap();
        assert!(notice.contains("no longer on the branch"), "{notice}");
        assert!(notice.contains("aaaaaaa"), "{notice}");
        assert!(!notice.contains(A), "the notice abbreviates: {notice}");
    }

    /// An unreachable `gh`/`git` leaves no head at all, which is a fallback
    /// rather than a failure.
    #[test]
    fn an_unreadable_head_falls_back_with_a_notice() {
        let plan = plan_review_diff(Some(A), None, true);
        assert!(
            plan.notice().unwrap().contains("could not read"),
            "{plan:?}"
        );
    }

    #[test]
    fn pr_revisions_carry_the_head_and_the_commits() {
        let json = r#"{"headRefOid":"bbbb","commits":[{"oid":"aaaaaaaaaa"},{"oid":"bbbb"}]}"#;
        let revs = parse_pr_revisions(json);
        assert_eq!(revs.head.as_deref(), Some("bbbb"));
        assert_eq!(revs.commits.len(), 2);
        assert!(revs.contains("aaaaaaaaaa"));
        // prefix-tolerant, so a recorded full SHA matches an abbreviated one
        assert!(revs.contains("aaaaaaa"));
        assert!(!revs.contains("cccccccccc"));
    }

    #[test]
    fn unusable_gh_output_reads_as_no_revisions() {
        for raw in ["not json", "", "{}", r#"{"headRefOid":""}"#] {
            let revs = parse_pr_revisions(raw);
            assert!(revs.head.is_none(), "{raw}");
            assert!(revs.commits.is_empty(), "{raw}");
            assert!(!revs.contains(A), "{raw}");
        }
    }

    fn event(id: i64, kind: &str, detail: &str) -> Event {
        Event {
            id,
            task_id: Some(1),
            at: "2026-08-12 00:00:00".into(),
            kind: kind.into(),
            detail: Some(detail.into()),
        }
    }

    #[test]
    fn a_task_nobody_rejected_has_no_rework_report() {
        let events = vec![event(1, "summary", "did the thing")];
        assert!(rework_report(&events).is_none());
    }

    #[test]
    fn the_report_pairs_the_newest_feedback_with_the_answer_to_it() {
        let events = vec![
            event(1, "summary", "first attempt"),
            event(2, "feedback", "tests missing"),
            event(3, "summary", "1. tests missing — added them"),
        ];
        let report = rework_report(&events).unwrap();
        assert_eq!(report.feedback, "tests missing");
        assert_eq!(
            report.summary.as_deref(),
            Some("1. tests missing — added them")
        );
    }

    #[test]
    fn a_rework_still_in_flight_reports_no_summary() {
        let events = vec![
            event(1, "summary", "first attempt"),
            event(2, "feedback", "tests missing"),
        ];
        let report = rework_report(&events).unwrap();
        assert_eq!(report.feedback, "tests missing");
        assert!(report.summary.is_none());
    }

    /// A second rejection supersedes the first: the report answers the newest
    /// feedback, and the summary that answered the previous round is not it.
    #[test]
    fn a_second_rejection_supersedes_the_first() {
        let events = vec![
            event(1, "feedback", "tests missing"),
            event(2, "summary", "added tests"),
            event(3, "feedback", "and the docs"),
        ];
        let report = rework_report(&events).unwrap();
        assert_eq!(report.feedback, "and the docs");
        assert!(report.summary.is_none());
    }
}
