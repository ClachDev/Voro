-- The state 'backlog' becomes 'parked' (its park/unpark transitions already
-- said so). SQLite cannot alter a CHECK constraint, so the table is rebuilt.
-- The runner disables foreign_keys around migrations, per the documented
-- procedure for schema changes; deps/sessions/events rows keep their task ids.
CREATE TABLE tasks_new (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  title      TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',   -- markdown; written as a dispatchable prompt
  priority   INTEGER NOT NULL DEFAULT 3 CHECK (priority BETWEEN 0 AND 3),  -- P0..P3
  state      TEXT NOT NULL DEFAULT 'proposed'
             CHECK (state IN ('proposed','parked','ready','running',
                              'needs-input','review','done','rejected')),
  agent      TEXT,                        -- optional override; NULL = resolve at dispatch
  question   TEXT,                        -- set iff state = 'needs-input'
  state_since TEXT NOT NULL,              -- ISO timestamp; drives the age bonus
  created_at  TEXT NOT NULL,
  closed_at   TEXT
);

INSERT INTO tasks_new (id, project_id, title, body, priority, state, agent,
                       question, state_since, created_at, closed_at)
SELECT id, project_id, title, body, priority,
       CASE state WHEN 'backlog' THEN 'parked' ELSE state END,
       agent, question, state_since, created_at, closed_at
FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE INDEX idx_tasks_project ON tasks(project_id);
CREATE INDEX idx_tasks_state ON tasks(state);
