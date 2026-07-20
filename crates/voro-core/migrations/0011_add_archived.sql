-- Retire a project without touching its tasks (DESIGN.md §5): archiving hides
-- the project and every one of its tasks from the cockpit — queue, stats,
-- running strip — while the projects screen keeps it findable under an
-- [archived] tag. Tasks freeze in whatever state they hold, so unarchiving
-- restores the pre-archive view exactly. Additive with a default, so every
-- existing project stays active.
ALTER TABLE projects ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1));
