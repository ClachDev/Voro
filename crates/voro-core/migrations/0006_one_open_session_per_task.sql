-- One open session per task (DESIGN.md §8). The session lifecycle now follows
-- the task, not the agent's listing: a session is opened at dispatch and closed
-- by the terminal transition (or superseded by a continuation), so a task must
-- never carry two open session rows at once. Older code left predecessors open
-- on continuation and redispatch, which is exactly how the running strip showed
-- a task twice.
--
-- Before the invariant can be enforced, dedupe any task that already has more
-- than one open session: keep the newest open row and close the rest, stamping
-- them `aborted` (they were superseded). A single lingering open session on a
-- closed task is left for reconciliation to finalise.
UPDATE sessions
SET ended_at = datetime('now'), outcome = 'aborted'
WHERE ended_at IS NULL
  AND id NOT IN (
    SELECT MAX(id) FROM sessions WHERE ended_at IS NULL GROUP BY task_id
  );

-- The invariant as a schema backstop: at most one open (unended) session per
-- task. Every open-session path closes its predecessor in the same transaction,
-- and this index makes a second open row structurally impossible.
CREATE UNIQUE INDEX idx_one_open_session_per_task
  ON sessions(task_id) WHERE ended_at IS NULL;
