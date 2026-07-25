# Changelog

All notable changes to Voro are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->

## [Unreleased](https://github.com/ClachDev/Voro/compare/v0.1.0...HEAD) - ReleaseDate

### Added

- Multi-repo projects: a project now owns one or more **repos**, and a task may
  name which one it runs in. Manage them with `voro repo add/list/path/default/
  remove`, pick one per task with `voro add --repo NAME` and `voro set --repo
  NAME`, and import from one with `voro import --repo NAME`. A project allocates
  attention; its repos locate checkouts.

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
