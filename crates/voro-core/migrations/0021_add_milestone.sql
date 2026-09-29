-- Mark a task as a milestone (DESIGN.md §3): an outcome the operator watches
-- happen, gated on the tasks that block it. A milestone is also a human task
-- and is created parked; the flag is the only extra data it carries. Additive
-- with a default, so every existing task stays an ordinary one.
ALTER TABLE tasks ADD COLUMN milestone INTEGER NOT NULL DEFAULT 0 CHECK (milestone IN (0, 1));
