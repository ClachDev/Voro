-- Facts the store keeps about itself, one row per fact (DESIGN.md §5).
--
-- `protected` marks the operator's store, written on any open at the
-- production path. It lives in the file rather than being inferred from the
-- path on each open, so the property travels with the data through a symlink,
-- a moved data directory, or a restored copy.
--
-- A protected store never migrates as a side effect of being opened: the TUI
-- asks at launch, the CLI refuses and names `voro migrate`.

CREATE TABLE store_meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
