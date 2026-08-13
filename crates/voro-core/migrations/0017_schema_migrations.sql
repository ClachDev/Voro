-- What this database's schema is actually made of (DESIGN.md §5).
--
-- `user_version` counts migrations; it cannot say *which* ones. Two branches
-- that both author a migration 17 produce databases that are indistinguishable
-- by the counter and incompatible in fact: a binary carrying the other 17 sees
-- `version == MIGRATIONS.len()`, applies nothing, refuses nothing, and fails at
-- the first query naming a column its own schema has and this database does
-- not. That is the failure this journal exists to make impossible to miss.
--
-- The applied SQL is stored verbatim rather than hashed because a hash can say
-- that a migration differs but never how, and the code that would answer that
-- is the first thing to go missing — a worktree is disposable, a branch is
-- deleted once merged. Journalling the statements lets a stranded database
-- carry its own incident report, enough to write the inverse without the build
-- that applied it. Rows for migrations applied before this table existed carry
-- a NULL `sql` and are honestly unverifiable.
--
-- The corollary is that an applied migration is immutable: editing one — even
-- its comments — is a divergence, and will be reported as one.

CREATE TABLE schema_migrations (
  idx        INTEGER PRIMARY KEY,   -- 1-based position in the migration list
  sql        TEXT,                  -- as applied; NULL for pre-journal history
  applied_at TEXT NOT NULL,
  applied_by TEXT                   -- the build that applied it, for the report
);
