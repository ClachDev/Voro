---
name: voro-cli
description: Manage Voro tasks and projects from the command line — create/propose tasks, check the inbox or the next task, record questions and completion, transition states. Use whenever work in this repo should be tracked in Voro's own database (dogfooding), when asked what to work on, or when proposing follow-up work discovered during a task. Never modify the Voro database with raw SQL.
---

# Voro CLI

Voro is this repo's own product *and* its task tracker (it eats its own dog
food). All state lives in a single SQLite database; every mutation must go
through the CLI so the state machine and event log stay honest. **Never write
to the database with raw SQL or a sqlite client.**

## Invocation

From this repo: `cargo run -q -p voro -- <verb> [args]` (or `./target/debug/voro`
after a build; `voro` if installed on PATH).

Database resolution: `--db PATH` flag → `FOCUS_DB` env var → the real one at
`~/.local/share/voro/voro.db`. Tests and experiments must use `--db` with a
scratch path; only deliberate task management touches the real database.

`voro help` prints the full verb reference.

## Reading

```
voro inbox              # what needs the human: questions + reviews, by score
voro next               # the single top ready task, with its full body
voro list [--state ready] [--project NAME]
voro show <id>          # body, deps, event history
voro explain <id>       # score decomposition (weight × priority + age bonus)
```

## Writing

```
voro add <project> <title> --body-file plan.md [--priority 0-3] [--blocks 3,7]
voro set <id> [--title T] [--priority N] [--body-file F] [--blocks IDS]
voro project add <name> <path>
voro weight <project> <0-5>          # 0 parks/hides the project
```

`add` defaults to state `proposed`, which is what an agent should almost
always want: proposed tasks wait for human triage and never enter the queues
untriaged. Write the body as a **dispatchable prompt** — self-contained
enough that an agent could execute the task from the body alone (name files,
state the acceptance criteria).

## Transitions

States: `proposed → backlog|ready → running → needs-input|review → done/rejected`.
`blocks` deps gate readiness automatically: a backlog task promotes to ready
when its last blocker closes.

```
voro start <id>                      # ready → running (claim the task)
voro ask <id> --question "A or B?"   # running → needs-input (blocked on human)
voro answer <id> TEXT                # needs-input → running
voro done <id>                       # running → review (work finished, await human)
voro abort <id>                      # running → ready (backing out)
```

As an agent, your lifecycle is exactly: `start` when you begin, `ask` when
blocked on a decision, `done` when finished. **Do not** `triage`, `accept`,
`reject`, or `abandon` — closing the loop is the human's job, and proposed
tasks you create must be left proposed.
