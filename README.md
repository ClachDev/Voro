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

## Install

Voro is Unix-only — Linux and macOS. It installs a single binary, `voro`, that is
both the TUI cockpit (run with no arguments) and the CLI (`voro <verb>`).

The quickest path is the prebuilt shell installer, which downloads the right
binary for your platform and drops it in Cargo's bin directory (`~/.cargo/bin`):

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ClachDev/Voro/releases/latest/download/voro-installer.sh | sh
```

Prefer to place the binary yourself? Each [GitHub
Release](https://github.com/ClachDev/Voro/releases) also carries tarballs —
`x86_64-unknown-linux-gnu` for Linux and `aarch64-apple-darwin` for Apple Silicon
macOS — alongside their checksums. Download one, extract it, and put `voro` on
your `PATH`.

To build and install from source instead:

```bash
cargo install --git https://github.com/ClachDev/Voro voro
```

## Quickstart

Voro is driven from its TUI cockpit. Launch it by running `voro` with no
arguments:

```bash
voro
```

The cockpit has three screens — **Cockpit** (the next-action queue), **Tasks**
(every task), and **Projects**. `tab` cycles between them, `j`/`k` move the
selection, and the footer always shows the keys that apply to what you have
selected. The walkthrough below drives one task from nothing to a reviewed diff.

**1. Register a project.** Go to the Projects screen (`tab` to it) and press `a`
to add one — Voro asks for a name and a path. Press `0`–`5` on a project to set
its weight: the higher the weight, the harder that project's tasks pull toward the
top of the queue.

**2. Create a task.** From the Cockpit or Tasks screen, press `n` to write the
task body in your `$EDITOR`, or `N` to launch a planning session — an
agent-assisted flow that drafts the body for you. Either way, **the body you end
up with is the prompt** the dispatched agent receives, so it is worth writing
like one.

**3. Triage it into the queue.** A newly created task arrives as a proposal.
Select it in the queue and press `enter` — the footer reads `⏎ triage` — to accept
it into ready work. The queue always floats items that need you first (a
question shows `⏎ answer`, a finished task shows `⏎ review`) above the
highest-scoring ready tasks.

**4. Dispatch it to an agent.** Select a ready task and press `d` to hand it to
the default coding agent, or `D` to choose which agent. Voro launches a headless
session; the agent works against the task body and reports back through the
return-path verbs (`voro ask` / `voro done` / `voro propose`), which are wired up
per [`docs/agent-integration.md`](docs/agent-integration.md). Those verbs are the
agent's interface, not yours.

**5. Review what lands.** When the agent calls `done`, the task moves to
`review` and rises to the top of the queue. Press `o` to open its checkout in a
viewer, or `g` to open its pull request (creating it when the project reviews
through GitHub). With the diff in front of you, press `enter` (`⏎ review`) to
accept or reject the work; rejecting with a note re-dispatches the agent to
address it.

Other keys worth knowing on a selected task: `s` change state, `x` the score
breakdown, `h` its history, `e` edit, `l` the session log, and `q` to quit.

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

## Releasing

Releases are cut with [cargo-release](https://github.com/crate-ci/cargo-release)
and built by [cargo-dist](https://github.com/axodotdev/cargo-dist). Both crates
share one version and ship under a single `v{version}` tag; pushing that tag
runs [`.github/workflows/release.yml`](.github/workflows/release.yml), which
builds `x86_64-unknown-linux-gnu` and `aarch64-apple-darwin` tarballs with
checksums and a `curl | sh` installer, then publishes a GitHub Release whose
notes come from the matching [`CHANGELOG.md`](CHANGELOG.md) section.

Record changes under the `Unreleased` heading in `CHANGELOG.md` as you go. To
cut a release, from a clean `main` run `cargo release <level> --execute` (e.g.
`patch`); cargo-release bumps the version, rolls `Unreleased` into a dated
section, commits, and tags. It does not push — review, then
`git push --follow-tags` to trigger the build. The first `v0.1.0` release is
special: the version is already `0.1.0`, so tag it directly with
`git tag v0.1.0 && git push origin v0.1.0`.

## Dispatching to agents

Voro dispatches a task by running a shell command template per agent, and ships
with built-in `claude` and `codex` agents, so with one of those on your `PATH` a
fresh install dispatches with no configuration: `voro dispatch <task-id>` (or the
dispatch key in the TUI) launches a headless session on a ready task. The agent
reports back through the return-path verbs (`voro ask/done/propose`), and its work
lands in `review` where `voro open` or `voro pr` puts the diff in front of you.

To extend or override the built-in agents and viewers, layer a
`~/.config/voro/voro.toml` on top (`voro agent init` writes a skeleton). The
dispatch semantics, the review action, and the `voro.toml` format are covered in
[`docs/DESIGN.md`](docs/DESIGN.md) §8; the `CLAUDE.md`/`AGENTS.md` return-path
snippet and the Claude Code hooks configuration are in
[`docs/agent-integration.md`](docs/agent-integration.md).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this work shall be dual licensed as above, without any
additional terms or conditions.
