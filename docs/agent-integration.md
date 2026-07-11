# Agent integration

This is the glue that lives *between* Voro and a coding agent, for the one agent
that has richer integration hooks than "run a shell command": Claude Code. None
of it is required — dispatch works for any agent through a command template,
built-in or from `voro.toml` (DESIGN.md §8) — and none of it lives in
`voro-core` or the dispatch path. It is per-agent configuration you drop into a
project.

Voro's boundary with an agent is task state versus session state (DESIGN.md §8):
Voro does not watch the process, it is *told* what happened. The telling is the
**return path** — a few CLI verbs the agent calls from inside its session. Hooks
are a belt-and-braces layer under that: a way for a Claude session that forgets
to call the verbs to still report, driven by Claude Code's own lifecycle events
rather than the agent's discipline.

## The return path

Dispatch injects a preamble at the top of every prompt it writes (DESIGN.md §8)
that names the verbs with the task's literal id already substituted in — so a
dispatched agent needs none of the setup in this section. This file is for the
*other* way an agent can reach the verbs: an operator-pasted snippet and Claude
Code hooks, both of which read the task and database from the environment rather
than from a rendered command.

Dispatch runs the agent in the project checkout with two environment variables
set (DESIGN.md §8): `VORO_TASK_ID`, the task being worked, and `VORO_DB`, the
database that dispatched it. A verb that reads them addresses the right task in
the right store without being told either. Note the injected preamble does *not*
rely on this — it renders the id and `--db <path>` into the commands directly,
precisely because some launch styles (notably `claude --bg`, which hands the
session to a supervisor daemon) do not propagate the spawned process's
environment to the session. The snippet and hooks below are best-effort under
those launch styles for the same reason; the injected preamble is the reliable
path. Advertise the verbs to the agent by pasting this into the project's
`CLAUDE.md` (or `AGENTS.md`):

```markdown
## Reporting back to Voro

You were dispatched by Voro on task $VORO_TASK_ID. When you reach one of these
points, run the matching command — Voro surfaces it in the operator's queue:

    voro ask "$VORO_TASK_ID" --question "Schema A or B? Trade-offs: ..."
    voro done "$VORO_TASK_ID" --branch "$(git rev-parse --abbrev-ref HEAD)"
    voro propose <project> "Follow-up title" --body-file plan.md

- `ask` when you are blocked on a human decision and cannot proceed.
- `done` when the work is complete and ready for review; `--branch` reports the
  git branch your work landed on so the task correlates with its PR (omit it if
  you did not work on a branch). If the task named an intended branch, you were
  told which one in the dispatch preamble — create or check it out yourself.
- `propose` to record follow-up work you noticed; it links back to this task.

`VORO_TASK_ID` and `VORO_DB` are already in your environment — do not set them.
```

`ask` moves the task `running → needs-input` (it becomes first among equals in
the queue); `done` moves it `running → review`; `propose` files a `proposed`
task discovered-from this one. That is the whole surface — see DESIGN.md §8 for
why it is deliberately this small.

## Session verbs: attachable dispatch

Background-only dispatch loses a lot against just running the agent
interactively: no live view, no way to jump in and steer, no way to reopen the
session afterwards. Agents that ship their own session layer close that gap
through optional verbs on their `[agents.<name>]` table, next to the required
`dispatch` (`cmd` is accepted as an alias, so older configs load unchanged).

The `claude` and `codex` definitions below **ship built-in** — Voro compiles
them in (DESIGN.md §5), so you get exactly these without writing any
`voro.toml`, and a binary upgrade updates them. They are reproduced here to
explain the verbs and to show what you would copy into a `voro.toml` table
to *override* one (a user table replaces a built-in wholesale, so keep every
verb you still want) or to model a new agent of your own on. `voro agent init`
writes the same built-ins into a fresh `voro.toml`, commented out and ready to
copy:

```toml
[agents.claude]
dispatch = "claude --bg --name \"voro-{task_id}\" --permission-mode auto \"$(cat {prompt_file})\""
sessions = "claude agents --json"
attach   = "claude attach {session}"
resume   = "claude --resume {session}"

[agents.codex]
dispatch = "codex exec \"$(cat {prompt_file})\""
resume   = "codex resume {session}"
continue = "codex exec resume {session} \"$(cat {prompt_file})\""
```

- `dispatch` may also carry `{task_id}`, replaced with the task's numeric id.
  It is optional — a template that omits it dispatches unchanged — and is used
  above to name the session `voro-<id>` (via Claude's `--name` flag) so it is
  identifiable in `claude agents` and the `/resume` picker. Agents with no
  session-naming flag simply leave it out.
- `sessions` prints the agent's sessions as a JSON array; Voro reads
  `sessionId` (or `id`), `cwd`, `startedAt` (ms epoch), and `state` (`"done"`
  once finished) from each object and ignores the rest.
- `attach` opens the *running* session interactively; `{session}` is replaced
  with the reference Voro captured at dispatch.
- `resume` reopens a *finished* session interactively.
- `continue` feeds a session new input headless — `{prompt_file}` holds the
  input (an answer), `{session}` addresses the session.

The dispatch template above runs Claude in `auto` mode: a dispatched session is
unattended, and `auto` auto-approves the actions it judges safe — edits, a cargo
build, a git commit — while still pausing on genuinely risky ones, so the queue
keeps moving without a human but the dangerous cases are still guarded. When it
does pause, the session parks until you `attach` and answer, so a task can stall
mid-run rather than sail through unchecked. Dispatch already refuses a dirty tree
and the agent's work lands as a reviewable diff, so most vetting happens at
review rather than per-command. Two alternatives trade differently:
`bypassPermissions` never pauses at all — nothing strands the task — but requires
accepting a one-time disclaimer (`claude --dangerously-skip-permissions`) before
`--bg` will start; `acceptEdits` auto-accepts edits yet still blocks on every
bash command. Set whichever `--permission-mode` matches how much you want to
approve by hand, and use `attach` to answer prompts in a live session.

Three behaviours hang off these verbs.

**Session-ref capture.** Launchers like `claude --bg` ignore any caller-chosen
session id, so the reference has to be captured after the fact: dispatch polls
the `sessions` listing for a session whose `cwd` is the project and whose
`startedAt` is at or after the spawn, falling back to the ANSI-coloured
`backgrounded · <id>` line the launcher prints (it lands in the session log).
The captured ref is stored on the session row (`session_ref`) and printed in
the dispatch summary; if nothing shows up within a few seconds the ref stays
NULL and the summary says so.

**Liveness without pids.** A `--bg`-style launch is owned by a supervisor: the
pid Voro spawned exits as soon as the launcher returns, so pid-checking would
declare the dispatch dead at birth. For agents with a `sessions` verb, the
reconciler therefore never pid-checks — a session is live while its ref
appears in the listing not-yet-`done`; a session that drops out of the listing
or finishes there without having called `voro done`/`ask` is flagged for
redispatch, exactly as pid-death is for plain agents. When liveness is
unknowable (no ref captured, listing failed) the session is left alone rather
than guessed at.

**Jump-in.** In the TUI, `a` on a running task runs the agent's `attach`
command in the project checkout with the TUI suspended — the real session,
full control, including answering permission prompts (which also means the
`--allowedTools` allowlist in a dispatch template can shrink once attach is
wired). Detaching returns to Voro. On a review or redispatch-flagged task the
same key runs `resume`, reopening the finished session with its history.
`answer` prefers the `continue` verb when it exists and the session has a ref
— the answer alone goes to the *same* session, context intact — and otherwise
falls back to spawning a fresh session re-sent the whole task body.

Every verb degrades gracefully when absent: no `attach`/`resume` disables the
jump-in key for that agent, no `sessions` keeps pid-liveness reconciliation,
no `continue` keeps fresh-spawn continuation. An agent defining only
`dispatch`/`cmd` behaves exactly as before the verbs existed.

### tmux as a universal fallback

An agent with no session layer of its own can still be attachable by running
under tmux. Dispatch runs with `VORO_TASK_ID` exported, so the template can
name the tmux session deterministically, and `tmux list-sessions -F` can be
dressed up as a `sessions` listing (session name as the ref, `jq -s` to build
the array):

```toml
[agents.myagent]
dispatch = "tmux new-session -d -s \"voro-$VORO_TASK_ID\" \"myagent run $(cat {prompt_file})\""
sessions = "tmux list-sessions -F '{\"sessionId\":\"#{session_name}\",\"cwd\":\"#{session_path}\",\"startedAt\":#{session_created}000,\"state\":\"working\"}' 2>/dev/null | jq -s ."
attach   = "tmux attach -t {session}"
```

A tmux session vanishes from `list-sessions` when its command exits, which is
exactly the drop-out the reconciler treats as finished-without-reporting — it
finalises the session but leaves the task `running` for the operator to handle
(DESIGN.md §8). There is no honest `resume`/`continue` for a dead tmux session,
so leave those verbs off and let an explicit abort/redispatch handle it.

## Hooks as a fallback

The return path depends on the agent remembering to call it. A session that does
the work and exits without calling `done` is indistinguishable, to Voro, from one
that crashed: the pid-liveness reconciler (DESIGN.md §8) finds the process gone
with the task still `running` and marks the session `failed`, but leaves the task
`running` — surfaced as an orphan for the operator to redispatch, accept, or
abort. That is the safe default, but it is manual and pessimistic — the work may
have been finished and only the report forgotten.

Claude Code fires [lifecycle hooks](https://docs.claude.com/en/docs/claude-code/hooks)
that can close that gap by calling the verbs on the agent's behalf. Each hook
runs as a subprocess of the session, so it inherits `VORO_TASK_ID` and `VORO_DB`
and has everything it needs to address the right task.

The hooks that matter here, and what each can honestly do:

| Hook | Fires when | Fallback | Value it adds |
|---|---|---|---|
| `SessionEnd` | the session terminates normally | `voro done --branch <current branch>` if the task is still `running` | upgrades a forgotten `done` from a `failed` reconcile that leaves the task a stalled `running` orphan to a real `review` — the operator sees the diff instead of having to chase down an orphan — and records the branch the work landed on (task #81) |
| `Notification` | Claude needs permission, or has idled waiting for input | `voro ask` with the notification message | the *only* signal for a session that is alive but stuck: its process is still running, so the pid-liveness reconciler never fires for it |
| `Stop` | the main agent finishes responding | same as `SessionEnd` | an earlier anchor for the same completion case; redundant with `SessionEnd` and optional |

Two honest limits shape this.

**There is no failure hook.** A hard crash or a usage-cap `SIGKILL` bypasses
`SessionEnd` entirely, and no Claude Code hook cleanly signals "this agent
failed." So the fallback deliberately does *not* try to synthesise a `failed`
outcome — that case stays with the pid-liveness reconciler, which already labels
it `failed`/`capped` and leaves the task a `running` orphan for the operator to
redispatch (DESIGN.md §8). The hooks only ever improve the *graceful* paths.

**`SessionEnd → done` is optimistic.** It marks the task `review` on the
assumption the work is finished, which is wrong if the agent gave up mid-task and
merely finished talking about it. That costs little: `review` is human-gated, so
a false completion is one rejection away from going back to `running`, and it
routes the diff to the operator's eyes rather than leaving a stalled orphan to
chase down. Prefer
that the agent call `done` itself with a real summary; treat the hook as the net,
not the plan.

### Double transitions are already safe

The obvious worry is a double transition: the agent calls `done`, *then* the
`SessionEnd` hook fires `voro done` again. It is a non-issue. Voro's transition
API rejects any illegal transition before it writes anything — no state change,
no event, the transaction never commits (`voro-core`'s `apply`; the
`full_transition_matrix` test pins every rejected pair). Once the agent has moved
the task to `review` or `needs-input`, a second `voro done` from a hook is
rejected and leaves the task exactly as it was.

So the hooks never need to inspect the task's current state before acting — the
rejection is all the protection required. This composes with the reconciler for
the same reason: whichever of the hook and the reconciler reaches the task first
wins, and the other finds a task that has already left `running` and no-ops (the
reconciler records the session `completed` and leaves the task alone, DESIGN.md
§8). Wiring the hooks cannot corrupt state; the worst case is a harmless rejected
command.

## Sample configuration

Two things make this safe to leave installed:

- **Guard on `VORO_TASK_ID`.** Only a dispatched session has it set. Without the
  guard, these hooks in a user-level `~/.claude/settings.json` would fire
  `voro done` at the end of *every* ordinary interactive session. The guard makes
  them inert outside dispatch, so they are safe at any settings scope; putting
  them in the dispatched project's `.claude/settings.json` narrows them further.
- **Swallow the exit code.** A rejected transition exits non-zero; `|| true`
  keeps Claude Code from surfacing it to the operator as a failed hook.

Both fallbacks get a small wrapper script on your `PATH`. The `SessionEnd`
fallback needs to read the checkout's current branch (guarding an empty or
detached HEAD so it never records a blank name), and the `Notification` fallback
reads the hook's JSON from stdin to lift out the message — both fiddly to inline.

`.claude/settings.json`:

```json
{
  "hooks": {
    "SessionEnd": [
      {
        "hooks": [
          { "type": "command", "command": "voro-done-hook" }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          { "type": "command", "command": "voro-notify-hook" }
        ]
      }
    ]
  }
}
```

`voro-done-hook` (make it executable, put it on `PATH`):

```sh
#!/bin/sh
# Claude Code SessionEnd hook -> voro done, for a forgotten completion.
[ -n "$VORO_TASK_ID" ] || exit 0           # inert outside a dispatched session
branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)
[ "$branch" = HEAD ] && branch=            # detached HEAD: no branch to report
if [ -n "$branch" ]; then
  voro done "$VORO_TASK_ID" --branch "$branch" >/dev/null 2>&1 || true
else
  voro done "$VORO_TASK_ID" >/dev/null 2>&1 || true
fi
```

`voro-notify-hook` (make it executable, put it on `PATH`):

```sh
#!/bin/sh
# Claude Code Notification hook -> voro ask, for a stuck-but-alive session.
[ -n "$VORO_TASK_ID" ] || exit 0          # inert outside a dispatched session
payload=$(cat)
if command -v jq >/dev/null 2>&1; then
  message=$(printf '%s' "$payload" | jq -r '.message // empty')
fi
[ -n "$message" ] || message="agent signalled it needs input"
voro ask "$VORO_TASK_ID" --question "$message" >/dev/null 2>&1 || true
```

`Stop` can be wired identically to `SessionEnd` if you want the earlier anchor,
but it adds nothing once `SessionEnd` is in place.

## What this is verified against

The transition-rejection guarantee the double-transition safety rests on is
verified in code (`voro-core`'s `full_transition_matrix` test, plus reading the
`apply` path: an illegal transition returns before any write and never commits).
The verb-to-state mapping and the `VORO_TASK_ID`/`VORO_DB` export are verified
against the dispatch and CLI source (DESIGN.md §8; `voro done` takes an optional
`--summary TEXT` and `--branch NAME`).

The *firing* of the hooks is verified against a live Claude Code session
(v2.1.206): the sample configuration above, installed in a scratch project with a
scratch `--db`, driving a real session under a dispatched task's environment. The
event names and the `message` extraction match Claude Code's current hook
contract, and no correction to the snippet was needed.

`SessionEnd` fires when the session ends (`hook_event_name` `"SessionEnd"`,
`reason` `"other"` on a normal `-p` exit), and its `voro-done-hook` upgrades a
still-`running` task to `review` while recording the current branch — the
forgotten completion is caught. `Notification` fires the moment a live session
stalls on a permission prompt, with `hook_event_name` `"Notification"` and
`message` `"Claude needs your permission"` (alongside a `notification_type`
`"permission_prompt"` the hook ignores); the `voro-notify-hook`'s `jq -r '.message
// empty'` lifts that string and `voro ask` records it while the process is still
alive and waiting — the one path the pid-liveness reconciler cannot cover. That
Notification is an interactive-session signal, though: a headless `claude -p` run
auto-denies a permission-gated tool and exits rather than stalling, so the
Notification fallback only earns its keep under an attachable launch (`claude
--bg`, the dispatch template's default). `Stop` fires too (`hook_event_name`
`"Stop"`, `stop_hook_active` `false`), confirming the optional earlier anchor is
there. Finally the `VORO_TASK_ID` guard holds: with the variable unset, an
ordinary session's hooks exit before invoking `voro` at all, leaving a canary
`running` task untouched. Re-confirm against a live session if Claude Code's hook
contract moves.
