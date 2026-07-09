-- Optionally track a GitHub PR on a task (DESIGN.md §11c). Additive: a single
-- nullable column holding the canonical PR URL. The URL names the PR's *base*
-- repo, not the checkout's origin, so a PR opened from a fork is still tracked
-- against the repo where the diff and its review comments actually live; the
-- owner/repo/number a `gh` call needs are parsed back out of it.
ALTER TABLE tasks ADD COLUMN pr_url TEXT;
