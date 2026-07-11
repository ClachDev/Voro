-- The project's review medium (DESIGN.md §8/§11a): how the unified `pr`
-- action shows a review task's diff. NULL is the auto default — GitHub when
-- the checkout is a GitHub repo, the configured viewer otherwise; 'pr' and
-- 'viewer[:name]' pin the medium explicitly. Free TEXT rather than a CHECK
-- because viewer names are user-defined in voro.toml.
ALTER TABLE projects ADD COLUMN review_action TEXT;
