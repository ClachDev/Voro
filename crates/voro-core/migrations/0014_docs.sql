-- Document links (DESIGN.md §3/§5): a plan or design doc registered against a
-- project, and the many-to-many edge tying it to the tasks derived from it. The
-- linkage today lives only as prose inside each task body, which drifts, cannot
-- be queried, and is never handed to a dispatched agent.
CREATE TABLE docs (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  repo_id    INTEGER REFERENCES repos(id),  -- NULL = the project's default repo
  title      TEXT,                          -- optional label; NULL reads as the location
  location   TEXT NOT NULL,                 -- repo-relative path, absolute path, or URL
  created_at TEXT NOT NULL,
  UNIQUE (project_id, location)
);

-- The edge is deliberately unconstrained by project: one plan doc routinely
-- spawns work across several projects, which is the case this table exists for.
CREATE TABLE task_docs (
  task_id INTEGER NOT NULL REFERENCES tasks(id),
  doc_id  INTEGER NOT NULL REFERENCES docs(id),
  PRIMARY KEY (task_id, doc_id)
);

CREATE INDEX task_docs_by_doc ON task_docs(doc_id);
