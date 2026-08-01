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
         [--blocked-by 3,7] [--blocks 9] [--repo NAME] [--deep]
voro set <id> [--title T] [--priority N] [--body-file F]
              [--blocked-by IDS] [--blocks IDS] [--unlink KIND:ID]
              [--branch NAME]        # intended git branch dispatch injects
              [--repo NAME | --no-repo]   # which checkout the task runs in
              [--deep | --no-deep]   # dispatch on the agent's strongest model
voro project add <name> <path>
voro weight <project> <0-5>          # 0 parks/hides the project
```

Dependency flags mean exactly what they say: `--blocked-by 3,7` makes the
task wait on tasks 3 and 7; `--blocks 9` makes task 9 wait on this one (the
discovered-prerequisite pattern). On `set`, `--blocked-by` *replaces* the
task's own blocker list while `--blocks` is *additive*. Both directions echo
their effect and are cycle-checked; `show` renders the edge as `blocked by #N`.

`--unlink KIND:ID` drops a single edge of any kind — `blocks:9`,
`discovered-from:4`, `parent:2`, `related:7` — naming it as `show` lists it
under that task, and leaves any other edge to the same task standing. Repeat
the flag for several. Naming an edge that is not there fails rather than
reporting a success it did not perform.

`add` defaults to state `proposed`, which is what an agent should almost
always want: proposed tasks wait for human triage and never enter the queues
untriaged. Write the body as a **dispatchable prompt** — self-contained
enough that an agent could execute the task from the body alone (name files,
state the acceptance criteria).

`--deep` marks work that warrants the strongest model the dispatching agent
offers rather than its workhorse. It changes only which model runs the task,
never its place in the queue — that is priority's job — and an agent whose
config names no models ignores it. It is refused on a `--human` task, which is
never dispatched at all.

## Projects and repos

A **project** allocates attention (name, weight, one queue); a **repo** locates
a checkout. A project owns one or more repos, exactly one of which is its
default, and a task runs in the repo it names — or the project's default when
it names none. One stream of work spanning several repositories is therefore
*one* project with several repos, not several projects.

```
voro repo list [project]              # every repo; * marks each project's default
voro repo add <project> <name> <path>
voro repo path <project> <name> <path>
voro repo default <project> <name>    # the repo tasks that name none run in
voro repo remove <project> <name>
```

`repo remove` refuses a project's last repo, its default while others remain,
and one any task still names — the message says which and what to do first.

## Working from an arbitrary project directory

Voro tracks checkouts by repo, independent of where you invoke the CLI. To find
which registered project *and repo* the current directory belongs to, list the
repos and match your cwd against their paths:

```
voro repo list          # project, repo name, path — one line per repo
```

Match the cwd (or its enclosing repo root) to a listed path. That row's project
name is what `add`, `propose`, `list --project`, and `weight` expect, and its
repo name is what `--repo` expects. **If the matched repo is not the project's
default (no `*`), pass `--repo <name>` when filing the task** — otherwise it
will dispatch into the project's default checkout, which is the wrong one. If
no listed path contains the cwd, the checkout is unregistered: add it to the
project it belongs to with `voro repo add <project> <name> <path>`, or register
a whole new project with `voro project add <name> <path>` if it is genuinely a
separate stream of work.

Propose follow-up work discovered here against the matched project:

```
voro propose <project> <title> --body-file plan.md [--from <task-id>]
```

`propose` creates a `proposed` task; `--from` links it discovered-from the task
you were working on (dispatch renders the flag with the running task's id).
`propose` takes no `--repo` — a proposal is untriaged, so name the repo in the
body and let triage set it with `voro set <id> --repo NAME`.

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
