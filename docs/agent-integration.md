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
    voro resume "$VORO_TASK_ID"
    voro done "$VORO_TASK_ID" --branch "$(git rev-parse --abbrev-ref HEAD)" \
        --summary "<what changed and why, then how you verified it — a PR description>"
    voro propose <project> "Follow-up title" --from "$VORO_TASK_ID" \
        --body-file plan.md

- `ask` when you are blocked on a human decision and cannot proceed.
- `resume` once that question is answered here in this session, to move the task
  back to `running` and carry on — Voro records no answer text, the exchange is
  already in this transcript.
- `done` when the work is complete and ready for review. Record **both** flags on
  the one call: `--branch` is the git branch your work landed on and `--summary`
  is the pull request's description — what changed and why, then how you
  verified it, written as a PR body rather than a status line. On a
  GitHub-reviewed project `voro pr` opens the pull request with that summary as
  its body and needs both, on any project the summary is the review context, and
  a `done` that supplies only one leaves the task flagged `[incomplete report]`.
  Omit both only for a task that produced no code (planning, triage). If the
  task named an intended branch, you were told which one in the dispatch
  preamble — create or check it out yourself.
- `propose` to record follow-up work you noticed; `--from "$VORO_TASK_ID"` links
  it back to this task (`voro` reads no environment on its own — pass the id, as
  every verb here does).

`VORO_TASK_ID` and `VORO_DB` are already in your environment — do not set them.
```

`ask` moves the task to `needs-input`, `resume` back to `running`, `done` to
`review`, and `propose` files a `proposed` task discovered-from this one. See
DESIGN.md §8 for why the surface is this small.

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
dispatch   = "claude --bg --name \"{session_name}\" --permission-mode auto --model {model} \"$(cat {prompt_file})\""
sessions   = "claude agents --json"
attach     = "claude attach {session}"
resume     = "claude --resume {session}"
message    = "claude -p --resume {session} --permission-mode auto \"$(cat {prompt_file})\""
logs       = "claude logs \"$(printf %.8s {session})\" 2>/dev/null | tail -c 20000"
stop       = "claude stop \"$(printf %.8s {session})\""
plan       = "claude --name \"{session_name}\" --permission-mode auto --model {model} \"$(cat {prompt_file})\""
model      = "opus"
model_deep = "fable"
model_plan = "fable"

[agents.codex]
dispatch = "codex exec \"$(cat {prompt_file})\""
resume   = "codex resume {session}"
```

- `dispatch` and `plan` may also carry `{session_name}`, replaced with the name
  Voro composes for the session that launch opens. It is used above with
  Claude's `--name` flag so every session Voro starts is identifiable in
  `claude agents` and the `/resume` picker; agents with no session-naming flag
  simply leave it out. The scheme is `voro-<id>-<slug>` for a dispatch of task
  `<id>`, where the slug is the first whole words of the task's title —
  lowercased, and stopping before it runs past about twenty characters, since
  that listing and the phone's both truncate; `voro-<id>-refine` for a refine
  of it (DESIGN.md §6); and `voro-plan-<project>` for a planning session and
  `voro-propose-<project>` for a quick propose, neither of which belongs to a
  task and so both are named for their project — by name, so the bare
  number in a Voro-composed session name is always a task id. Project names are
  unique, and any character outside `[A-Za-z0-9._-]` is replaced with `-`
  before the name is used; a title that survives none of that leaves the bare
  `voro-<id>`. Every name still opens `voro-<id>`, which is the stable part of
  the contract — anything else pointed at the same task suffixes a kind rather
  than colliding with it, and a dispatch whose slug would be exactly such a
  kind takes another word instead.
- `dispatch` may also carry `{task_id}`, replaced with the task's numeric id,
  for a template that wants the id somewhere other than the session name. It is
  optional — a template that omits it dispatches unchanged — and it is refused
  on `plan`, which serves a target that has no task id to bind, and on the
  session verbs, which name their session with `{session}`.
- `sessions` prints the agent's sessions as a JSON array; Voro reads
  `sessionId` (or `id`), `cwd`, `startedAt` (ms epoch), `state` (`"done"` once
  finished, `"working"` while going) and `pid` from each object and ignores the
  rest. `state` and `pid` are what say whether an entry is still live, and both
  are optional individually — but an entry carrying neither claims nothing and
  reads as dead, so a listing should emit one or the other for every entry it
  wants Voro to see as running (see *Liveness without pids* below).
- `attach` opens the *running* session interactively; `{session}` is replaced
  with the reference Voro captured at dispatch.
- `resume` reopens a *finished* session interactively.
- `message` says one thing into a session *headlessly*: it takes both
  `{session}` and `{prompt_file}`, appends the file's contents to that session's
  transcript as the next turn, and returns without owning the terminal. It is
  the only session verb Voro backgrounds. Voro watches the spawned command for
  about two seconds and treats an exit with a non-zero status in that window as
  a message that was never delivered: nothing is transitioned, the task stays
  where it was, and the command's last log line is quoted on the status line. A
  send still running past the window — or one that finished cleanly inside it —
  has landed, and from there it is fire-and-forget: Voro reads no reply, so
  anything the agent says afterwards is in the launch's log rather than the UI.
  A rejection's `review → running` transition (DESIGN.md §6) hangs off that
  confirmation, so feedback is never recorded against a message the agent
  refused. `codex` names no `message`, so the quick-message key reports that on
  the status line and the jump-in still works.
- `message` may also carry `{new_session}`, replaced with a fresh v4 UUID Voro
  generates for the send. Use it when your agent's sessions cannot be resumed
  headlessly but can be *forked*: the fork continues the same conversation
  under the reference Voro named, and Voro records that reference on the
  session row once the send is confirmed — so later messages, the jump-in
  keys, and reconciliation all follow the conversation to where it continued. A
  `message` template without the placeholder resumes in place and keeps the
  reference it had, which is what the built-in `claude` one does — Voro
  releases the supervisor's hold with `stop` at rest rather than forking around
  it (DESIGN.md §8). `{new_session}` is refused on every other verb: it names
  the session a send opens, and nothing else opens one. `voro agent list` names
  a forking send `message(fork)` and a resuming one `message`.
- A `message` template should carry whatever permission flag its agent's
  `dispatch` carries — the built-in `claude` one carries `--permission-mode
  auto`. A resumed turn does real work, and on agents where the flag is per
  invocation rather than a property of the session, leaving it off runs that
  turn in the agent's default ask mode against a stdin at `/dev/null`: every
  edit and every command outside the allowlist stops for an approval nobody can
  give. The refusals land in the launch log rather than the TUI, so the send
  looks delivered and quietly does nothing.
- `logs` prints a session's recent output, taking `{session}` alone. Voro reads
  it for exactly one thing: whether that session is sitting on a **usage cap**
  (DESIGN.md §8). A cap does not kill a backgrounded session — the supervisor
  stays alive and waits for the window to reset — so without this a capped
  dispatch rides the running strip looking healthy for hours; with it, the row
  is badged `⚠ capped ↻21:50`. The same reading tells a capped death from an
  ordinary one, which the launch log cannot do for a `--bg` launch whose
  launcher exits at birth having logged only its backgrounding banner.
  Everything about it is best-effort: the output may be a terminal capture
  (escape sequences are stripped before matching), the exit status is not
  consulted, and an agent that defines no `logs` is probed for nothing and
  badged for nothing — `codex` names none. Tail the output in the template, as
  the built-in does; Voro reads whatever it prints. Note the built-in's
  truncation: `claude logs` keys on the *job* id, the first eight characters of
  the session id, so `{session}` is trimmed rather than passed whole.
- `stop` retires a session from the agent's own registry, taking `{session}`
  alone. Voro fires it on three triggers (DESIGN.md §8), and all three must be
  safe before you define it.

  The first is **closing the session's row**, so the agent's listing shows work
  actually in flight rather than every dispatch ever made — a `claude --bg`
  session otherwise keeps both its `claude agents` entry and a supervisor
  process until the machine reboots, and that listing is how you find a session
  to attach to and how Voro reads liveness. Voro fires it on the operator's
  closing verdicts (accept, abort, abandon) and on a reconciled dead or stale
  session.

  The second is **rest**: a task in `needs-input`, `review` or `waiting` — a
  session Voro is *keeping open* — whose listing entry reports its turn ended
  (`state: "done"`) is stopped too. This is what makes an in-place headless
  `message` possible at all, since the registration is the hold that would
  refuse it. The row is untouched by it: the session stays open, stays the
  task's conversation, and stays attachable, so `stop` must keep the
  conversation rather than discard it (`claude stop`: "Its conversation is
  kept"). The `done` test is the whole guard — a session at `blocked`, which is
  what a permission prompt and a supervisor mid-turn both look like, is never
  released, because the handover verbs run from inside a turn that may still be
  finishing.

  The third is the **capped-session sweep** (`u`), and it is the one that fires
  with no listing test at all. A session sitting on a usage cap is `blocked`
  with its supervisor alive, so it will never read `done` and the rest trigger
  will never reach it — yet the hold is exactly what would refuse the resume the
  sweep is about to make. What identifies it instead is the `logs` reading, and
  on the strength of that the sweep releases unconditionally. If your agent's
  sessions cannot be safely stopped while merely idle-looking, define no `stop`
  and the sweep degrades to a refused send rather than doing damage.

  The first two are fire and forget: Voro spawns them detached with output going
  to the launch log, reads neither the output nor the exit status, and never
  waits or rolls a transition back. A stop made by a *sender* is waited on
  instead — the quick message that outran a reconcile pass, and every nudge —
  and a non-zero exit refuses that send rather than spawning a message that
  could not land. Nothing is checked first in any case, so the verb
  must tolerate being fired at a session that is already gone (`claude stop`
  prints its line and exits zero). Note the built-in's truncation, the same as
  `logs`: `claude stop` keys on the eight-character job id, so `{session}` is
  trimmed rather than passed whole. `codex` names none, and a session under it
  lingers in whatever listing it keeps exactly as before — and its `message`,
  were it to define one, would resume without any release being needed.
- `plan` runs an interactive *foreground* session for the TUI's agent-assisted
  task creation (DESIGN.md §8): `{prompt_file}` holds the planning brief, and
  the command owns the terminal until the conversation ends, so it must not
  background itself. It carries no `{session}` — a planning session belongs to
  no task and records no session row — and no `{task_id}`, since it may be
  drafting a task that does not exist yet. It may carry `{session_name}`, as
  the built-in does: Claude's `--name` is not a background-only flag, so a
  planning or interactive-refine session is findable in the `/resume` picker
  afterwards.

The **model map** is the last three keys, and it is what `{model}` resolves to.
`dispatch` and `plan` may each carry the placeholder; Voro fills it from this
agent's own keys and never interprets the values, which are opaque names passed
straight through to the command:

- `model` on an ordinary dispatch — a workhorse for implementation.
- `model_deep` on a dispatch of a task flagged **deep** (`voro add --deep`,
  `voro set <id> --deep`, or `!` in the TUI) — the strongest model, for work
  that warrants it. Falls back to `model` when an agent names only one.
- `model_plan` on `plan`, whatever the queue holds — a planning session belongs
  to no task, so there is no depth to read. Falls back to `model` too.

The values above are `claude` model *aliases*, not pinned model ids, so each
resolves to the current model of its class and does not churn as models are
released. Want other models? Override the agent wholesale in `voro.toml` — copy
the block above and change the three keys (a user `[agents.claude]` table
replaces the built-in entirely, so keep every verb you still want).

The placeholder is meaningful only on `dispatch` and `plan`, the two verbs that
launch work; on a session verb it is refused at config load, since a session
already runs on whatever model started it. An agent that carries `{model}` but
names no `model` is likewise a config error. The reverse is fine and inert: an
agent whose templates carry no `{model}` — `codex` above — simply takes no model
direction, and a deep task dispatches with it exactly as any other task does,
which is how the flag degrades gracefully across agents.

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

**Liveness without pids.** A `--bg`-style launch is owned by a supervisor and
its spawned pid exits at once, so for agents with a `sessions` verb the
reconciler never checks the pid the session row recorded: liveness comes from
the listing entry the ref appears on, read by the contract in DESIGN.md §8
(`done` means dead; failing that an entry-named pid decides; failing both, only
`state: "working"` reads live). A session that drops out, finishes, or zombies
there without calling `voro done`/`ask` stalls its task, exactly as pid-death
does for plain agents. When liveness is unknowable (no ref, listing failed) the
session is left alone. The row's own pid is still read in one direction, for
every agent: a pid that is *alive* proves the session is, whatever the listing
says — which is what a quick message leaves behind, and a headless send does
not appear in the listing while it runs, so without this rule the next
reconcile would stall a task whose agent is mid-answer.

**Jump-in.** In the TUI, `A` on a running task runs the agent's `attach` command
with the TUI suspended — the real session, full control, including answering
permission prompts; on a review or stalled task it runs `resume` instead,
reopening the finished session. This is also how a `needs-input` question is
answered: the operator jumps into the agent's own session, answers in the
conversation, and then runs `voro resume` (or presses Enter on the inbox row) to
move the task back to `running`. Voro never records the answer text — the
exchange lives in the session transcript (DESIGN.md §6/§8).

**Quick message.** `a` is the same steering without the round-trip: it collects
one line and fires it into the session through `message`, leaving the TUI
standing. It applies to the three states whose session is open and between
turns — `needs-input`, `review`, `waiting` — and refuses the rest (DESIGN.md
§8). Voro probes liveness first and refuses a session still running, since
that one wants the terminal; if the same probe finds the session merely
registered at rest, it releases it through `stop` before sending, which
reconciliation has usually done already — so a message only ever reaches a
session that has explicitly handed back. On a `review` or `waiting` task the
message *is* a reject-with-feedback: the send goes first and the transition
follows it, so a send the agent refuses leaves the task untouched. On a
`needs-input` task nothing transitions — the answer lives in the transcript,
and the agent's own `voro resume` moves the task back (DESIGN.md §6). Either
way the session row follows the send: it records the process now carrying the
turn, so reconciliation leaves the task `running` while the agent answers, and
— for a verb that forks (`{new_session}`) — the reference the conversation
continued under.

The same jump-in resolves a **stale review branch**. A task can sit in `review`
while other work merges, leaving its branch in conflict with the moved base
(Voro surfaces this as the `[branch conflicts]` marker, DESIGN.md §8). Rather
than have Voro rebase on the operator's behalf, the operator attaches to the
task's still-open agent session and asks it to fix the branch. The dispatch
preamble already tells the agent how: if its branch conflicts with the base, it
runs `git fetch origin <base>` from inside its own worktree and rebases or merges
onto `origin/<base>` there. Fetching only updates remote-tracking refs and
touches no working tree — including the primary checkout — so it leaves the trust
model intact: a dispatched agent still cannot push. The task never leaves
`review`; the PR simply updates.

Every verb degrades gracefully when absent: no `attach`/`resume` disables the
jump-in key for that agent, no `message` turns the quick-message key into a
status line pointing at the jump-in, no `sessions` keeps pid-liveness
reconciliation, no `stop` leaves closed sessions registered with the agent as
they always were, no `plan` turns the TUI's planning key into a status line saying
what to configure, and no `{model}` anywhere makes the deep flag a no-op for
that agent. An agent defining only `dispatch`/`cmd` behaves exactly as before
the verbs existed.

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
stop     = "tmux kill-session -t {session}"
```

A tmux session vanishes from `list-sessions` when its command exits, which is
exactly the drop-out the reconciler treats as finished-without-reporting — it
finalises the session and lands the task in `stalled` (DESIGN.md §8), where
redispatch is one key away. There is no honest `resume` for a dead tmux
session, so leave that verb off and let redispatch handle it. `stop` is honest
here, though: an abort takes the running session down with the task, and on a
close where the command has already exited `kill-session` finds nothing and says
so in the launch log, which is all a failed stop ever does.

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
| `SessionEnd` | the session terminates normally | `voro done --branch <current branch> [--summary <final message>]` if the task is still `running` | upgrades a forgotten `done` from a stall to a real `review`, recording the branch and, best-effort, the final assistant message as the summary |
| `Notification` | Claude needs permission, or has idled waiting for input | `voro ask` with the notification message | the *only* signal for a session that is alive but stuck: its process still runs, so the pid-liveness reconciler never fires |
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

The built-in `stop` is verified against a live `claude` (v2.1.206) on throwaway
sessions: `claude stop <short-id>` retires the entry from the default `claude
agents --json` listing (it survives only under `--all`) and kills the supervisor
daemon, keeping the conversation for a later `claude attach`; and stopping a
session whose supervisor is already dead prints `stopped <id>` and exits zero,
which is what lets Voro fire it without checking first. `claude stop` has its own
`--help` but is absent from `claude --help`'s command table, which is exactly why
it rides an optional verb: a claude that drops it degrades to a lingering listing
entry, nothing broken.

The rest-release the built-in `message` depends on is verified on the same
footing, end to end against a real `--bg` dispatch on a scratch store. The
session hands over, reconcile releases it (its entry leaves the default listing;
a second stop the same second is accepted and exits zero, which is the
idempotence the rule relies on), and two quick messages then land back to back
with a `voro done` between them — both rendered as `claude -p --resume <uuid>`
against the *same* session id, with no stop between them and no manual step. The
work of all three turns is there afterwards, and the conversation is one
transcript file: no fork siblings. The display name rides the transcript's own
`customTitle` record and survives the stop and both in-place resumes, which is
the whole reason delivery resumes rather than forks. One observation worth
recording because it cuts the other way from the assumption: on this version a
finished `-p` turn did *not* put the session back into the default listing, so
no further release was needed and none was made. The rule is written to be
indifferent to that — a session that does re-register reads `done` and is
released on the next pass — but nothing should be built on the re-registration
happening.

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
