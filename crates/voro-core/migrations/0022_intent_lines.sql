-- A project's intent (DESIGN.md §3/§5): ordered lines, each a short label and
-- one sentence, stating why the project exists and what it is. Order is
-- precedence. An agent's proposed line waits, unaccepted, for the operator.
-- Purely additive: a project with no rows behaves exactly as before.
CREATE TABLE intent_lines (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  label      TEXT NOT NULL,
  sentence   TEXT NOT NULL,
  position   INTEGER NOT NULL,
  accepted   INTEGER NOT NULL DEFAULT 0 CHECK (accepted IN (0, 1)),
  created_at TEXT NOT NULL,
  UNIQUE (project_id, label)
);
