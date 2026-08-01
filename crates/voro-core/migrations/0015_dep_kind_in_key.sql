-- One pair of tasks may carry edges of more than one kind (DESIGN.md §5): a
-- task proposed mid-session is `discovered-from` its parent and is often also
-- gated on it. The primary key was the pair alone, so the second edge collided
-- with the first and was dropped. `kind` joins the key.
CREATE TABLE deps_new (
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  depends_on INTEGER NOT NULL REFERENCES tasks(id),
  kind       TEXT NOT NULL DEFAULT 'blocks'
             CHECK (kind IN ('blocks','discovered-from','parent','related')),
  PRIMARY KEY (task_id, depends_on, kind)
);

INSERT INTO deps_new (task_id, depends_on, kind)
  SELECT task_id, depends_on, kind FROM deps;

DROP TABLE deps;
ALTER TABLE deps_new RENAME TO deps;

CREATE INDEX idx_deps_depends_on ON deps(depends_on);
