# CLAUDE.md

Focus is a local, single-operator command centre that prioritises tasks across many
projects and dispatches them to coding agents. **Read `docs/DESIGN.md` before any
non-trivial work** — it is authoritative for concepts (§3), the SQLite schema (§5),
the task state machine (§6), scoring (§7), and dispatch semantics (§8). If your
implementation needs to deviate from it, update the design doc in the same change
and say so explicitly.

## Architecture rules

- Cargo workspace with two crates: `focus-core` (store, state machine, scheduler,
  scoring — pure logic, no terminal I/O) and `focus` (the ratatui TUI). Business
  logic never lives in the TUI crate; the TUI renders and forwards intents.
- Task state changes go through the `focus-core` transition API, which enforces
  the state machine in DESIGN.md §6. Never mutate `tasks.state` with raw SQL.
- Schema changes are numbered migrations, additive where possible. The `events`
  table is append-only.
- The scoring function stays small. Any change to it must update the score
  decomposition view and DESIGN.md §7 in the same change.
- Prefer boring dependencies: rusqlite, ratatui, crossterm, serde, thiserror.
  Justify anything beyond that in the PR description.

## Commands

- Build: `cargo build --workspace`
- Test: `cargo test --workspace` — must pass before any commit
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Format: `cargo fmt --all`

`focus-core` requires tests; state-machine transitions and scheduler ordering are
the highest-value targets. TUI code is tested where practical, not dogmatically.

## Git conventions

- Feature branches, squash-merged to `main`. One logical change per PR.
- Imperative commit subjects ("Add ready-work query"), body explains *why*.
- Never commit directly to `main`; never force-push shared branches.

## Working style

- When a genuine design decision surfaces mid-task (ambiguity the design doc
  doesn't resolve), stop and ask rather than choosing silently.
- When you finish a task and notice follow-up work, propose it as a distinct
  task rather than expanding scope — this repo eats its own dog food.
- Documentation is written in prose, not bullet lists, except for reference
  material like this file.
