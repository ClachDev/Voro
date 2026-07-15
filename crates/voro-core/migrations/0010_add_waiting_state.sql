-- A 'waiting' state (DESIGN.md §6): in-flight work handed off to an external
-- party — a PR awaiting someone else's review or merge — that asks nothing of
-- the operator. It sits out of the queue like 'parked', reached only from
-- 'review' via HandOff. SQLite cannot alter a CHECK constraint, so the table is
-- rebuilt, as in 0008; the runner disables foreign_keys around migrations, and
-- deps/sessions/events keep their task ids. Purely additive — no rows change.
CREATE TABLE tasks_new (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  title      TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',   -- markdown; written as a dispatchable prompt
  priority   INTEGER NOT NULL DEFAULT 3 CHECK (priority BETWEEN 0 AND 3),  -- P0..P3
  state      TEXT NOT NULL DEFAULT 'proposed'
             CHECK (state IN ('proposed','parked','ready','running',
                              'needs-input','review','waiting','stalled',
                              'done','rejected')),
  agent      TEXT,                        -- optional override; NULL = resolve at dispatch
  question   TEXT,                        -- set iff state = 'needs-input'
  pr_url     TEXT,                        -- optional tracked GitHub PR (DESIGN.md §11c)
  branch     TEXT,                        -- optional git branch (task #81)
  state_since TEXT NOT NULL,              -- ISO timestamp; drives the age bonus
  created_at  TEXT NOT NULL,
  closed_at   TEXT,
  human      INTEGER NOT NULL DEFAULT 0 CHECK (human IN (0, 1))  -- human-only (0007)
);

INSERT INTO tasks_new (id, project_id, title, body, priority, state, agent,
                       question, pr_url, branch, human, state_since, created_at, closed_at)
SELECT id, project_id, title, body, priority, state, agent,
       question, pr_url, branch, human, state_since, created_at, closed_at
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX idx_tasks_project ON tasks(project_id);
CREATE INDEX idx_tasks_state ON tasks(state);
