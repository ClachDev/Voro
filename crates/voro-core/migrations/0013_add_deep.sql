-- Mark a task as warranting the strongest model the agent offers rather than
-- its workhorse (DESIGN.md §8): dispatch resolves `{model}` in the agent's
-- command template to `model_deep` instead of `model`. It is orthogonal to
-- priority, which orders the queue, and to the agent override, which picks
-- *which* agent runs — deep only says how hard it runs. Additive with a
-- default, so every existing task keeps dispatching on the workhorse.
ALTER TABLE tasks ADD COLUMN deep INTEGER NOT NULL DEFAULT 0 CHECK (deep IN (0, 1));
