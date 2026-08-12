# Changelog

All notable changes to Voro are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->

## [Unreleased](https://github.com/ClachDev/Voro/compare/v0.1.0...HEAD) - ReleaseDate

### Added

- **Quick-message a task's session**: `a` in the TUI collects one line and sends
  it straight into the task's recorded agent session, headlessly, without
  suspending the cockpit — for a `needs-input`, `review`, or `waiting` task
  whose session is between turns. On a review or waiting task the message *is*
  the rejection: the send goes first and the feedback is appended to the body
  and logged behind it, so a message the agent refuses leaves the task
  untouched rather than recording feedback nobody received. The interactive
  jump-in moves from `a` to `A`. Agents declare the capability with a new
  optional `message` verb (`{session}` plus `{prompt_file}`, and the optional
  `{new_session}` for an agent that can only be joined by forking — which is
  how the built-in `claude` verb reaches a session its supervisor still holds),
  built in for `claude`; an agent without one, such as `codex`, reports so on
  the status line and keeps its jump-in.
- `voro set <id> --unlink <kind>:<other-id>` drops a single dependency edge —
  `related:7`, `discovered-from:4`, `blocks:9` — named as `voro show` lists it.
  A pair of tasks carrying two edges keeps the one not named, so an edge
  authored by mistake no longer has to be removed with raw SQL; dropping a
  blocker reconciles readiness as any other blocker edit does.
- **Body edits are recoverable and guarded.** Every edit that changes a
  non-empty task body records the text it replaced as a `body` event, so a
  rewrite is no longer irreversible; `voro show <id> --event <event-id>` prints
  that text back, ready to redirect into `set --body-file`. A replacement that
  would leave the body empty is refused unless `--allow-empty` is given, and
  `voro set --append-body[-file]` adds to a body instead of replacing it.
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
- Document links from the TUI: `c` on a selected task — on the cockpit, in the
  task browser, and inside the browser's detail popup — opens a picker over
  every registered document with the task's existing links ticked, and ⏎ links
  or unlinks the highlighted one without leaving the TUI. Registering and
  removing documents remains `voro doc add`/`remove`.
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
- `?` in the TUI opens the current screen's complete key map — actions,
  navigation, and screen switching — including the keys no hint ever advertised
  (`o` open in a viewer, `g` open the PR, `A` jump into the session, `l` page
  the log, `J`/`K`
  and the page keys, `ctrl-r`). Any key closes it again.

### Changed

- The cockpit key line advertises `d/D dispatch` only on a `ready` or `stalled`
  row, where dispatch can actually act, rather than on any selection — which
  also makes room for the new `a/A message` slot within the line's ten.
- **Agent contract**: verb templates gain a `{session_name}` placeholder, and
  the built-in `claude` agent now names its session with it instead of spelling
  `voro-{task_id}` itself. Every session Voro launches carries a name it
  composed — `voro-<id>` for a dispatch (unchanged), `voro-<id>-refine` for a
  refine, `voro-plan-<project>` for a planning session — so a refine no longer
  launches under the literal, shared name `voro-{task_id}` and can be found and
  attached to in `claude agents`. `{session_name}` is honoured on `dispatch` and
  `plan` and refused elsewhere, the same rule `{model}` follows; `{task_id}` is
  now refused on `plan`, whose target may be a project with no task. No verb was
  added: an agent defining only `dispatch` (or `cmd`) refines exactly as before.
  A wholesale `[agents.claude]` override copied from an older `voro.toml` keeps
  working, and keeps its old naming.
- Prompts and command lines are filled by a single-pass renderer, so a task
  body, branch name, document title or project name containing `{task_id}`,
  `{db}`, `{note}`, `{seed}`, `{branch}` or `{docs}` now reaches the agent
  verbatim instead of being rewritten before it is read.
- The TUI's contextual key line is shorter and speaks one language. A lowercase
  key and its shifted sibling now share a single slot labelled with the base
  verb — `d/D dispatch`, `r/R refine`, `n/N new` — instead of three different
  renderings of the same idea, and the line keeps only what changes a task's
  state or destiny: `x` score, `h` history, `c` docs and `l` log still work but
  are documented in the `?` map rather than on the line. No key was rebound.
- `projects.path` is gone. Existing databases convert in place: every project's
  old path becomes its default repo, and existing tasks resolve to exactly the
  checkouts they had before. `voro project add` and `voro project path` keep
  working, now on the default repo.
- `voro import`'s `--repo` flag now names a Voro repo, matching `--repo`
  everywhere else in the CLI. The GitHub-side override it used to be is now
  `--gh-repo owner/name`.

### Fixed

- A headless `voro refine` is no longer marked `⚠ refine failed` seconds after
  it starts. The round runs the agent's own `dispatch` template, so under a
  `claude --bg`-style launcher the pid Voro recorded belonged to a launcher that
  exits at birth — reconciliation read it as a dead round and sent the proposal
  back to `proposed` under a failure marker while the agent worked on, so the
  rewrite that landed later was read under a warning saying no rewrite had
  happened. A round now captures a session ref at launch and is read from the
  agent's own listing, exactly as a dispatch is; a genuinely dead round is still
  caught, and an agent defining no `sessions` verb still reconciles by pid, as
  does the interactive refine, whose foreground child really is the round.
- A `voro set --body`/`--body-file` landing on a `proposed` task whose last
  refine round concluded `failed` corrects that round's outcome to applied, so a
  rewrite that arrives after its round was concluded reads `↻ refined` rather
  than sitting under the failure marker. The task neither reopens nor
  transitions.

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
