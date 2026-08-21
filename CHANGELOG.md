# Changelog

All notable changes to Voro are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!-- next-header -->

## [Unreleased](https://github.com/ClachDev/Voro/compare/v0.2.0...HEAD) - ReleaseDate

## [0.2.0](https://github.com/ClachDev/Voro/compare/v0.1.0...v0.2.0) - 2026-08-21

The release where the loop closes around the operator rather than the agent.
Refine answers the proposal that is worth keeping but badly written; a task's
own agent session takes a message without you leaving the cockpit; and the
queue ranks by what a row costs your attention rather than by what it is worth
alone. The cockpit itself grew a key map, a mouse, a Config screen, and colour
where something is stuck on you. Underneath: multi-repo projects, document
links so an agent reads the plan instead of rediscovering it, a deep flag for
the work that deserves your strongest model, and a store that will not migrate
itself out from under you.

### Added

- **Refine: the answer to a proposal worth keeping but badly written.**
  Triage offered three verdicts, and a proposal that was vague, over-scoped
  or written against the wrong repo fitted none of them — accept it and the
  problem moves downstream to dispatch and review, reject it and the idea is
  lost, rewrite it by hand and you have written the task yourself. `r` on the
  row sends the task to an agent with a one-line note about what is wrong
  (`voro triage <id> refine --note "..."` at the shell), and `R` opens an
  interactive session to talk the body into shape. Refine is not a verdict —
  it is a key pressed over the row, not an outcome in the menu — and the task
  leaves the queue for a real `refining` state while the rewrite is in
  flight, so no verdict can race the agent's own write and a round that dies
  is recorded rather than leaving the proposal looking untouched. `C` cancels
  a round that is taking too long. The task comes back marked `↻ refined`,
  retitled too where the note asked for that, to be triaged next pass. Ready
  work refines as well as proposals: rewriting a body invalidates the verdict
  given to the old one, so a refined ready task returns through triage.
- **Quick-message a task's session**: `a` in the TUI collects one line and sends
  it straight into the task's recorded agent session, headlessly, without
  suspending the cockpit — for a `needs-input`, `review`, or `waiting` task
  whose session is between turns. On a review or waiting task the message *is*
  the rejection: the send goes first and the feedback is appended to the body
  and logged behind it, so a message the agent refuses leaves the task
  untouched rather than recording feedback nobody received. The interactive
  jump-in moves from `a` to `A`. Agents declare the capability with a new
  optional `message` verb (`{session}` plus `{prompt_file}`, and the optional
  `{new_session}` for an agent that can only be joined by forking), built in
  for `claude`; an agent without one, such as `codex`, reports so on the status
  line and keeps its jump-in. The message resumes the session in place, so the
  whole life of a task is one session id, one `voro-<id>-<slug>` name and one
  transcript — the handle you find it by in `claude agents` and the `/resume`
  picker never moves out from under you.
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
- **A Config screen, so `voro.toml` is editable from the cockpit.** The
  fourth TUI screen (`alt-4`) lists the agents Voro will dispatch to with the
  command each one runs and the file it was resolved from, and the viewers
  editably: add one, edit its command, delete it, or pick the default.
  Writes go through a rewriter that preserves the file's existing content,
  formatting and comments and touches only the key that changed, so a
  hand-written `voro.toml` survives being edited from the TUI. Deleting a
  viewer a project names is refused, naming the projects. `voro viewer
  add`/`remove` are the same operations at the shell.
- `?` in the TUI opens the current screen's complete key map — actions,
  navigation, and screen switching — including the keys no hint ever advertised
  (`o` open in a viewer, `g` open the PR, `A` jump into the session, `l` page
  the log, `J`/`K`
  and the page keys, `ctrl-r`). The overlay sizes itself to the terminal and
  `tab` turns the page where it runs to more than one; any other key closes
  it again.
- **The mouse selects.** A click moves the selection to the row under it, on
  the queue, in the task browser and on the projects list, and inside a
  picker a second click on the option already under the cursor confirms it as
  ⏎ would. A click never fires a row's action, so nothing irreversible is one
  stray click away.
- **Queue rows name each task's state, and colour the ones stuck on you.**
  The first column used to render the derived next-action verb, a near
  synonym of the state it comes from, which left the cockpit speaking a fifth
  vocabulary for the same fact. It now shows the state itself, with
  `needs-input` cyan, `review` green and `stalled` red; `ready` and
  `proposed` stay plain, so an uncoloured queue reads as nothing waiting on
  you. The colours are terminal palette entries rather than fixed RGB, so
  your theme still picks the hue, and the word is on the row either way.
- **Handed-off work rides the running strip.** A `waiting` task — one you
  handed off with `w` because its PR is with someone else — earns no score
  and is out of the queue by design, which used to mean merged PRs sat
  unaccepted for days with nothing saying so. To you it is the same fact as a
  running task: something else owns the work. The strip now carries it,
  badged with what the hand-off is holding up (`blocks N`) and whether a PR
  tracks it, its elapsed time counted from the hand-off rather than from the
  session underneath, which says nothing about how long the PR has been
  sitting there. The strip is taller too: a normal day puts more in flight
  than the four rows it used to cap at.
- **Task bodies render as markdown.** Bodies are written in markdown and were
  drawn as plain text, so `**bold**`, backticks, fences, headings and bullets
  all showed their own markers. A small renderer now styles that subset
  wherever a body is displayed, and anything it does not understand — an
  unclosed fence, an unmatched `**` — degrades to literal text rather than
  being dropped or mangled. The agent's own blocks parse the same way: a
  completion summary, a rework account and a question each sit behind a cyan
  `│` gutter, which separates the agent's voice from the body without the
  wash of colour that used to do that job and collide with inline code.
- **Built-in viewers**, so a fresh install's first `o` works: `code`, `cursor`
  and `zed` ship compiled in and are probed against `PATH` in that order, the
  way the built-in agents already are. A `voro.toml` still wins — a
  `[viewers.code]` table overrides that built-in wholesale — and `voro viewer
  list` and the Config screen now show the built-ins beside your own with each
  one's provenance, starring whichever the next `open` will run. A built-in is
  overridden rather than edited or deleted, and both surfaces say so. When
  nothing resolves at all, `o` in the TUI raises the add-viewer form on the
  spot rather than only complaining, and at the shell the failure asks you to
  register the viewer you already use (`voro viewer add <name> '<cmd>'`), with
  the probed built-ins named after the action — instead of reporting a config
  file that may not exist as an invalid *agents* config. Naming the editor is
  enough: the command is now optional in both the form and `voro viewer add`,
  defaulting to `<name> {path}`, or to a built-in's own line when you name one
  to override it. In the form it *follows* the name as you type — dim until you
  take it over, which the first character you type in the field does whole —
  and deleting what you wrote hands it back to the name.
- **Capped sessions are visible instead of silently stuck.** A usage cap does
  not kill a backgrounded agent — the supervisor stays alive and waits for the
  window to reset — so capped work used to ride the running strip looking
  healthy for hours. Its row is now badged `⚠ capped ↻21:50`, with the reset
  time where the agent named one and `⚠ capped · reset passed` once that time
  has gone by. The task stays `running` and the session stays open: nothing is
  dead and nothing needs redispatching. The badge clears itself once the
  session is continued. Agents declare the capability with a new optional
  `logs` verb (`{session}`), built in for `claude`; an agent without one, such
  as `codex`, is probed for nothing and behaves exactly as before.
- **The cap badge takes its reset time from the agent, not from its screen.**
  Voro used to read "resets 6:40pm" off a capped session's output and guess
  which 6:40pm was meant, because a bare clock time carries no date. Agents can
  now report the same thing exactly, through a new optional `cap` verb that
  prints when the account's usage window reopens as a Unix epoch. The badge
  reads the same, but "reset passed" is now a fact rather than a
  nearest-occurrence guess, and a cap whose wording named no time at all gets
  one. A subscription meters more than one window — the five-hour pool, the
  weekly one, and each strong model's own allowance — so the verb is asked with
  the model the session launched under, and sessions asking the same thing share
  one reading. The built-in `claude` agent defines it; `codex` does not, and
  anything Voro cannot ask keeps the old parse. Because asking costs the agent
  an API call rather than a screen replay, Voro asks only while a session is
  already badged capped, and once per capped episode.
- **One key gets every capped session working again.** A usage cap ends a
  session's turn and leaves it there — nothing retries — so recovering the fleet
  used to mean attaching to each capped session in turn and typing "continue",
  and the reset hours went missing overnight. `u` now sweeps every badged
  session whose reset has passed, tells each to continue, and reports how many
  it nudged, how many are still waiting on their window, and any the agent
  refused. A capped session's supervisor is still holding it, so the sweep
  releases each one before resuming it and abandons that nudge if the release
  fails — nothing is sent that could only be refused. For now it fires only when
  you press it — resuming capped sessions
  automatically once the window reopens is the intended next step, and this is
  the half that will sit underneath it. Sessions already back at work are
  untouched, and the badge drops as each nudge lands so a second press cannot
  start a second agent on the same worktree.
- **Closing a task stops its agent session.** Voro used to leave every session
  it launched registered with the agent forever: a `claude agents` listing full
  of finished `voro-*` entries, each backed by a supervisor process that runs
  until you reboot. That listing is how you find a session to attach to and how
  Voro reads liveness, so both got worse the longer it grew. A session's entry
  now follows its row: accept, abort or abandon a task, and the agent is asked
  to retire its own entry for it. The conversation is kept: `claude stop` drops
  the entry from the default listing and `claude attach` still reopens it.
  Sessions Voro keeps open on purpose (`needs-input`, `review`, `waiting`) are
  not stopped at all — you still answer and reject into the row, and `A` still
  opens it with its full context — and a quick message releases the
  supervisor's hold at the moment you send, so the session resumes in place.
  Every stop follows something you just did, a closing verdict or that send:
  nothing reaches into a session on a background pass, which is what used to
  make an unrelated window flicker while you worked. Agents declare the capability with a new optional
  `stop` verb (`{session}`), built in for `claude`; an agent without one, such
  as `codex`, behaves exactly as before, and a stop that fails leaves a line in
  `launches.log` rather than touching the transition.
- **A review branch that has gone stale says so.** A task can sit in `review`
  while other work merges, leaving its branch in conflict with the base it
  was cut from. For a review task with a tracked PR, Voro asks `gh` whether
  the PR still merges and shows a `[branch conflicts]` marker when it does
  not — read fresh at the moment you look, never stored, and purely
  informational: it takes no action and changes no recommendation. A missing
  or unauthenticated `gh` shows nothing, since an absent signal is not a
  conflict. The probe is network I/O, so it runs once per selection, off the
  event loop, rather than once per rendered row. Fixing one is the agent's
  job: the dispatch preamble now tells it to fetch the base and rebase inside
  its own worktree, so a conflicted branch is one message to the session that
  is still open.
- **Sending work back costs one review, not two.** Rejecting used to reopen
  the whole diff when the rework came back, with the feedback that prompted
  it out of view — and a rejection that expensive is a rejection not made. A
  rejection now records the revision you reviewed, so the second pass opens
  only what the rework added: on GitHub the compare-within-a-PR view, which
  keeps the review comments and the merge button where they are, and in a
  local viewer a diff based at that revision. Every gap — nothing new, a
  force-push that took the revision away — degrades to the full diff with a
  notice rather than an error. The rework's own summary answers the feedback
  point by point and is rendered against it in the detail pane and `voro
  show`. First reviews are unchanged, down to the network calls they make.
- **Archive a project that has stopped mattering.** Weight 0 only snoozed
  one, which leaves it untagged and indistinguishable from a project that is
  merely resting. `A` on the projects screen — `voro project
  archive`/`unarchive` at the shell — hides the project and all of its tasks,
  in whatever state each holds, from the queue, the inbox, `next`, the stats
  and the running strip, without transitioning or closing anything, so
  unarchiving restores the view exactly. It stays on the projects screen and
  in `voro project list`, dimmed under an `[archived]` tag, and no interface
  will file new work into it.
- **Work that other work waits on scores higher.** The score ignored the
  dependency graph, so a task three others were parked behind ranked
  identically to one blocking nothing and you had to reconstruct the graph in
  your head to notice that finishing one would release three. It gains a
  fourth term — one point per direct open `blocks` dependent, capped at two —
  inside the project-weight multiply with the others. Only `blocks` edges
  count and only direct ones, so a long chain cannot manufacture a number
  nobody drew the graph to mean, and the cap keeps it a nudge rather than a
  priority level: it peaks at the `review` bonus, so a P2 blocking five
  tasks never outranks a genuine P0. `x` folds out the decomposition it
  appears in.
- **Body edits are recoverable and guarded.** Every edit that changes a
  non-empty task body records the text it replaced as a `body` event, so a
  rewrite is no longer irreversible; `voro show <id> --event <event-id>` prints
  that text back, ready to redirect into `set --body-file`. A replacement that
  would leave the body empty is refused unless `--allow-empty` is given, and
  `voro set --append-body[-file]` adds to a body instead of replacing it.
- `voro set <id> --unlink <kind>:<other-id>` drops a single dependency edge —
  `related:7`, `discovered-from:4`, `blocks:9` — named as `voro show` lists it.
  A pair of tasks carrying two edges keeps the one not named, so an edge
  authored by mistake no longer has to be removed with raw SQL; dropping a
  blocker reconciles readiness as any other blocker edit does.
- **The `voro-cli` skill installs as a Claude Code plugin.** The repository
  is a plugin marketplace: `claude plugin marketplace add ClachDev/Voro`
  followed by `claude plugin install voro` registers the skill globally, so
  any coding session in any project — not only a Voro checkout — knows the
  read and write verbs, how the database is resolved, and how to file
  follow-up work against the right project.
- **Migrating your real store now takes a yes.** Every build used to migrate
  any database it opened as a side effect of opening it, which is how an
  unreleased migration from a worktree once landed on real data. The operator's
  store now carries a `protected` marker — in the file itself, so it survives a
  symlink, a moved data directory, or a restored copy — and a protected store
  with pending migrations refuses to migrate silently: launching the TUI shows
  the pending count and asks before the terminal is taken, every CLI verb
  refuses with a message pointing at the new `voro migrate` verb, and
  `voro migrate --yes` consents from a script, recorded in the migration
  journal's `applied_by` so even the override leaves a trace. A fresh install
  still creates its database with no ceremony, and dev and scratch stores
  migrate on open exactly as before.
- **A build from a `target/` directory opens a dev store, not your real one.** A
  `voro` run out of `target/debug` or `target/release` now uses
  `~/.local/share/voro/dev.db`, seeded on first run with a fixture board
  covering every task state, both live and dead sessions, each dependency kind,
  a multi-repo project and an archived one. `voro seed --force` rebuilds it;
  seeding the operator's store is refused. Such a build also takes no database
  from the environment: dispatch exports `VORO_DB` so a session's return path
  finds the store its dispatcher was on, which put the operator's database in
  the environment of every agent working in a worktree, and a `cargo run` there
  inherited it. An explicit `--db` is still honoured. This chooses a default
  and bounds nothing — `cargo install --path` builds a working checkout into an
  ordinary install location, where it counts as installed — so what protects
  the schema is the journal and the version check below.
- **The store records which migrations it is made of.** A `schema_migrations`
  journal keeps each applied migration's SQL alongside the build that applied
  it, and every open checks it against the migrations the running binary
  carries. `user_version` is a counter, so it cannot distinguish two branches
  that each add a migration 17 — the binary carrying the other 17 applies
  nothing, refuses nothing, and dies at the first query on a missing column.
  That case is now refused up front, naming the migration, the build that
  applied it, and the snapshot to restore. Migrations applied before the
  journal existed are recorded as unverifiable rather than assumed. One new
  rule follows: an applied migration is immutable, and editing one is reported
  as a divergence.
- **Two further guards on opening a store.** A database whose schema is ahead of
  the running binary is refused with an error naming the way out, instead of
  silently skipping the migration and failing later as a missing column; and
  any open that is about to migrate first copies the file to `backups/` beside
  it, so a migration that renames or drops a column stays recoverable.
  Startup failures now print their message rather than a `Debug` dump.
- **The TUI footer names the database it opened** whenever that is not your
  own store — dim, right-aligned, on every screen — so a scratch or a dev
  store says which one it is instead of looking exactly like the real thing.
  Your own store shows nothing and leaves the key line the row entire.

### Changed

- **The two review keys are static: `g`/`voro pr` is always GitHub, `o`/`voro
  open` is always a local viewer.** They used to be one operation in two
  media — `pr` resolved the project's review action and could land on either
  — which made the meaning of a key depend on a setting last touched months
  ago, something to be thought about before pressing it. Neither does now.
  `g` on a review task with no PR recorded creates one from the completion
  summary, after showing you the branch and title, and opens it in the
  browser once it exists, since creating a pull request is all but always
  followed by looking at it. On a checkout with nowhere to push, `g` says so
  and names `o` instead.
- **The queue ranks by what a row costs you, not only by what it is worth.**
  Ranking on worth alone prices attention backwards: reviewing a PR is
  fifteen to sixty minutes of a human being and triaging a proposal is one,
  so the expensive row led and the cheap one starved. Rows are now ordered by
  score divided by the cost of their own next action, in a deliberately
  narrow band — 0.8 to answer or triage, 1.0 to dispatch, 1.4 to review, 1.8
  to do — so one priority level still outweighs the whole of it; a `[costs]`
  table in `voro.toml` overrides the defaults. Dispatch is shaped rather than
  priced, its real cost being a concurrency slot: `max_running` (default 5)
  caps how many sessions ride at once, and at the cap the queue shows a
  capacity line in place of the rows that would open another. Proposals no
  longer take a queue slot each — they collapse into one digest row per
  project, scored as its best child, which `⏎` folds open for rapid triage.
  The stored score, the state machine and every transition are untouched:
  this is the order things are shown in.
- **A question is answered in the agent's own session, not in Voro.** An
  agent that hits a blocker still runs `voro ask`, which parks the task in
  `needs-input` and floats it to the top of the queue — but Voro is the
  signpost now rather than the channel. You answer in the session itself
  (`A` opens it, `a` sends one line into it) and `voro resume <id>`, or `⏎`
  on the row, returns the task to `running`. What this replaces never worked:
  the built-in `claude` agent defines no headless `continue` verb, so
  answering fell back to respawning the whole task prompt and restarted the
  work instead of resuming the conversation. No answer text is recorded,
  because the exchange is already in the transcript. The `answer` and
  `continue` CLI verbs are gone, and a `continue` line in an agent's table in
  `voro.toml` is now refused as an unknown field.
- **A bare digit sets the number on the row you have selected; screen jumps
  move to `alt-1`–`alt-4`.** Digits meant two things and neither was the
  frequent one: `0`–`5` weighted a project on the projects screen while
  `1`–`4` jumped between screens everywhere else, so pressing `1` there to
  reach the cockpit silently re-weighted the selected project and reordered
  every queue with nothing said and nothing to undo it. The digit's meaning
  now follows the selection — `0`–`3` a task's priority on the cockpit and in
  the task browser, `0`–`5` a project's weight on the projects screen — and
  `tab` still cycles the four screens. A digit with nothing to act on says
  why: a collapsed digest names no single task, and `4` on a task reports
  that priority stops at P3.
- **The Config screen edits settings you select, starting with the dispatch
  cap.** It used to bind a letter per value — `V` for the default viewer, `A`
  for the default agent — and `max_running`, the one queue option with no TUI
  surface at all, was next in line for a third. Instead the screen has grown a
  **Settings** list above the viewers: the default agent, the default viewer,
  and the dispatch cap, each showing the value in force and whether it came from
  your `voro.toml` or is Voro's own default. One selection runs over the
  settings and the viewers together, and ⏎ (or `e`) edits whatever it is on —
  the same pickers `V` and `A` used to open, and a numeric entry for the cap.
  Raising the cap while the queue reads `⏸ dispatch at capacity` brings the
  dispatch rows back on that keypress. `a` adds a viewer and `d` deletes one as
  before; `V` and `A` are unbound, and the `?` map no longer lists them. Every
  write still goes through the comment-preserving writer, so your file's
  formatting and comments survive it.
- **`n` proposes a task from one typed line, in the background.** It opens a
  one-line modal rather than the `$EDITOR` form; ⏎ hands what you typed to a
  headless agent, which expands it into a title and a dispatchable body and
  files the task with `voro add`. The TUI never suspends and nothing waits on
  the agent — the proposal appears in the queue as `proposed` on a later
  refresh, like any other, and a launch that fails leaves its trace in
  `launches.log` and the session log rather than on screen. `N` still opens the
  interactive planning session, and the manual `$EDITOR` form moves to `ctrl-n`,
  still the only path that sets state, priority, agent and blockers at creation
  time. A quick propose runs the `dispatch` verb on a launch that names no task,
  so an agent whose `dispatch` template carries `{task_id}` is refused up front,
  naming the template to fix, rather than launching with the placeholder
  unsubstituted.
- **The review card shows the agent's completion summary.** A task in `review`
  or `waiting` renders what the agent reported at `done` — its account of what
  changed and how it was verified — above the task body, in the TUI detail pane
  and in `voro show`. It was previously rendered only on a task that had been
  rejected once, which left a first review with no account of the work anywhere
  but `voro show`'s event log — and no PR or configured viewer to fall back on,
  on the fresh install where that matters most. A rework's block is unchanged
  but for its heading, and shows nothing while the rework is still in flight.
- The per-project review action is now what it does: a viewer name. Since the
  review keys split — `pr` always GitHub, `open` always a local viewer — the
  setting decided only which `[viewers.<name>]` table a project's local diffs
  open in, so `projects.review_action` becomes `projects.viewer` and holds that
  name or nothing. Existing databases convert in place: `viewer:<name>` keeps
  its name, and `auto`, `pr`, and a bare `viewer` — three spellings of "name no
  viewer" — all become the default viewer. `voro project action <p>
  <auto|pr|viewer[:NAME]>` is now `voro project viewer <p> [NAME]`, naming no
  viewer to fall back to the default, and the projects screen's `v` picker
  offers the default and each named viewer instead of two entries that did
  nothing distinguishable.
- `projects.path` is gone. Existing databases convert in place: every project's
  old path becomes its default repo, and existing tasks resolve to exactly the
  checkouts they had before. `voro project add` and `voro project path` keep
  working, now on the default repo.
- `voro import`'s `--repo` flag now names a Voro repo, matching `--repo`
  everywhere else in the CLI. The GitHub-side override it used to be is now
  `--gh-repo owner/name`.
- **The project picker offers only projects that can take the work, weightiest
  first.** It used to list every project alphabetically, archived ones included
  — and an archived project refuses new tasks, so picking one could only fail,
  which in the `$EDITOR` and planning flows it did only after you had written
  the task out. Archived projects are now dropped from the picker entirely
  (unarchiving stays where it was, on the projects screen), and the rest are
  ordered by the weight you set every morning, each row showing it, so the
  project you are actually working on is the one under the cursor. A parked
  project is still offered and simply sorts last. One live project beside
  archived ones now skips the picker and creates straight into it, and with
  every project archived `n`/`N` say so instead of raising an empty list.
- **A first run lands on the Projects screen and stays there until a project
  exists.** An empty database opened on a cockpit whose queue read `nothing
  to do — press n`, and `n` refused, citing a screen number nothing
  documents. The cockpit and the task browser are now gated until there is a
  project to show, `tab` cycling just Projects and Config until then, and the
  task browser has an empty state of its own rather than a blank bordered box
  that reads as a rendering fault.
- The TUI's contextual key line is shorter and speaks one language. A lowercase
  key and its shifted sibling now share a single slot labelled with the base
  verb — `d/D dispatch`, `r/R refine`, `n/N new` — instead of three different
  renderings of the same idea, and the line keeps only what changes a task's
  state or destiny: `x` score, `h` history, `c` docs and `l` log still work but
  are documented in the `?` map rather than on the line. No key was rebound.
- The cockpit key line advertises `d/D dispatch` only on a `ready` or `stalled`
  row, where dispatch can actually act, rather than on any selection — which
  also makes room for the new `a/A message` slot within the line's ten.
- **The key line stops offering `o` and `g` on a review task with nothing to
  show.** A task whose whole product is its summary — an investigation, a
  triage, an audit — reaches `review` having never made a branch, and both keys
  were advertised there anyway: `o` builds its diff from the task's branch, and
  `g` on a task with no tracked PR goes to `pr` create, which refuses without
  one. The two slots are now earned by having something to show rather than by
  the state alone — `o` on a `review` or `running` task carrying a branch, `g`
  on a `review` task carrying a branch or a tracked PR — in the cockpit and the
  task browser alike, which leaves the branchless review row two slots shorter
  beside a card already recommending *accept*. Neither key's binding changes:
  both still work everywhere they worked before, and the `?` key map still
  lists them.
- **A minimum supported Rust version, and an Install section that matches what
  is actually published.** The workspace declares `rust-version = "1.88"` — the
  floor its dependency tree already imposes, above the 1.85 that edition 2024
  alone would need — so `cargo install voro` on an older toolchain now refuses
  up front naming the version required, rather than failing deep in a
  dependency, and a CI job builds and tests on that exact toolchain so the
  declaration cannot quietly drift out of truth in either direction. The
  README says so too, and no longer implies a prebuilt binary
  exists for every Unix: binaries are published for `x86_64-unknown-linux-gnu`
  and `aarch64-apple-darwin`, and every other platform, an Intel Mac included,
  is pointed at the source build. It also warns that `~/.cargo/bin`, where the
  shell installer puts the binary, is not on the `PATH` of someone who has never
  installed Rust.
- **Dispatch no longer refuses because your checkout is dirty.** The guard
  existed so an agent's work would land on a clean base, which the worktree
  convention already guarantees — `git worktree add` snapshots the commit,
  not the working tree — while the refusal fired exactly when you were
  working, which is most of the time. A path that is not a git repository is
  still refused, and named. Isolation moved into the prompt instead: the
  preamble tells every dispatched agent to work in a throwaway worktree and
  never touch the primary checkout, and names the harness's own mechanism for
  making one ahead of `git worktree add`, which it keeps as the fallback.
  Pointing a harness that has such a tool at a pre-made worktree raises an
  approval prompt a headless session can never answer, which is how three of
  four dispatches on one day in July froze two minutes in and sat `running`
  for eleven hours.
- **The prompts Voro sends ask for the two things reviews kept lacking.** A
  completion summary is the pull request's description — `voro pr` opens the
  PR with that text as its body — so every agent-facing prompt and the
  `voro-cli` skill now say so outright, in place of a gesture at a "PR-ready
  `--summary`" that returned one-liners like "Implemented X, tests pass". A
  planning session is asked where evidence should go and what the task it
  drafts supersedes, so planned work stops committing verification write-ups
  and design docs into project repos as files nobody asked for.
- **Agent contract**: verb templates gain a `{session_name}` placeholder, and
  the built-in `claude` agent now names its session with it instead of spelling
  `voro-{task_id}` itself. Every session Voro launches carries a name it
  composed — `voro-<id>-<slug>` for a dispatch, the slug cut from the task's
  own title so the name can be read without the queue open beside it,
  `voro-<id>-refine` for a refine, `voro-plan-<project>` for a planning
  session — so a refine no longer
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
- **A quick propose is named for its project, like the planning session beside
  it.** `n` used to launch its agent under `voro-propose-<project_id>` while `N`
  on the same project opened `voro-plan-<project>`, so the two read in
  `claude agents` as different kinds of thing, and the bare number was the one
  shape a Voro-composed name otherwise reserves for a task id. A quick propose
  on the `voro` project is now `voro-propose-voro`, sanitised exactly as a
  planning session's name is, and its prompt and log files under the runtime
  directory are stamped `propose-voro` to match. Nothing else about it moves: it
  still runs the `dispatch` verb, opens no session row and files its task with
  `voro add`.

### Fixed

- Sessions no longer read as alive for days after they die. `claude agents
  --json` never retires an entry — in one real listing 34 of 52 sat at
  `"state": "blocked"` long after the process was gone — and Voro treated
  only `"done"` as finished, so the running strip showed 41 sessions running
  where 7 were. Liveness is now read from the entry's pid as well as its
  state, and a session records which source answered for it.
- The status line wraps instead of clipping at the pane width. Voro's
  refusals are written to end on the way out — the key to press instead — so
  a clipped line lost precisely the half worth reading: `g` on a review task
  in a non-GitHub checkout said the repository was not GitHub and never got
  as far as naming `o`, and `o` with no viewer configured stopped at "no
  viewer con". Typed text no longer scrolls out of sight either: the prompt,
  the quick-create line and the link-PR field size their popup from the
  wrapped text rather than from a newline the input cannot contain, so a long
  note keeps both its tail and the cursor.
- `voro set <id> --blocked-by <other>` no longer silently does nothing when
  the pair is already joined by an edge of another kind. Dependencies were
  keyed on the pair alone, so a task filed with `propose --from` — which
  writes a `discovered-from` edge — could not then be given a blocker: the
  write was dropped, `task <id> updated` was printed, and the task stayed
  ready with its blocker open. The key now carries the kind, so a pair can
  hold one edge of each.
- A review task in a project with nowhere to push no longer advertises `next:
  pr`, an action that could only fail there. Where the task's checkout has no
  git remote, every surface that names a next action — the cockpit detail card,
  the browser and `voro list` suffixes, `voro show`, and the `voro inbox` verb
  column — now reads `next: open` and points at `o` rather than `g`. The keys
  themselves are unchanged: `g`/`pr` is still always GitHub and still refuses a
  checkout `gh` cannot address, and `o`/`open` is still always the local viewer.
  The advertisement rides a rendered row, so it asks git alone whether the
  checkout has any remote, network-free and memoised per checkout, rather than
  paying `pr`'s `gh` round-trip; a checkout whose remote is not GitHub reads as
  before and is refused at press time.
- The `[incomplete report]` marker flags a half-written report rather than a
  task that produced no code. It was an XOR over a review task's branch and
  summary, on the reading that work with no artifact also has nothing to say
  — but an investigation that concludes the bug was fixed elsewhere, a triage
  that concludes won't-fix, an audit whose whole product is its findings each
  end with a summary and no branch, and each was flagged with no way to clear
  it. The marker now means what it says: the summary is missing. It withholds
  exactly one verb — `pr`, the one built from the summary the task does not
  have — and stands beside every other, in `voro show` as on the detail card,
  the browser row and the `list` suffix. A review task that made no branch at
  all derives *accept* rather than *pr*, since there is nothing a pull request
  could carry.
- Jumping into a task's session picks attach or resume from the session, not
  from the task's state. A backgrounded session commonly outlives `running` —
  rebasing a stale review branch is asked of one — and `claude --resume`
  refuses a session its supervisor still holds, so the jump-in failed on
  exactly the review tasks it was most wanted for. It now reads the session's
  own liveness and picks the verb that will work.
- Two errors a first-time user is likeliest to meet now say what to do about
  them. An editor that will not run reports the variable and the command it
  came from rather than a bare exit code — `could not run $EDITOR
  (definitely-not-an-editor): not found` in place of `editor exited with exit
  status: 127` — and a missing `gh` is named as the GitHub CLI, with where to
  install it, instead of an `os error 2` that reads as if the checkout were
  missing.
- **The Config screen's agents pane no longer hides agents in silence.** It drew
  the rows it could fit and dropped the rest, and its bottom border looked the
  same either way — on a short terminal with several agents configured, the ones
  past the fold could only be read by resizing the terminal or running `voro
  agent list` at the shell. The pane now scrolls with `J`/`K` and the page keys,
  the same gesture the cockpit's focus card takes and for the same reason: this
  pane has no selection of its own, `j`/`k` on that screen belonging to the
  viewers list below it. When there is nothing hidden the border says nothing;
  when there is, it carries the overflow and the keys that move it.
- **A capped session's badge now shows the reset time it actually named.** Real
  cap messages end with an upgrade prompt that mentions a usage limit of its
  own, and that trailing mention was winning: it carries no time, so every
  genuine cap badged as a bare `⚠ capped` and the strip could never say whether
  the window had reopened. The prompt is now read as the boilerplate it is.
  Verified against a real cap rather than the wordings this was first written
  from.
- A backgrounded session that dies on a usage cap is recorded `capped` rather
  than `failed`. The classification read Voro's own launch log, which for a
  `--bg` launch holds nothing but the backgrounding banner — the launcher exits
  at birth — so it could essentially never fire; it now reads the session's own
  output through the agent's `logs` verb, keeping the log tail as the fallback
  for agents defining none. The phrases it matches also cover what agents
  actually write: a five-hour cap is worded "Session limit reached" and a weekly
  one "Weekly limit reached", neither of which contains "usage limit", "rate
  limit" or "quota exceeded", so the previous list matched almost no real cap.
  Warnings short of a cap ("approaching", "80% of your", "not your") are
  excluded, so the marker still never fires on a healthy session.
- **A quick message no longer wakes a session that cannot do anything.** The
  `message` verb carried no `--permission-mode`, and the flag is per invocation
  rather than a property of the session, so every resumed turn ran in ask mode
  against a closed stdin: edits and commands stopped for approvals nobody could
  give, and the refusals went to the launch log instead of the TUI. Sends looked
  delivered and quietly did nothing.
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
