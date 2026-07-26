-- Split the checkout out of the project (DESIGN.md §3/§5): a project is a unit
-- of attention allocation, a repo is an execution target, and one project may
-- span several repos. Deliberately non-additive — DESIGN.md §5's "additive
-- where possible" yields here, because leaving `projects.path` in place would
-- leave two sources of truth for the same checkout in a single-operator local
-- database that this migration converts in one pass.
CREATE TABLE repos (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  name       TEXT NOT NULL,
  path       TEXT NOT NULL,
  is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0,1)),
  UNIQUE (project_id, name)
);

-- At most one default per project, enforced by the schema; the remaining
-- invariants (never zero repos, no deleting a referenced or last repo) live in
-- the store API, the same place the state machine lives.
CREATE UNIQUE INDEX repos_one_default ON repos(project_id) WHERE is_default = 1;

-- Every project's old checkout reappears as its default repo, named after the
-- project, so dispatch and `pr` resolve exactly the paths they did before.
INSERT INTO repos (project_id, name, path, is_default)
  SELECT id, name, path, 1 FROM projects;

-- NULL means "the project's default repo", which is what every existing task
-- gets — preserving today's behaviour for all of them.
ALTER TABLE tasks ADD COLUMN repo_id INTEGER REFERENCES repos(id);

ALTER TABLE projects DROP COLUMN path;
