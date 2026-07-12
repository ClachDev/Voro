---
name: verify
description: Drive the voro TUI end-to-end against a scratch database to observe a change working. Use when verifying TUI or dispatch changes at the real surface rather than through tests.
---

# Verifying voro live

Build with `cargo build --workspace`; the binary lands at `target/debug/voro`.

Everything isolates through two env vars: `VORO_DB=<scratch>/voro.db` picks the
database (a missing file is created), and `XDG_CONFIG_HOME=<scratch>/config`
redirects the agents/viewers config to `<scratch>/config/voro/voro.toml`.

## Seeding states

Create fixtures with the CLI (`voro project add`, `voro add … --state ready`,
`voro dispatch <id>`). To get a *stalled* task, dispatch to a stub agent that
exits immediately — reconcile-on-read stalls it on the next `voro list` or TUI
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
  "env VORO_DB=… XDG_CONFIG_HOME=… PAGER=less ./target/debug/voro"
/bin/tmux -L <socket> send-keys <key>
/bin/tmux -L <socket> capture-pane -p
/bin/tmux -L <socket> kill-server
```

`r` refreshes in place after CLI mutations from outside; `PAGER` is honoured
by the `l` log key, so setting it to `less` keeps the pager capturable.
