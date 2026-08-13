-- The migrations this database is made of (DESIGN.md §5).
--
-- `user_version` counts migrations without identifying them, so two databases
-- carrying different migrations at the same index are indistinguishable by the
-- counter: a binary holding the other one applies nothing, refuses nothing, and
-- fails at the first query naming a column the schema does not have. Each row
-- here holds an applied migration's SQL verbatim, checked on every open against
-- the migrations the running binary carries.
--
-- The stored text is enough to read a stranded database's history back and
-- reverse it without the build that migrated it. Rows for migrations applied
-- before this table existed carry a NULL `sql` and cannot be verified.
--
-- An applied migration is immutable: editing one, comments included, reads as
-- a divergence.

CREATE TABLE schema_migrations (
  idx        INTEGER PRIMARY KEY,   -- 1-based position in the migration list
  sql        TEXT,                  -- as applied; NULL for pre-journal history
  applied_at TEXT NOT NULL,
  applied_by TEXT                   -- the build that applied it
);
