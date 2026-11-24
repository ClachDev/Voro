---
name: voro-cli
description: Record your work in Voro from the command line — claim a task, ask a blocking question, report it done, read the queue, and file follow-up proposals. Use when you were dispatched on or asked to work on a Voro task, when asked what to work on next, or when you discover follow-up work mid-task. Never modify the Voro database with raw SQL.
---

# Voro CLI

Voro is the operator's task queue across many projects. Change it only
through the `voro` CLI; **never write to its database with raw SQL or a sqlite
client.**

## Invocation

**Always pass a verb.** A bare `voro` launches the interactive TUI, which hangs
a headless session.

Run `voro` from your `PATH`. In a checkout of Voro itself with no binary
installed, run `cargo run -q -p voro -- <verb> [args]` instead. `voro help`
prints the full verb reference.

The CLI writes to `--db PATH`, else `$VORO_DB`, else the operator's real
database. Pass `--db` with a scratch path for any test or experiment.

## What is yours and what is the operator's

You run your own lifecycle (`start`, `ask`, `resume`, `done`, and `abort` when
backing out), read anything, file proposals (`propose`, `add`), and edit your
own task (`set`). Leave every other verb to the operator, because verdicts and
side effects are theirs: the verdicts `triage`, `accept`, `reject` and
`abandon`; the hand-offs `wait`, `reclaim`, `park` and `unpark`; the
configuration verbs `project`, `weight`, `repo`, `doc`, `intent`, `agent` and
`viewer`, bar the reads `repo list`, `doc list`, `doc show` and `intent show`;
and `dispatch`, `pr`, `open`, `import`, `migrate` and `seed`, which reach
outside the database.

## Lifecycle

```
voro start <id>                      # ready → running: claim the task
voro ask <id> --question "A or B?"   # running → needs-input
voro resume <id>                     # needs-input → running
voro done <id> --summary-file pr.md --branch <name>   # running → review
voro abort <id>                      # running → ready: back out
```

When asked to work on a task, run `voro start <id>` before anything else; if
the id is unclear, ask. A dispatched task is already running, so skip `start`.

After `voro ask`, stop and wait. The operator answers in your own session and
Voro records no answer text, so once you have the answer, run `voro resume
<id>` yourself and carry on.

`done`'s summary (`--summary TEXT` or `--summary-file PATH`) becomes the pull
request's description: write what changed and why, then how you verified it.
`--branch` names the branch your work is committed on. After `done`, tell the
operator the task is in review; `voro pr`, `voro open` and `voro accept` are
theirs.

## Reading

```
voro inbox              # the next-action queue, ranked by score
voro next               # the single top ready task, with its body
voro list [--state STATE] [--project P] [--doc DOC]
voro show <id>          # body, docs, deps, event history
voro show <id> --event <event-id>   # one event's detail, e.g. a lost body
voro tree <id>          # the task and its blockers, nested
voro explain <id>       # score decomposition
voro milestones         # open milestones and their open task counts
voro doc list [project] # registered documents
voro doc show <doc>     # a document and every task linked to it
voro intent show <project>   # why the project exists, in priority order
```

Dispatch names a task's linked documents by location; read the file itself.
`voro doc show` lists the other tasks derived from the same plan.

## Which project and repo is this checkout?

A project is one queue of work; a repo is a checkout its tasks run in. A task
runs in the project's default repo unless it names another with `--repo`.

```
voro repo list          # project, repo name, path; * marks each default
```

Match your cwd (or its enclosing repo root) to a listed path. That row's
project name is what `add`, `propose` and `list --project` expect. **If the
matched repo is not its project's default (no `*`), pass `--repo <name>` on
`add`**, or the task dispatches into the default checkout, which is the wrong
one. `propose` takes no `--repo`: name the repo in the body instead.

If no listed path contains your cwd, the checkout is unregistered. Do not
register it. Say to the operator: "This checkout (`<path>`) is not registered
in Voro; add it as a repo of the project it belongs to, then I can file the
task."

## Filing work

```
voro propose <project> "<title>" --body-file plan.md [--from <id>]
             [--blocks IDS]
voro add <project> "<title>" --body-file plan.md [--priority 0-3]
         [--blocked-by IDS] [--blocks IDS] [--repo NAME] [--deep]
```

Both verbs leave the task `proposed`; never pass `add` a `--state`. A
proposal waits for the operator's triage, so tell the operator what you filed
and its id, and do not chase it. `--from <id>` links the proposal
discovered-from the task you are working on.

`--blocked-by 3,7` makes the new task wait on tasks 3 and 7; `--blocks 9`
makes task 9 wait on it. `--deep` asks for the agent's strongest model.

Write the body as a dispatchable prompt. The agent starts in the project
checkout, so its harness loads the repo's agent instructions, and Voro also
gives it: how to report back, the branch and worktree rules, the linked
documents by location, the milestones the task counts toward, and a notice when
the task was sent back from review. It gets nothing from your session and
nothing from the tasks this one waits on.

The body states what is specific to this task: the outcome, the files, the
decisions already made, and concrete acceptance criteria. It does not restate
the repo's working rules (those belong in the repo's agent instructions), the
plan (link the document), or how to report back.

## Editing your own task

```
voro set <id> --append-body-file notes.md   # add a finding to the body
voro set <id> --branch <name>               # record your branch
voro set <id> --summary-file pr.md          # rewrite the summary in review
voro set <id> --blocked-by IDS | --blocks IDS | --unlink KIND:ID
```

`--body-file` on `set` replaces the **whole** body; use `--append-body-file`
to add to it. `--blocked-by` replaces the task's blocker list; `--blocks` adds
the task to the listed tasks' blockers. `--unlink` drops one edge as `show`
names it, such as `blocks:9`.

## Milestones

A milestone is an outcome the operator watches happen, such as "Carpet
crossing"; a fix or a refactor is a task. Before you file a task, run `voro
milestones` and `voro show` the one it belongs to, so you do not duplicate a
member. Pass `--blocks <milestone id>` if the milestone cannot pass without the
task, and say why in one sentence of the body.

Propose a new milestone (`propose ... --milestone`) only if no listed one
covers the outcome. Give it a title of four words or fewer and a body that
states, in measurable terms, what the operator will watch happen.
