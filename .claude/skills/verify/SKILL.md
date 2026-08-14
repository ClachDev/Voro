---
name: verify
description: Drive the voro TUI end-to-end against a scratch database to observe a change working. Use when verifying TUI or dispatch changes at the real surface rather than through tests.
---

# Verifying voro live

Build with `cargo build --workspace`; the binary lands at `target/debug/voro`.

Isolate the store by passing `--db <scratch>/voro.db` to every invocation of
that binary, the TUI launch included; a missing file is created. The flag is
the only thing that moves a `target/` build off the dev store. Such a build
declines an inherited `VORO_DB` on purpose (DESIGN.md §5): dispatch exports
that variable onto every agent it spawns, which makes it a value a process
inherits rather than one it asks for, so a build out of `target/` reads it as
someone else's store and falls back to `dev.db`. It says as much on stderr —
where ratatui's alternate screen paints over it at once, leaving a TUI sitting
on the real dev database and looking isolated. Treat `VORO_DB` as what a
dispatched session inherits, never as what a manual run sets, and do not
"correct" these instructions back to it.

The config isolates separately and does take an environment variable:
`XDG_CONFIG_HOME=<scratch>/config` redirects the agents/viewers config to
`<scratch>/config/voro/voro.toml`.

## Seeding states

Create fixtures with the CLI, giving every verb the same `--db`
(`voro --db <scratch>/voro.db project add <name> <path>`,
`voro --db … add <project> "<title>" --state ready`, `voro --db … dispatch
<id>`). To get a *stalled* task, dispatch to a stub agent that exits
immediately — reconcile-on-read stalls it on the next `voro list` or TUI
refresh:

```toml
default_agent = "stub"

[agents.stub]
dispatch = "cat {prompt_file}"                       # dies at once -> failed

[agents.capstub]
dispatch = "echo usage limit reached # {prompt_file}" # cap phrase -> capped
```

The dispatch template must contain `{prompt_file}` or config validation
rejects it. Capped detection matches "usage limit", "rate limit", or
"quota exceeded" in the log tail (`crates/voro/src/reconcile.rs`).

## Driving the TUI

Run it in an isolated tmux and capture panes. The login shell's tmux alias is
broken for non-interactive use — call `/bin/tmux` directly:

```sh
/bin/tmux -L <socket> new-session -d -x 110 -y 30 \
  "env XDG_CONFIG_HOME=… PAGER=less ./target/debug/voro --db <scratch>/voro.db"
/bin/tmux -L <socket> send-keys <key>
/bin/tmux -L <socket> capture-pane -p
/bin/tmux -L <socket> kill-server
```

`r` refreshes in place after CLI mutations from outside; `PAGER` is honoured
by the `l` log key, so setting it to `less` keeps the pager capturable.

A scratch store opens empty — the fixture seeds only the dev store — so the
first capture of an unseeded run shows a board with no projects and no tasks.
Real project names in that capture mean the isolation did not take.
