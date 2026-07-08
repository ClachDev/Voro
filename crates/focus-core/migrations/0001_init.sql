CREATE TABLE projects (
  id      INTEGER PRIMARY KEY,
  name    TEXT NOT NULL UNIQUE,
  path    TEXT NOT NULL,            -- checkout location; third-party repos welcome
  weight  INTEGER NOT NULL DEFAULT 3 CHECK (weight BETWEEN 0 AND 5)  -- 0 = parked/hidden
);

CREATE TABLE tasks (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  title      TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',   -- markdown; written as a dispatchable prompt
  priority   INTEGER NOT NULL DEFAULT 3 CHECK (priority BETWEEN 0 AND 3),  -- P0..P3
  state      TEXT NOT NULL DEFAULT 'proposed'
             CHECK (state IN ('proposed','backlog','ready','running',
                              'needs-input','review','done','rejected')),
  agent      TEXT,                        -- optional override; NULL = resolve at dispatch
  question   TEXT,                        -- set iff state = 'needs-input'
  state_since TEXT NOT NULL,              -- ISO timestamp; drives the age bonus
  created_at  TEXT NOT NULL,
  closed_at   TEXT
);

CREATE TABLE deps (              -- taxonomy borrowed from beads
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  depends_on INTEGER NOT NULL REFERENCES tasks(id),
  kind       TEXT NOT NULL DEFAULT 'blocks'
             CHECK (kind IN ('blocks','discovered-from','parent','related')),
  PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE sessions (
  id        INTEGER PRIMARY KEY,
  task_id   INTEGER NOT NULL REFERENCES tasks(id),
  agent     TEXT NOT NULL,               -- the agent actually used
  pid       INTEGER,
  log_path  TEXT,
  started_at TEXT NOT NULL,
  ended_at   TEXT,
  outcome    TEXT                        -- 'completed','asked','failed','capped','aborted'
);

CREATE TABLE events (            -- append-only audit of state transitions & answers
  id        INTEGER PRIMARY KEY,
  task_id   INTEGER REFERENCES tasks(id),
  at        TEXT NOT NULL,
  kind      TEXT NOT NULL,
  detail    TEXT
);

CREATE INDEX idx_tasks_project ON tasks(project_id);
CREATE INDEX idx_tasks_state ON tasks(state);
CREATE INDEX idx_deps_depends_on ON deps(depends_on);
CREATE INDEX idx_events_task ON events(task_id);
