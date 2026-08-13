-- Which liveness source is authoritative for a session (DESIGN.md §8, task
-- #387), recorded at launch by the code that spawned the process instead of
-- inferred by reconciliation from the presence of a session ref. 'pid' means
-- the recorded pid is the work itself (a foreground child, or an agent with no
-- 'sessions' verb); 'listing' means the launch handed the work to a supervisor,
-- so only the agent's own listing can answer. Purely additive.
--
-- Sessions open across the upgrade default to 'listing', which is what every
-- dispatch of an agent with a 'sessions' verb already was. A pre-migration
-- interactive refine round is therefore left alone rather than pid-checked —
-- unprobeable in the direction that never finalises a live session wrongly, and
-- the operator's next transition closes it.
ALTER TABLE sessions ADD COLUMN liveness_source TEXT NOT NULL DEFAULT 'listing'
  CHECK (liveness_source IN ('pid','listing'));
