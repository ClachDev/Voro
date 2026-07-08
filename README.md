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

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual licensed as above, without any
additional terms or conditions.
