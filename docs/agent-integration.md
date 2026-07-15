# Agent integration

This is the optional glue between Voro and Claude Code, the one agent with richer
integration points than "run a shell command." None of it is required — dispatch
works for any agent through a command template (DESIGN.md §8) — and none of it
lives in `voro-core` or the dispatch path. It is per-agent configuration you drop
into a project.

The **return path** is the few CLI verbs an agent calls from inside its session to
report what happened; DESIGN.md §8 covers why the surface is deliberately this
small. Hooks are a belt-and-braces layer under it, driven by Claude Code's own
lifecycle events for a session that forgets to call the verbs itself.

## The return path

Dispatch injects a preamble naming the verbs with the task's literal id already
substituted in (DESIGN.md §8), so a *dispatched* agent needs nothing from this
section. This file is for the other way to reach the verbs — an operator-pasted
snippet and Claude Code hooks — which read the task and database from
`VORO_TASK_ID` and `VORO_DB` (dispatch exports both) rather than from a rendered
command. That makes them best-effort under launch styles that do not propagate the
spawned process's environment, notably `claude --bg`; there the injected preamble
is the reliable path (DESIGN.md §8). Advertise the verbs by pasting this into the
project's `CLAUDE.md` (or `AGENTS.md`):

```markdown
## Reporting back to Voro

You were dispatched by Voro on task $VORO_TASK_ID. When you reach one of these
points, run the matching command — Voro surfaces it in the operator's queue:

    voro ask "$VORO_TASK_ID" --question "Schema A or B? Trade-offs: ..."
    voro done "$VORO_TASK_ID" --branch "$(git rev-parse --abbrev-ref HEAD)" --summary "Implemented X, tests pass"
    voro propose <project> "Follow-up title" --body-file plan.md

- `ask` when you are blocked on a human decision and cannot proceed.
- `done` when the work is complete and ready for review. Record **both** flags on
  the one call: `--branch` is the git branch your work landed on and `--summary`
  is a PR-ready account of what changed, why, and how you verified it — on a
  GitHub-reviewed project `voro pr` opens the pull request straight from them
  and needs both, on any project the summary is the review context, and a `done`
  that supplies only one leaves the task flagged `[incomplete report]`. Omit both only
  for a task that produced no code (planning, triage). If the task named an
  intended branch, you were told which one in the dispatch preamble — create or
  check it out yourself.
- `propose` to record follow-up work you noticed; it links back to this task.

`VORO_TASK_ID` and `VORO_DB` are already in your environment — do not set them.
```

`ask` moves the task to `needs-input`, `done` to `review`, and `propose` files a
`proposed` task discovered-from this one. See DESIGN.md §8 for why the surface is
this small.

## Session verbs: attachable dispatch

Background-only dispatch loses the ability to watch a session, jump in to steer,
or reopen it afterwards. Agents that ship their own session layer close that gap
through optional verbs on their `[agents.<name>]` table, beside the required
`dispatch` (`cmd` is accepted as an alias, so older configs load unchanged). The
`claude` and `codex` definitions below ship built-in — Voro compiles them in
(DESIGN.md §5), so you get exactly these without writing any `voro.toml`, and a
binary upgrade updates them. They are reproduced here to explain the verbs and to
show what you would copy to *override* one (a user table replaces a built-in
wholesale, so keep every verb you still want) or to model a new agent on; `voro
agent init` writes the same built-ins into a fresh `voro.toml`, commented out.

```toml
[agents.claude]
dispatch = "claude --bg --name \"voro-{task_id}\" --permission-mode auto --model opus \"$(cat {prompt_file})\""
sessions = "claude agents --json"
attach   = "claude attach {session}"
resume   = "claude --resume {session}"
plan     = "claude --permission-mode auto --model fable \"$(cat {prompt_file})\""

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
- `plan` runs an interactive *foreground* session for the TUI's agent-assisted
  task creation (DESIGN.md §8): `{prompt_file}` holds the planning brief, and
  the command owns the terminal until the conversation ends, so it must not
  background itself. It carries no `{session}` — a planning session belongs to
  no task and records no session row.

The two claude verbs pick different models for their different jobs:
`dispatch` runs `--model opus`, a workhorse for implementation, and `plan` runs
`--model fable`, a stronger reasoning model for interactive planning. These are
`claude` model *aliases*, not pinned model ids, so each resolves to the current
model of its class and does not churn as models are released. Want other models?
Override the agent wholesale in `voro.toml` — copy the block above and change
the `--model` flags (a user `[agents.claude]` table replaces the built-in
entirely, so keep every verb you still want).

The dispatch template runs Claude in `auto` mode: it auto-approves the actions it
judges safe — edits, a build, a commit — and pauses on genuinely risky ones, so
an unattended session keeps moving while the dangerous cases stay guarded. When it
pauses, the task stalls mid-run until you `attach` and answer. Set a different
`--permission-mode` to move that line — `bypassPermissions` never pauses (after a
one-time `claude --dangerously-skip-permissions` disclaimer that `--bg` requires),
`acceptEdits` auto-accepts edits but still blocks on every bash command — and use
`attach` to answer prompts in a live session.

Three behaviours hang off these verbs.

**Session-ref capture.** Launchers like `claude --bg` ignore any caller-chosen
session id, so dispatch captures the reference after the fact: it polls the
`sessions` listing for a session whose `cwd` is the project and whose `startedAt`
is at or after the spawn, falling back to the `backgrounded · <id>` line the
launcher prints into the session log. The ref is stored on the session row
(`session_ref`); if none shows up within a few seconds it stays NULL and the
dispatch summary says so.

**Liveness without pids.** A `--bg`-style launch is owned by a supervisor and its
spawned pid exits at once, so for agents with a `sessions` verb the reconciler
never pid-checks: a session is live while its ref appears in the listing
not-yet-`done`, and one that drops out or finishes there without calling `voro
done`/`ask` stalls its task, exactly as pid-death does for plain agents (DESIGN.md
§8). When liveness is unknowable (no ref, listing failed) the session is left
alone.

**Jump-in.** In the TUI, `a` on a running task runs the agent's `attach` command
with the TUI suspended — the real session, full control, including answering
permission prompts; on a review or stalled task it runs `resume` instead,
reopening the finished session. `answer` prefers the `continue` verb when it
exists and the session has a ref — the answer goes to the *same* session, context
intact — and otherwise spawns a fresh session re-sent the whole task body.

Every verb degrades gracefully when absent: no `attach`/`resume` disables the
jump-in key for that agent, no `sessions` keeps pid-liveness reconciliation, no
`continue` keeps fresh-spawn continuation, no `plan` turns the TUI's planning key
into a status line saying what to configure. An agent defining only
`dispatch`/`cmd` behaves exactly as before the verbs existed.

### tmux as a universal fallback

An agent with no session layer of its own can still be attachable by running
under tmux. Dispatch runs with `VORO_TASK_ID` exported, so the template can name
the tmux session deterministically, and `tmux list-sessions -F` can be dressed up
as a `sessions` listing (session name as the ref, `jq -s` to build the array):

```toml
[agents.myagent]
dispatch = "tmux new-session -d -s \"voro-$VORO_TASK_ID\" \"myagent run $(cat {prompt_file})\""
sessions = "tmux list-sessions -F '{\"sessionId\":\"#{session_name}\",\"cwd\":\"#{session_path}\",\"startedAt\":#{session_created}000,\"state\":\"working\"}' 2>/dev/null | jq -s ."
attach   = "tmux attach -t {session}"
```

A tmux session vanishes from `list-sessions` when its command exits, which is
exactly the drop-out the reconciler treats as finished-without-reporting — it
finalises the session and lands the task in `stalled` (DESIGN.md §8), where
redispatch is one key away. There is no honest `resume`/`continue` for a dead
tmux session, so leave those verbs off and let redispatch handle it.

## Hooks as a fallback

The return path depends on the agent remembering to call it. A session that does
the work and exits without calling `done` looks, to Voro, exactly like a crash:
the pid-liveness reconciler finds the process gone with the task still `running`,
marks the session `failed`, and lands the task in `stalled` (DESIGN.md §8). That
is safe but pessimistic — the work may have been finished and only the report
forgotten. Claude Code's
[lifecycle hooks](https://docs.claude.com/en/docs/claude-code/hooks) close that
gap by calling the verbs on the agent's behalf. Each hook runs as a subprocess of
the session, inheriting `VORO_TASK_ID` and `VORO_DB`, so it can address the right
task.

The hooks that matter here, and what each can honestly do:

| Hook | Fires when | Fallback | Value it adds |
|---|---|---|---|
| `SessionEnd` | the session terminates normally | `voro done --branch <current branch> [--summary <final message>]` if the task is still `running` | upgrades a forgotten `done` from a `failed` reconcile that would stall the task to a real `review` — the operator sees the diff instead of a redispatch row — recording the branch the work landed on and, best-effort, the session's final assistant message as the summary so the fallback lands a complete report rather than a summary-less one |
| `Notification` | Claude needs permission, or has idled waiting for input | `voro ask` with the notification message | the *only* signal for a session that is alive but stuck: its process is still running, so the pid-liveness reconciler never fires for it |
| `Stop` | the main agent finishes responding | same as `SessionEnd` | an earlier anchor for the same completion case; redundant with `SessionEnd` and optional |

Two honest limits shape this. There is no failure hook — a hard crash or a
usage-cap `SIGKILL` bypasses `SessionEnd`, so hard failure stays with the
reconciler by design (DESIGN.md §8) and the hooks only improve the graceful paths.
And `SessionEnd → done` is optimistic: it marks the task `review` assuming the
work is finished, which is wrong if the agent gave up mid-task. That costs little —
`review` is human-gated, so a false completion is one rejection from `running` —
but treat the hook as the net, not the plan, and prefer a real `done --summary`.

That summary is likewise best-effort: the hook lifts the session's final assistant
message out of the transcript Claude Code names in the payload. When it reads, the
task lands a complete report; when it can't (no `jq`, an unreadable transcript),
the task lands `review` flagged `[incomplete report]` (DESIGN.md §8), for the
operator or a resumed session to complete with `voro set <id> --summary`. Either
way the guarantee holds: a complete report or a visible anomaly, never a silent
gap. The final message is a genuine closing account, safe as a PR body, but not a
summary written on purpose — so a real `done --summary` is still preferred.

### Double transitions are already safe

Wiring the hooks cannot corrupt state. The transition API rejects any illegal
transition before it writes anything, so a hook's `voro done` after the agent has
already moved the task is a harmless no-op, and the hooks never inspect the task's
state before acting. This composes with the reconciler confluently — whichever
gets there first, the task ends in the same place. See DESIGN.md §8 for the full
argument.

## Sample configuration

Two things make this safe to leave installed:

- **Guard on `VORO_TASK_ID`.** Only a dispatched session has it set. Without the
  guard, these hooks in a user-level `~/.claude/settings.json` would fire
  `voro done` at the end of *every* ordinary interactive session. The guard makes
  them inert outside dispatch, so they are safe at any settings scope; putting
  them in the dispatched project's `.claude/settings.json` narrows them further.
- **Swallow the exit code.** A rejected transition exits non-zero; `|| true`
  keeps Claude Code from surfacing it to the operator as a failed hook.

Both fallbacks are fiddly enough to inline that they get a small wrapper script on
your `PATH` instead.

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
payload=$(cat)                             # SessionEnd JSON on stdin

branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)
[ "$branch" = HEAD ] && branch=            # detached HEAD: no branch to report

# Best-effort summary: the session's final assistant message, from the
# transcript Claude Code names in the payload. A real account of what the agent
# did, so it is safe as a PR body. If it can't be read, the summary is omitted
# and the task lands flagged [incomplete report] for the operator to complete.
summary=
if command -v jq >/dev/null 2>&1; then
  transcript=$(printf '%s' "$payload" | jq -r '.transcript_path // empty')
  if [ -n "$transcript" ] && [ -f "$transcript" ]; then
    summary=$(jq -rs '
      map(select(.type == "assistant")) | last | .message.content
      | (if type == "array" then map(select(.type == "text") | .text) | join("\n")
         else . end) // empty' "$transcript" 2>/dev/null)
  fi
fi

set --                                     # build argv, omitting empty flags
[ -n "$branch" ] && set -- "$@" --branch "$branch"
[ -n "$summary" ] && set -- "$@" --summary "$summary"
voro done "$VORO_TASK_ID" "$@" >/dev/null 2>&1 || true
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

The transition-rejection guarantee behind the double-transition safety is verified
in code (`voro-core`'s `full_transition_matrix` test, plus reading the `apply`
path: an illegal transition returns before any write and never commits). The
verb-to-state mapping and the `VORO_TASK_ID`/`VORO_DB` export are verified against
the dispatch and CLI source (DESIGN.md §8).

The hooks *firing* is verified against a live Claude Code session (v2.1.206): the
sample configuration above, driving a real session under a dispatched task's
environment. `SessionEnd` fires on a normal exit and upgrades a still-`running`
task to `review` while recording the branch; `Notification` fires when a live
session stalls on a permission prompt and reaches `voro ask` while the process is
still alive — the one path the reconciler cannot cover, and one that earns its
keep only under an attachable launch, since a headless `claude -p` auto-denies and
exits rather than stalling; `Stop` fires as the optional anchor; and with
`VORO_TASK_ID` unset the hooks exit before invoking `voro` at all. The one part
*not* yet live-verified is the summary extraction: the transcript's JSONL schema
(`.type == "assistant"`, `.message.content`) is assumed from Claude Code's format,
not confirmed against a captured file. It degrades safely to an `[incomplete
report]` flag, so treat that line as best-effort and re-confirm, with the rest, if
the hook contract moves.
