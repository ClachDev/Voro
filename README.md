# Voro

A local command centre for AI-assisted development across many projects. Voro
tracks tasks per project, weights each project by how much it matters *today*,
and answers one question: **where should my attention go right now?**

A single **next-action queue** drives everything: questions, reviews, and
proposals that need a human first, then the highest-scoring ready tasks across
all projects — each body written as a prompt, ready to dispatch to a coding
agent.

**Status:** early development. The cockpit, CLI, and dispatch loop work
end-to-end. Expect churn.

![Voro TUI showing the next-action queue and running sessions](docs/images/voro-tui.png)

## Design

The full design lives in [`docs/DESIGN.md`](docs/DESIGN.md) — concepts, schema,
task state machine, scoring, and dispatch semantics. Agent contributors should
read [`CLAUDE.md`](CLAUDE.md) first.

## Building

Rust workspace: `voro-core` (store, scheduler) and `voro` (ratatui TUI).

```bash
cargo build --workspace
cargo test --workspace
cargo run -p voro
```

## Dispatching to agents

Voro dispatches a task by running a shell command template per agent. It ships
with built-in `claude` and `codex` agents (see DESIGN.md §8), so with one of
those on your `PATH` a fresh install dispatches with no configuration at all:
`voro dispatch <task-id>` (or the dispatch key in the TUI) launches a headless
session on a ready task, and `default_agent` resolves to the first built-in
found on `PATH`.

To extend or override the built-ins, layer a `~/.config/voro/voro.toml` on
top:

```bash
voro agent list       # effective agents (built-in + user) with provenance; * marks the default
voro agent init       # optional: write a voro.toml skeleton (won't overwrite)
voro agent path       # print where dispatch looks for the file
```

Each `[agents.<name>]` table adds an agent or, when named `claude`/`codex`,
replaces that built-in wholesale (so copy every verb you still want — `agent
init` writes the built-ins commented out, ready to copy). `{prompt_file}` is
replaced with a path to the task's prompt, and `default_agent` names the agent
used when a task has no `--agent` override. A dispatched session runs
unattended, so most agents need a non-interactive permission flag.

A dispatched agent reports back through the return-path verbs (`voro ask/done/
propose`). For the `CLAUDE.md`/`AGENTS.md` snippet that advertises them, and a
sample Claude Code hooks configuration that reports for a session that forgets
to, see [`docs/agent-integration.md`](docs/agent-integration.md).

## Seeing a diff

Once an agent finishes and its task lands in `review`, `voro open <task-id>`
(or the open key in the TUI, on a review or running row) opens the task's
checkout in a viewer so you can see the diff. Like agents, the viewer is a
command template — add an optional `[viewer]` table to `voro.toml`:

```toml
[viewer]
cmd = "zed {path}"        # or e.g. "git difftool -d"
```

`{path}` is replaced with the project's checkout path, and the command is run
in that directory, so a viewer that operates on the current directory needs no
placeholder. Prefer a viewer that opens its own window (an editor or a GUI
difftool); with no `[viewer]` configured, the open action reports what to add
rather than failing silently.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual licensed as above, without any
additional terms or conditions.
