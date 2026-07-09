# Voro — Design Document

**Working title:** Voro (binary: `voro`; formerly Voro)
**Status:** Draft v2 — beads dropped in favour of an owned store; TUI-first; per-dispatch agent selection
**Author:** Michael Johnson (with Claude)
**Date:** 2026-07-08

## 1. Problem

When developing with AI agents, the scarce resource is no longer typing speed or even code review bandwidth — it is directed human attention. Work is spread across many projects, each with its own repository, its own backlog (sometimes GitHub issues, sometimes nothing), and its own shifting importance. Existing tools each solve a fragment of this: GitHub issues hold tasks but are siloed per repo and unprioritised across them; Claude Code's Agent screen shows active sessions but nothing about what *should* be active; Jira and Linear model priority but are heavyweight, remote, and hostile to fast iteration; agent orchestrators (Vibe Kanban, Crystal, Gas Town and dozens of others) answer "what are my agents doing?" rather than "where should I look next?".

Voro is an operational command centre whose single organising question is: **given how much I care about each project today, what is the one thing most worth my attention right now?** It has one output: **the queue** — a single ranked list of next actions, where an action is *answer this question*, *review this diff*, *triage this proposal*, or *start this task*. Everything competes on the same attention score; splitting decisions and startable work into separate views would just hand the arbitration between them back to the human. The queue offers a handful of rows, not one, because the score ranks but does not dictate — the human keeps the autonomy to pick around the top item. Startable work is capped at the few highest-scoring ready tasks so the queue stays an answer, not a todo list.

## 2. Goals and non-goals

**Goals.** A local, text-first tool that aggregates tasks across an arbitrary set of projects, including third-party repositories where we have no write access to the upstream issue tracker. Tasks are fully described — a task body should be able to serve as a ready-to-run agent prompt, prepared in advance and held behind a dependency until it becomes actionable. Project priority is a first-class, cheaply editable quantity that can change daily. The tool can dispatch tasks to coding agents — Claude Code by default, others selected per dispatch — and receive "I need a decision" signals back from them, closing the loop between the queue and running work.

**Non-goals**, at least for v1. No automatic task generation — agents may *propose* tasks, but nothing enters the priority queues without explicit human triage. No team features, sync servers, or cloud components; this is a single-operator tool. No two-way sync with GitHub issues in v1 — issues can be imported as tasks, but Voro's store is the source of truth for priority and state. No terminal multiplexing: Voro tracks and steers sessions at the task level, but attaching to a live agent is the agent's own tooling's job.

## 3. Concepts

A **project** is a unit of attention allocation: a name, a filesystem path to a checkout, and a *weight* expressing today's importance. Nothing about a project requires write access to any remote — a clone of a third-party repo is a perfectly good project. Note what a project is *not*: it carries no agent configuration.

A **task** belongs to exactly one project and carries an identifier, a title, a markdown **body written as a dispatchable prompt** where possible, a priority (P0–P3), a state (§6), dependencies on other tasks, an optional **agent override** for tasks that inherently require a specific capability, and an optional **question** field populated when the task is waiting on human input.

An **agent** is a named dispatch template — a command line into which the prompt and working directory are substituted. Agent selection is resolved at the moment of dispatch (§8), because the two real reasons to switch agent — a usage cap being hit, and a task needing a specific capability — are properties of the dispatch moment and the task respectively, never of the project.

The **attention score** is the scalar that merges project weight and task priority; the scheduler sorts everything by it.

## 4. Architecture

Three layers, deliberately decoupled so each can be replaced without disturbing the others. All three ship as one Rust workspace: a `voro-core` library crate (store + scheduler) and thin binaries over it.

The **store** is a single SQLite database owned by Voro (`~/.local/share/voro/voro.db`), holding projects, tasks, dependencies, and an event log. Schema in §5. SQLite because the access pattern is a single writer with trivial volumes, transactions matter (dispatch touches task state and session records together), and every future consumer — TUI, CLI verbs, a GUI, an ad-hoc `sqlite3` query when debugging — reads it natively.

The **scheduler** is pure logic: it pulls candidate tasks from the store, computes attention scores, and produces the two ordered views. It lives in `voro-core` with no I/O of its own — trivially testable, and identical beneath every interface.

The **cockpit** is the interface, and it is built first: a ratatui TUI rendering the queue, with in-place editing of tasks and project weights, and hosting the dispatch actions (§9). The agent-facing CLI verbs (§8) are a second, later consumer of `voro-core`; a GUI would be a third. Building the TUI first is safe precisely because the scheduler is a library — the interface risk is contained to rendering and keybindings, not logic.

**Dispatch** is the bridge to agents: given a task and a resolved agent, spawn a headless session in the project's path with the task body as the prompt, record the session, and observe its lifecycle. A thin verb surface lets the running agent write back to the store — most importantly, to raise a needs-input question.

## 5. The store

A note on the road not taken: an earlier draft wrapped beads (one database per project) to inherit its dependency resolution and ready-work detection. On inspection, beads' distinctive machinery — git-native JSONL sync, multi-agent locking, per-repo state travelling with the code — solves multi-agent, multi-machine contention problems this tool does not have, and the part we actually wanted is a small SQL query. We drop the dependency and keep the ideas: the dependency taxonomy and the discovered-from convention below are lifted directly from beads' design.

```sql
CREATE TABLE projects (
  id      INTEGER PRIMARY KEY,
  name    TEXT NOT NULL UNIQUE,
  path    TEXT NOT NULL,            -- checkout location; third-party repos welcome
  weight  INTEGER NOT NULL DEFAULT 3 CHECK (weight BETWEEN 0 AND 5)  -- 0 = parked/hidden
);

CREATE TABLE tasks (
  id         INTEGER PRIMARY KEY,
  project_id INTEGER NOT NULL REFERENCES projects(id),
  title      TEXT NOT NULL,
  body       TEXT NOT NULL DEFAULT '',   -- markdown; written as a dispatchable prompt
  priority   INTEGER NOT NULL DEFAULT 3 CHECK (priority BETWEEN 0 AND 3),  -- P0..P3
  state      TEXT NOT NULL DEFAULT 'proposed'
             CHECK (state IN ('proposed','parked','ready','running',
                              'needs-input','review','done','rejected')),
  agent      TEXT,                        -- optional override; NULL = resolve at dispatch
  question   TEXT,                        -- set iff state = 'needs-input'
  state_since TEXT NOT NULL,              -- ISO timestamp; drives the age bonus
  created_at  TEXT NOT NULL,
  closed_at   TEXT
);

CREATE TABLE deps (              -- taxonomy borrowed from beads
  task_id    INTEGER NOT NULL REFERENCES tasks(id),
  depends_on INTEGER NOT NULL REFERENCES tasks(id),
  kind       TEXT NOT NULL DEFAULT 'blocks'
             CHECK (kind IN ('blocks','discovered-from','parent','related')),
  PRIMARY KEY (task_id, depends_on)
);

CREATE TABLE sessions (
  id        INTEGER PRIMARY KEY,
  task_id   INTEGER NOT NULL REFERENCES tasks(id),
  agent     TEXT NOT NULL,               -- the agent actually used
  pid       INTEGER,
  log_path  TEXT,
  started_at TEXT NOT NULL,
  ended_at   TEXT,
  outcome    TEXT                        -- 'completed','asked','failed','capped','aborted'
);

CREATE TABLE events (            -- append-only audit of state transitions & answers
  id        INTEGER PRIMARY KEY,
  task_id   INTEGER REFERENCES tasks(id),
  at        TEXT NOT NULL,
  kind      TEXT NOT NULL,
  detail    TEXT
);
```

Ready-work detection — the thing beads would have provided — is one query: a task is *unblocked* when no `blocks` dependency points at a task not in `done`/`rejected`. Only `blocks` gates readiness; `discovered-from`, `parent`, and `related` are navigational metadata. When a task's last blocker closes, the store promotes it `parked → ready` and stamps `state_since`, which is what makes the prepared-prompt pattern work: tomorrow's task, written today and chained behind its blocker, surfaces fully loaded the moment it becomes actionable. Auto-promotion applies only to parked tasks that *have* blockers — a parked task with none is deliberately deferred and moves only by manual unpark. The reverse holds too: adding an open blocker to a `ready` task demotes it back to `parked` in the same write, and any state transition that would land a task in `ready` while a blocker is still open — triage, abort, manual unpark — is reconciled the same way. The invariant is therefore total: `ready` always means genuinely actionable, so the scheduler can hide blocked work by hiding `parked` alone. (This state was called `backlog` until migration 0002; the Kanban connotation — work you still intend to do soon — was exactly wrong for a state the scheduler ignores.)

Agent definitions live in a small config file rather than the database, since they are command templates, not state — `~/.config/voro/agents.toml`:

```toml
default = "claude"

[agents.claude]
cmd = "claude -p --output-format stream-json {prompt_file}"

[agents.codex]
cmd = "codex exec {prompt_file}"
```

## 6. Task state machine

| State | Meaning | Enters by | Leaves by |
|---|---|---|---|
| `proposed` | Suggested (often by an agent post-task); not yet triaged. In the queue for triage; never dispatched. | agent proposal, quick capture | human triage → `ready`/`parked`, or → `rejected` |
| `parked` | Triaged, real, but out of the running: dependencies open or deliberately deferred. Invisible to scheduler. | triage; dependency added | dependencies close → `ready`; manual unpark → `ready`; abandon → `rejected` |
| `ready` | Actionable now. Eligible for the queue's start rows and for `voro next`. | triage; last blocker closes | dispatch or manual start → `running`; park → `parked`; abandon → `rejected` |
| `running` | An agent (or the human) is actively on it. | dispatch | agent raises question → `needs-input`; work lands → `review`; cap/failure → `ready` (flagged for redispatch); abort → `ready` |
| `needs-input` | Blocked on a human decision; `question` is set. **First among equals in the queue.** | agent verb; manual flag | answer supplied → `running` (answer appended to the task body under an `## Answers` heading, logged as an event, and fed to the session); abandon → `rejected` |
| `review` | Agent believes it is done; awaiting human acceptance. | agent completion | accept → `done`; reject with feedback → `running`; abandon → `rejected` |
| `done` / `rejected` | Closed. `done` prompts triage of any `discovered-from` proposals. | acceptance / triage | — |

Three deliberate choices. Any triaged, non-terminal state can be *abandoned* straight to `rejected` — obsolete work must not need walking through the rest of the machine to close, and parking (`ready` → `parked`) has a manual inverse for the same reason. Second, `needs-input` and `review` are both human-attention states but are kept distinct because they sort differently: at equal score an unanswered question outranks a completed diff, which outranks startable work, which outranks an untriaged proposal — a question stalls in-flight work; a proposal's priority is agent-asserted and untrusted until triage, so it wins nothing but ties it deserves. Third, `proposed` exists precisely so agent-generated tasks can be captured freely without granting them anything: each proposed task rides the queue on the same score as everything else, visibly untriaged, and cannot be dispatched until a human triages it. Surfacing the tasks themselves rather than a count keeps the generation pipeline honest without automating it — triage is one keypress away.

## 7. Scoring

Start embarrassingly simple and only add terms that earn their place:

```
score(task) = project.weight × (priority_value(task) + state_bonus(task)) + age_bonus(task)

priority_value: P0 → 8, P1 → 4, P2 → 2, P3 → 1
state_bonus:    needs-input → 4, review → 2, ready → 0, proposed → 0
age_bonus:      0.1 × days_in_current_state, capped at 2
```

Project weight is an integer 0–5, where 0 means "parked — hide entirely" (this is how a project is snoozed without deleting anything). The geometric priority values ensure a P0 in a weight-2 project (16) still beats a P2 in a weight-5 project (10) — priorities within a project should mean something absolute, not just relative.

The `state_bonus` folds task state into the priority term rather than leaving it a pure tiebreaker, because the states are not just labels — they say *what a delay costs*. `needs-input` blocks an idle agent, so unblocking it keeps work flowing and earns the largest bonus; `review` only blocks a finished task from closing, so it earns half as much; `ready` and `proposed` earn nothing — startable work rides its own priority, and an untriaged proposal's priority is agent-asserted and untrusted, so it wins nothing but the ties its raw score already deserves. Folding the bonus *inside* the weight multiply (rather than adding a flat constant) keeps project weight the single master gain and the whole formula head-computable: a question is worth "one extra P1's worth" of priority in its own project, no more. The chosen magnitudes mean an attention item floats above routine startable work but a genuine emergency still wins — a P0 ready task (weight×8) still outranks a P2 question (weight×6) in the same project.

The age bonus is a gentle anti-starvation nudge so old P2s eventually surface, capped so it can never masquerade as a priority level. It applies uniformly to every scored state — `ready`, `needs-input`, `review`, and `proposed` alike — because a week-old unanswered question is a smell worth amplifying, not just an old task. Taskwarrior's experience suggests urgency formulae accrete coefficients until nobody trusts the number; resist that — this is the one and only additive state term. Any tuning should be observable via a score-decomposition view in the TUI (and later `voro explain <task>`).

Ordering of the queue: every `needs-input`, `review`, and `proposed` task plus the top three `ready` tasks, in one list sorted by score. Because the state bonus now lives in the score, state usually settles itself; the state precedence (§6) only breaks genuinely equal totals. `voro next` still answers with the single top ready task — that is what an agent asking for work should be handed, and the dispatch default.

## 8. Dispatch and agent integration

**Agent resolution**, in order: the task's `agent` override if set (the capability case — a task that needs image generation is inherently a task for the agent that has it, so the choice is worth persisting on the task); otherwise the global default from `agents.toml`. The TUI offers two dispatch actions: dispatch-with-resolved-agent on one key, dispatch-via-picker on another — the picker existing mostly for the cap case, where the default is temporarily unusable but nothing about the task has changed.

**Redispatch** is a first-class action born of the same cap case: a `running` task whose session ends with outcome `capped` or `failed` drops back to `ready` with a visible flag, and redispatching it offers the agent picker plus the previous session's notes/log so the successor agent does not start cold. The flag is not a column: it is read fresh from whether the task's most recent session ended `failed`/`capped`, since it is exactly the queue and task browser's business to render it, not the store's to keep it in sync.

**Observing the end of a session** is the other half of the loop, and has to answer a wrinkle: the `voro` invocation that dispatched a session may not outlive it — a one-shot `voro dispatch` returns immediately, and a TUI session watching it can simply be closed before the agent finishes. So there is no daemon or waiter; instead Voro reconciles on read. Every code path that consults live session or task state — `App::refresh` in the TUI, and every CLI verb — first calls a reconciler that walks `sessions` where `ended_at` is still null, and for each checks whether its `pid` is still alive (`kill -0`, run from the `voro` crate; `voro-core` never touches a process, it only takes the liveness result as a plain bool and decides what it means). A session whose process has exited is finalised:

- if its task is still `running`, the agent's process ended without calling `done` or `ask` — the session outcome is `capped` if the log tail matches a short list of known usage-limit phrases, else `failed`, and the task drops `running → ready` flagged for redispatch. This reuses the `Abort` transition (same `running → ready` target, same cascading readiness checks) but tags the event log with a distinct `reconcile` entry naming the outcome, so an automatic reconciliation reads differently from a human's `voro abort`.
- if its task already left `running` on its own — `done`/`ask` landed it in `review`/`needs-input` before the process exited, or a human moved it some other way — the session outcome reflects that instead (`completed`/`asked`/`aborted`, respectively) and the task is left untouched.

Usage-cap detection is deliberately trivial: a substring match over the last few KB of the log for phrases like "usage limit". It will miss agents that word it differently, in which case the session is reported `failed` rather than `capped` — a labelling gap, not a functional one, since both outcomes flag the task for redispatch identically. A dispatched process must also be reaped once it exits, or it sits as a zombie for the life of the spawning `voro` process — and `kill -0` on a zombie still reports it alive, which would silently defeat this whole mechanism in a long-lived TUI session. Dispatch therefore hands the child to a detached reaper thread the moment the session is recorded, rather than leaving it to `Drop`.

**The return path** is a small verb surface agents call from within their sessions, advertised to them via a CLAUDE.md/AGENTS.md snippet per project:

```
voro ask <task-id> --question "Schema A or B? Trade-offs: ..."   # → needs-input
voro done <task-id> --summary "Implemented X, tests pass"        # → review
voro propose <project> "title" --body-file plan.md               # → proposed (discovered-from)
```

Dispatch exports `VORO_TASK_ID` into the session's environment; when `propose` is called without `--from`, it uses that variable as the discovered-from source, so proposals made mid-session link back to the task that spawned them without the agent having to think about it. It also exports `VORO_DB` — the database the dispatching Voro is using — so the return-path verbs write back to the same store even when that store is not the default one (as under `--db`). `propose` always creates tasks in `proposed` — it cannot take a state.

The prompt is the task's title and body written to a file outside the checkout (so writing it never trips the dirty-tree guard), and the agent's command template — the `{prompt_file}` line from `agents.toml` — is run through `sh -c` in the project's path with the process detached into its own process group and its output captured to a per-session log. Spawning happens only after every check that can fail (task is `ready`, agent resolves, tree is clean, prompt writes); the `ready → running` transition and the session insert then land together in one transaction, so a running task always has a session and a session always names a live dispatch. A path that is not a git repository is refused, since its cleanliness cannot be verified.

Because these are plain CLI calls writing to a local SQLite file, they work identically for Claude Code, Codex, or anything that can run a shell command — no per-agent integration beyond the command template in `agents.toml`. An MCP server wrapping the same three verbs is a later nicety, not a requirement. Note these verbs are a thin second consumer of `voro-core`, not a prerequisite for the TUI — they arrive in the milestone that closes the agent loop.

Dispatch runs in the project's path with a dirty-tree guard in v1; per-dispatch worktrees are deferred until parallel dispatch within one project is actually wanted (§11).

## 9. Cockpit

The TUI is built first and is the primary interface throughout. Ratatui, three regions: the **queue** (top), a **detail** pane showing the full body of whichever row is selected (middle), and a **running** strip showing live sessions, their agents, and their states (bottom).

The first milestone deliberately restricts scope to three lists and a handful of keybindings — the risk of TUI-first is polishing panes before the workflow is validated, and the mitigation is scope, not sequence. Core interactions, roughly in order of implementation: create/edit a task in `$EDITOR` (title, body, priority, deps, agent override via frontmatter or a form); edit project weights in a one-line modal (*this must be fast — it happens every morning*); answer a queued question inline; dispatch a ready task (default agent) and dispatch-via-picker; accept/reject a review item; triage `proposed` tasks from the queue; redispatch a flagged task; a score-decomposition popup on any task.

Every action ultimately gets a CLI equivalent so the whole tool is scriptable and agent-legible, but the human-facing CLI trails the TUI rather than preceding it.

## 10. Delivery plan

Ordered by dependency and by time-to-useful, not by calendar — with agents doing the implementation, phases are checkpoints for *validation*, not estimates.

**Milestone A — usable command centre.** `voro-core` (schema, state machine, scheduler, scoring) plus the TUI with manual task management: create/edit tasks, weights modal, the queue and its detail pane, mark states by hand. No dispatch yet — dispatching is copy-the-body-into-Claude-Code by hand. This is already the tool you are missing: cross-project prioritised attention. Live in it immediately; everything after this is judged against real use.

**Milestone B — the loop.** Dispatch with agent resolution, the session table, the `voro ask/done/propose` verbs, CLAUDE.md snippets per project, needs-input flowing back into the queue, redispatch. The command centre now commands.

**Milestone C — refinement from usage.** Review UX (inline diff pane vs. open-in-Zed), triage ergonomics, GitHub issue *import* for owned repos, the human CLI surface, worktrees if parallel dispatch has become real.

**Milestone D — maybe.** GUI cockpit over the same core, richer session steering, two-way issue sync, smarter proposal triage.

## 11. Open questions

Worktrees per dispatch, or run in the main checkout? In-place with a dirty-tree guard until parallel dispatch within a single project is genuinely wanted. How does `review` get its diff in front of you — is opening the repo in Zed enough, or does the TUI need an inline diff pane (Vibe Kanban's strongest feature)? ~~Should the age bonus apply to `needs-input` items too?~~ — resolved: it applies uniformly to all scored states (§7). ~~How much session output should Voro retain — full logs per session, or just the tail plus the outcome?~~ — resolved for now: keep the full log at `log_path` indefinitely. At single-user scale a session's log is at most a few MB, disk is not the constraint, and the tail is already read back for usage-cap detection (§8) and would be for a redispatch's "previous session's notes/log" (§8) — trimming would have to be reversed the first time either needed more context. Revisit if logs ever get large enough (huge agent output, long-running sessions) that retention actually costs something. ~~And naming: `focus` collides with shell muscle memory for some?~~ — resolved: renamed to `voro` (unclaimed on crates.io, easy to type, no shell collision).

## 12. Risks

The tool becoming a procrastination object — mitigated by Milestone A's brutal scope: three lists, manual state, immediately lived-in. TUI-first building interface ahead of validated workflow — same mitigation; nothing in Milestone A's UI is speculative, every element maps to the queue from §1. Score distrust — mitigated by the decomposition view and by keeping the formula small enough to compute in your head. Schema regret — mitigated by the event log (replayable) and by SQLite migrations being cheap at single-user scale. And the standing risk of all personal tooling: that triage discipline decays. The `proposed` rows sitting visibly untriaged in the queue are the guard rail; if they stop working, that is signal about the design, not the user.
