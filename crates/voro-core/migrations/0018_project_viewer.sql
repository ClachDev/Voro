-- 0018: collapse the review action to what it names — a viewer.
--
-- The review keys are static (§8): `pr` is always GitHub, `open` always a local
-- viewer. All the setting still decides is which [viewers.<name>] table a
-- project's local diffs open in, so the column is that name: 'viewer:<name>'
-- keeps <name>, while 'auto', 'pr', and bare 'viewer' all meant "name no
-- viewer, use the default", which is NULL.

UPDATE projects SET review_action = NULL
WHERE review_action IN ('auto', 'pr', 'viewer');

UPDATE projects SET review_action = substr(review_action, length('viewer:') + 1)
WHERE review_action LIKE 'viewer:%';

ALTER TABLE projects RENAME COLUMN review_action TO viewer;
