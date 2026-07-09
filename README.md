# Voro

A local command centre for AI-assisted development across many projects. Voro
tracks tasks per project, weights each project by how much it matters *today*,
and answers one question: **where should my attention go right now?**

Two views drive everything: an **inbox** of tasks blocked on a human decision,
and the single highest-priority **ready task** across all projects — with its
body written as a prompt, ready to dispatch to a coding agent.

**Status:** early development, pre-everything. Expect churn.

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

Voro dispatches a task by running a shell command template per agent, read from
`~/.config/voro/agents.toml` (see DESIGN.md §8). That file is not created for
you — scaffold a starter with:

```bash
voro agents init      # writes ~/.config/voro/agents.toml (won't overwrite)
voro agents list      # show configured agents; * marks the default
voro agents path      # print where dispatch looks for the file
```

Then edit the file so each `[agents.<name>]` `cmd` matches an agent you have
installed. `{prompt_file}` is replaced with a path to the task's prompt, and
`default` names the agent used when a task has no `--agent` override. A
dispatched session runs unattended, so most agents need a non-interactive
permission flag. Once configured, `voro dispatch <task-id>` (or the dispatch
key in the TUI) launches a headless session on a ready task.

A dispatched agent reports back through the return-path verbs (`voro ask/done/
propose`). For the `CLAUDE.md`/`AGENTS.md` snippet that advertises them, and a
sample Claude Code hooks configuration that reports for a session that forgets
to, see [`docs/agent-integration.md`](docs/agent-integration.md).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual licensed as above, without any
additional terms or conditions.
