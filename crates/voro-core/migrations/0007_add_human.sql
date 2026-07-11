-- Mark a task as executable only by the human (DESIGN.md §3/§6): Mote-style
-- hands-on work no agent can perform at all. Additive with a default, so every
-- existing task stays dispatchable. This is a flag, not an executor enum —
-- every task is already human-executable via the by-hand `start`, so the only
-- fact worth storing is "no agent can do this".
ALTER TABLE tasks ADD COLUMN human INTEGER NOT NULL DEFAULT 0 CHECK (human IN (0, 1));
