# Changelog

All notable changes to Voro are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->

## [Unreleased](https://github.com/ClachDev/Voro/compare/27b1105...HEAD) - ReleaseDate

Initial release of Voro, a local, single-operator command centre that
prioritises tasks across many projects and dispatches them to coding agents.

### Added

- The next-action queue: a single cross-project ranking of the work that needs
  a human first, then the highest-scoring ready tasks.
- `voro-core`: the SQLite store, task state machine, scheduler, and scoring.
- The ratatui TUI cockpit for triaging the queue and watching running sessions.
- The `voro` CLI for creating, proposing, transitioning, and inspecting tasks.
- Dispatch to coding agents with built-in `claude` and `codex` agent templates.
