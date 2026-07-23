---
name: voro-cli
description: Track work in Voro from the command line — check the inbox or the next task, create and propose tasks, record questions and completion, and transition task states across any project. Use whenever the current work should be tracked in Voro, when asked what to work on next, or when proposing follow-up work discovered mid-task. Never modify the Voro database with raw SQL.
---

# Voro CLI

Voro is a single-operator command centre that prioritises tasks across many
projects and dispatches them to coding agents. All state lives in one SQLite
database; every mutation must go through the `voro` CLI so the state machine and
event log stay honest. **Never write to the database with raw SQL or a sqlite
client.**

## Invocation

Run `voro` directly — it is expected on your `PATH`.

Database resolution: `--db PATH` flag → `VORO_DB` env var → the real one at
`~/.local/share/voro/voro.db`. Tests and experiments must use `--db` with a
scratch path; only deliberate task management touches the real database.

`voro help` prints the full verb reference.

## Reading

```
voro inbox              # the next-action queue: questions, reviews, proposals, top ready tasks — one list by score
voro next               # the single top ready task, with its full body
voro list [--state ready] [--project NAME]
voro show <id>          # body, deps, event history
voro explain <id>       # score decomposition (weight × priority + age bonus)
```

## Writing

```
voro add <project> <title> --body-file plan.md [--priority 0-3]
         [--blocked-by 3,7] [--blocks 9]
voro set <id> [--title T] [--priority N] [--body-file F]
              [--blocked-by IDS] [--blocks IDS]
              [--branch NAME]        # intended git branch dispatch injects
voro project add <name> <path>
voro weight <project> <0-5>          # 0 parks/hides the project
```

Dependency flags mean exactly what they say: `--blocked-by 3,7` makes the
task wait on tasks 3 and 7; `--blocks 9` makes task 9 wait on this one (the
discovered-prerequisite pattern). On `set`, `--blocked-by` *replaces* the
task's own blocker list while `--blocks` is *additive*. Both directions echo
their effect and are cycle-checked; `show` renders the edge as `blocked by #N`.

`add` defaults to state `proposed`, which is what an agent should almost
always want: proposed tasks wait for human triage and never enter the queues
untriaged. Write the body as a **dispatchable prompt** — self-contained
enough that an agent could execute the task from the body alone (name files,
state the acceptance criteria).

## Working from an arbitrary project directory

Voro tracks projects by name and checkout path, independent of where you invoke
the CLI. To find which registered project the current directory belongs to,
list the projects and match your cwd against their paths:

```
voro project list       # id, weight, name, path — one line per project
```

Match the cwd (or its enclosing repo root) to a listed path; that project's
name is what `add`, `propose`, `list --project`, and `weight` expect. If no
listed path contains the cwd, the project is unregistered — add it with
`voro project add <name> <path>` (pointing at the checkout root) before filing
tasks against it.

Propose follow-up work discovered here against the matched project:

```
voro propose <project> <title> --body-file plan.md [--from <task-id>]
```

`propose` creates a `proposed` task; `--from` links it discovered-from the task
you were working on (it defaults to `$VORO_TASK_ID` when set).

## Transitions

States: `proposed → parked|ready → running → needs-input|review → done/rejected`.
`blocks` deps gate readiness automatically: a parked task promotes to ready
when its last blocker closes.

```
voro start <id>                      # ready → running (claim the task)
voro ask <id> --question "A or B?"   # running → needs-input (blocked on human)
voro answer <id> TEXT                # needs-input → running
voro done <id> [--branch NAME]       # running → review; --branch records the
                                     #   git branch your work landed on
voro abort <id>                      # running → ready (backing out)
```

**When you are asked to work on a task, run `voro start <id>` before you do
anything else.** This claims the task and moves it to `running`, so the queue
reflects that it is being worked rather than still waiting. The task id is in
the request (e.g. "implement task 35"); if it is unclear, ask.

Your lifecycle as an agent is exactly: `start` when you begin, `ask` when
blocked on a decision, `done` when finished. **Do not** `triage`, `accept`,
`reject`, or `abandon` — closing the loop is the human's job, and proposed
tasks you create must be left proposed.

## Working inside the Voro repository itself

When you are in a checkout of Voro with no `voro` binary installed,
`cargo run -q -p voro -- <verb> [args]` runs the CLI straight from source.
