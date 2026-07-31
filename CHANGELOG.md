# Changelog

All notable changes to Voro are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->

## [Unreleased](https://github.com/ClachDev/Voro/compare/v0.1.0...HEAD) - ReleaseDate

### Added

- **Document links**: register the plan or design doc a body of work derives
  from with `voro doc add <project> <path-or-url>`, and link it to the tasks it
  spawned (`voro doc link/unlink`, or `--doc` on `voro add`/`voro set`). `voro
  list --doc <doc>` answers "which tasks came from this plan?", `voro doc show`
  rolls up every linked task's state, and dispatch names each linked document
  at its resolved location in the agent's prompt, so the agent reads the plan
  instead of rediscovering it from hints in the task body. A task in any project
  may cite a document, since one plan routinely spawns work across several. An
  absolute path inside one of the project's checkouts is stored relative to it,
  so the link survives the checkout moving.
- Multi-repo projects: a project now owns one or more **repos**, and a task may
  name which one it runs in. Manage them with `voro repo add/list/path/default/
  remove`, pick one per task with `voro add --repo NAME` and `voro set --repo
  NAME`, and import from one with `voro import --repo NAME`. A project allocates
  attention; its repos locate checkouts.
- The **deep** flag: mark a task with `voro add --deep`, `voro set <id> --deep`,
  or `!` in the TUI and it dispatches on the strongest model its agent offers
  rather than the workhorse. Agent templates gained a `{model}` placeholder and
  a per-agent `model`/`model_deep`/`model_plan` map to fill it; an agent naming
  no models, such as the built-in `codex`, ignores the flag entirely.

### Changed

- `projects.path` is gone. Existing databases convert in place: every project's
  old path becomes its default repo, and existing tasks resolve to exactly the
  checkouts they had before. `voro project add` and `voro project path` keep
  working, now on the default repo.
- `voro import`'s `--repo` flag now names a Voro repo, matching `--repo`
  everywhere else in the CLI. The GitHub-side override it used to be is now
  `--gh-repo owner/name`.

## [0.1.0](https://github.com/ClachDev/Voro/compare/27b1105...v0.1.0) - 2026-07-16

Initial release of Voro, a local, single-operator command centre that
prioritises tasks across many projects and dispatches them to coding agents.

### Added

- The next-action queue: a single cross-project ranking of the work that needs
  a human first, then the highest-scoring ready tasks.
- `voro-core`: the SQLite store, task state machine, scheduler, and scoring.
- The ratatui TUI cockpit for triaging the queue and watching running sessions.
- The `voro` CLI for creating, proposing, transitioning, and inspecting tasks.
- Dispatch to coding agents with built-in `claude` and `codex` agent templates.
