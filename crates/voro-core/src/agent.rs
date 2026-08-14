//! Agent dispatch templates (DESIGN.md §5, §8): command templates, not state,
//! so they live outside the database. Owns the built-in `claude`/`codex`
//! definitions, layers the user's `~/.config/voro/voro.toml` on top, and
//! resolves which agent a task dispatches with.
//!
//! An agent is a set of verb templates; only `dispatch` is required (`cmd` is
//! an accepted alias). The optional `sessions`/`attach`/`resume`/`message`/`stop`
//! verbs unlock session-aware dispatch and `plan` unlocks the TUI's interactive
//! planning sessions (DESIGN.md §8); each degrades gracefully when absent
//! (docs/agent-integration.md). Config is layered: built-ins under `voro.toml`,
//! which may add agents, override a built-in wholesale, and set `default_agent`
//! and viewers. A missing file is not an error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::LivenessSource;
use crate::scheduler::{AttentionCosts, DEFAULT_MAX_RUNNING};
use crate::template::{render, shell_quote};

/// The prompt-file substitution in the `dispatch`, `plan` and `message`
/// templates. The working directory is handled by the spawner, not the
/// template.
pub const PROMPT_FILE_PLACEHOLDER: &str = "{prompt_file}";

/// The task-id substitution in the `dispatch` template, the numeric id of the
/// task. Optional — a template that omits it dispatches unchanged — so a
/// template can put the id somewhere other than the session name. Refused on
/// `plan`, which serves targets that have no task id to bind.
pub const TASK_ID_PLACEHOLDER: &str = "{task_id}";

/// The session-name substitution in the `dispatch` and `plan` templates: the
/// name Voro composes for the session a launch opens ([`Launch::session_name`]),
/// so every backgrounded session is findable by a name Voro chose. Optional,
/// and refused on the session verbs for the same reason [`MODEL_PLACEHOLDER`]
/// is — they act on a session that already exists and has its name.
pub const SESSION_NAME_PLACEHOLDER: &str = "{session_name}";

/// The session-reference substitution in the `attach`, `resume`, `message`,
/// `logs` and `stop` templates: the agent-opaque reference captured at dispatch
/// (a Claude session UUID, a Codex session id, a tmux session name).
pub const SESSION_PLACEHOLDER: &str = "{session}";

/// The fresh-reference substitution in the `message` template, bound to a v4
/// UUID Voro generates for the send (DESIGN.md §8). An agent whose sessions are
/// held by a supervisor cannot be resumed headlessly while that supervisor
/// lives; it can be *forked*, which continues the same conversation under a
/// reference the caller names up front. A `message` template carrying this
/// placeholder is declaring that shape, and the session row follows the fork:
/// what Voro binds here becomes the session's reference. Optional — a template
/// without it resumes in place and keeps the reference it had.
pub const NEW_SESSION_PLACEHOLDER: &str = "{new_session}";

/// The model substitution in a verb template, resolved from the agent's own
/// `model`/`model_deep`/`model_plan` keys (DESIGN.md §8). Voro is model-blind:
/// the values are opaque strings it pastes into the command and never
/// interprets. Optional — an agent with no `{model}` anywhere takes no model
/// direction at all, and a deep task dispatches with it unchanged.
pub const MODEL_PLACEHOLDER: &str = "{model}";

/// The substitution in a viewer command template (DESIGN.md §11a): the checkout
/// path of the task's project — or the task's worktree, when it has a branch
/// checked out in one (DESIGN.md §8). Optional — a viewer that acts on the
/// current directory (`git difftool -d`) needs no placeholder.
pub const VIEWER_PATH_PLACEHOLDER: &str = "{path}";

/// The substitution in a viewer command template for the task's git branch, or
/// empty when the task has none. Paired with [`VIEWER_BASE_PLACEHOLDER`] it lets
/// a viewer express a diff range (`{base}...{branch}`) rather than a bare
/// directory (DESIGN.md §8).
pub const VIEWER_BRANCH_PLACEHOLDER: &str = "{branch}";

/// The substitution in a viewer command template for the checkout's default
/// branch — the base a task branch is diffed against (DESIGN.md §8).
pub const VIEWER_BASE_PLACEHOLDER: &str = "{base}";

/// The agents Voro ships with the binary, layered under any `voro.toml`
/// (DESIGN.md §5/§8). Compiled in, so a binary upgrade upgrades the agents; a
/// user `voro.toml` can override either wholesale ([`Provenance::UserOverride`]).
/// `claude` launches attachably (`--bg`) with the full session verb set and
/// plans interactively in the foreground; `codex` covers the headless-resume
/// shape. Must parse and pass [`validate_agent`].
///
/// Both claude verbs name their session from `{session_name}` rather than
/// spelling `voro-{task_id}` themselves, so every launch Voro makes — a
/// dispatch, a refine, a planning session — carries a distinct Voro-composed
/// name in `claude agents` and the `/resume` picker (DESIGN.md §8). `--name` is
/// not a background-only flag, so the foreground `plan` verb takes it too.
///
/// The claude verbs take their model from `{model}` rather than a baked-in
/// flag, so the model varies per purpose and per task: `model` is the workhorse
/// a normal dispatch runs, `model_deep` the stronger one a `deep` task earns,
/// and `model_plan` the one interactive planning reasons with (DESIGN.md §8).
/// They name `claude` model *aliases* (`opus`, `fable`), not pinned model ids,
/// so each resolves to the current model of that class and does not churn with
/// each release; an operator wanting other models overrides the agent
/// wholesale in `voro.toml`. `codex` carries no `{model}`, which is the
/// no-model-direction case: a deep task dispatches with it unchanged.
///
/// The claude `message` verb is `resume` plus `-p`, and the near-duplication is
/// deliberate: a verb is an opaque per-agent contract, which is exactly what
/// lets an agent define a subset of them and degrade per-verb. `codex` defines
/// no `message` and the TUI's quick-message key says so on the status line.
/// It resumes the session in place rather than forking it
/// ([`NEW_SESSION_PLACEHOLDER`], which the verb no longer carries): a
/// `claude --bg` session keeps its supervisor process after finishing its turn,
/// and that supervisor refuses a headless `--resume` for as long as it lives —
/// so Voro releases it at rest instead, through `stop`, and the send then lands
/// on the session's own reference (DESIGN.md §8). A fork would land too, but it
/// moves the conversation out from under the name Voro composed for it, and that
/// name is how the operator addresses the session everywhere outside Voro.
///
/// It carries `--permission-mode` for the same reason `dispatch` does: the mode
/// belongs to a launch rather than to a verb (DESIGN.md §8). The flag is per
/// invocation rather than a property of the session, so a resumed turn without
/// it runs in the default ask mode against a stdin at `/dev/null`: every edit
/// and every command outside the allowlist stops for an approval nobody can
/// give, and the refusals land in the launch log rather than the TUI. A send
/// like that appears to have been delivered and quietly does nothing, which is
/// the one failure a fire-and-forget channel cannot report — and since `message`
/// carries every rejection's feedback, a rework session missing the flag cannot
/// even run `voro done`, so its finished work goes unreported and reconcile
/// lands the task in `stalled`, reading as an agent that died.
///
/// `resume` deliberately carries no mode: it hands the operator a terminal and
/// no prompt, so the ask-mode default is answerable by whoever is sitting there.
///
/// The claude `logs` verb replays a background session's screen, which is the
/// only place a usage cap is legible (DESIGN.md §8): `claude agents --json`
/// reports a capped session as plain `blocked`, the same word a permission
/// prompt earns, and Voro's own launch log holds nothing but the backgrounding
/// banner. Two details of the spelling are load-bearing. `claude logs` keys on
/// the *job* id — the first eight characters of the session id — so `{session}`
/// is truncated in the template rather than passed whole; and the output is a
/// full screen replay of unbounded length, so it is tailed here rather than
/// read whole by the caller. It exits zero when it finds no such job, printing
/// a not-found line instead, which is why nothing reads its status: text with
/// no cap signature in it means "not capped", however it came about.
///
/// The claude `stop` verb retires a session from the agent's own listing once
/// Voro closes its row — and, at rest, once it hands back (DESIGN.md §8): the
/// release the supervisor holds is what a headless `message` resumes through.
/// A `claude --bg` session outlives its work
/// twice over — the entry stays in `claude agents` and the supervisor holding it
/// runs until the machine reboots — so an operator who dispatches all week reads
/// their session list through a wall of finished ones. The conversation survives
/// the call: `claude stop` keeps the transcript and drops only the entry from the
/// default listing, which `claude attach` can still reopen. It keys on the *job*
/// id as `logs` does, so `{session}` is truncated to the same eight characters,
/// and it exits zero on a session whose supervisor is already gone, which is what
/// lets Voro fire it without checking first.
const BUILTIN_AGENTS: &str = "\
[agents.claude]
dispatch   = \"claude --bg --name \\\"{session_name}\\\" --permission-mode auto --model {model} \\\"$(cat {prompt_file})\\\"\"
sessions   = \"claude agents --json\"
attach     = \"claude attach {session}\"
resume     = \"claude --resume {session}\"
message    = \"claude -p --resume {session} --permission-mode auto \\\"$(cat {prompt_file})\\\"\"
logs       = \"claude logs \\\"$(printf %.8s {session})\\\" 2>/dev/null | tail -c 20000\"
stop       = \"claude stop \\\"$(printf %.8s {session})\\\"\"
plan       = \"claude --name \\\"{session_name}\\\" --permission-mode auto --model {model} \\\"$(cat {prompt_file})\\\"\"
model      = \"opus\"
model_deep = \"fable\"
model_plan = \"fable\"

[agents.codex]
dispatch = \"codex exec \\\"$(cat {prompt_file})\\\"\"
resume   = \"codex resume {session}\"
";

/// The order the built-in agents are probed against PATH when no `default` is
/// configured: the first one both defined and installed wins (DESIGN.md §8).
const DEFAULT_PROBE_ORDER: [&str; 2] = ["claude", "codex"];

/// The parsed, validated built-in templates, layered under a user file by
/// [`AgentsConfig::load`]. A parse or validation failure is a bug in
/// [`BUILTIN_AGENTS`], so it panics rather than surfacing as a config error.
fn builtin_agents() -> &'static BTreeMap<String, AgentTemplate> {
    static BUILTINS: LazyLock<BTreeMap<String, AgentTemplate>> = LazyLock::new(|| {
        let raw: RawConfig = toml::from_str(BUILTIN_AGENTS).expect("built-in agents TOML parses");
        for (name, agent) in &raw.agents {
            validate_agent(name, agent, Path::new("<built-in>")).expect("built-in agent is valid");
        }
        raw.agents
    });
    &BUILTINS
}

/// The viewers Voro ships with the binary, layered under any `voro.toml`
/// exactly as [`BUILTIN_AGENTS`] is (DESIGN.md §11a), so a fresh install with
/// an editor CLI on PATH opens a task's checkout with no config at all. A user
/// `[viewers.<name>]` table of the same name replaces one wholesale.
///
/// Each opens its own window on a directory, because that is the only shape
/// `open` can run: the viewer is spawned detached with no terminal (DESIGN.md
/// §8), so a pager-driven command such as `git difftool -d` has nothing to draw
/// on. They therefore take `{path}` alone rather than a `{base}...{branch}`
/// range — an editor cannot open a diff range from its command line — which is
/// what the README's review step promises anyway.
const BUILTIN_VIEWERS: &str = "\
[viewers.code]
cmd = \"code -n {path}\"

[viewers.cursor]
cmd = \"cursor -n {path}\"

[viewers.zed]
cmd = \"zed {path}\"
";

/// The built-in viewers by name, in the order they are probed against PATH
/// when nothing user-configured resolves: the first one installed wins
/// (DESIGN.md §11a). Public because the messages that say what was looked for
/// are written where the failure is surfaced, and none of them should spell
/// the list again.
pub const BUILTIN_VIEWER_NAMES: [&str; 3] = ["code", "cursor", "zed"];

/// The parsed built-in viewers, layered under a user file the same way
/// [`builtin_agents`] is. A malformed built-in is a bug here, not a user config
/// error, so it panics.
fn builtin_viewers() -> &'static BTreeMap<String, ViewerTemplate> {
    static BUILTINS: LazyLock<BTreeMap<String, ViewerTemplate>> = LazyLock::new(|| {
        let raw: RawConfig = toml::from_str(BUILTIN_VIEWERS).expect("built-in viewers TOML parses");
        for (name, viewer) in &raw.viewers {
            assert!(
                !viewer.cmd.trim().is_empty(),
                "built-in viewer '{name}' has a command"
            );
        }
        for name in BUILTIN_VIEWER_NAMES {
            assert!(
                raw.viewers.contains_key(name),
                "probe order names a built-in viewer, not '{name}'"
            );
        }
        raw.viewers
    });
    &BUILTINS
}

/// Whether a name is a built-in viewer. The write surfaces ask, so removing or
/// editing one is refused as "built in, override it" rather than reported as a
/// viewer that isn't there.
pub fn is_builtin_viewer(name: &str) -> bool {
    builtin_viewers().contains_key(name)
}

/// A built-in viewer's command, so an override can start from what it replaces
/// rather than from an empty field.
pub fn builtin_viewer_cmd(name: &str) -> Option<&'static str> {
    builtin_viewers().get(name).map(|v| v.cmd.as_str())
}

/// Header prose for the skeleton `agent init` writes. [`starter_config`]
/// appends the current built-ins (commented) and example stanzas after it.
const STARTER_HEADER: &str = r#"# Voro configuration (~/.config/voro/voro.toml).
#
# This file is OPTIONAL. Voro ships with built-in `claude` and `codex` agents
# and built-in `code`, `cursor` and `zed` viewers, so a fresh install with any
# of those on PATH dispatches and opens a diff with no config here. Run
# `voro agent list` and `voro viewer list` to see the effective sets and where
# each entry comes from.
#
# Use this file to extend or override the built-ins, and to set app options:
#
#   * add your own agent — a new [agents.<name>] table. Only `dispatch` is
#     required (`cmd` is an alias): it starts a session on a task, with
#     `{prompt_file}` replaced by the prompt file's path, the optional
#     `{session_name}` by the name Voro composes for the session
#     (`voro-<id>-<title-slug>` for a dispatch, `voro-<id>-refine` for a
#     refine, `voro-plan-<project>` for planning, `voro-propose-<project>` for
#     a quick propose), and the optional `{task_id}`
#     by the task's numeric id.
#     The optional session verbs unlock attachable dispatch, and each degrades
#     gracefully when absent:
#       sessions  list the agent's sessions as JSON (liveness + ref capture).
#                 Each entry needs an id (`sessionId`, or `id`); `state`
#                 (`done` once finished, `working` while going) and `pid` say
#                 whether it is still live, `pid` deciding it where present.
#                 An entry carrying neither reads as dead.
#       attach    open a running session interactively    ({session})
#       resume    reopen a finished session interactively  ({session})
#       message   say one thing into a session headlessly, no terminal
#                 ({session} and {prompt_file}, plus the optional
#                 {new_session}: a fresh reference for an agent that can only
#                 be joined by forking, which the session row then follows)
#       logs      print a session's recent output               ({session})
#                 Read for one thing: whether the session is sitting on a
#                 usage cap, which is badged on the running strip and used to
#                 tell a capped death from an ordinary one. Tail it in the
#                 template — Voro reads whatever it prints.
#       stop      retire a session from the agent's own registry ({session})
#                 Fired when Voro closes the session's row, so the agent's
#                 listing shows work actually in flight. Fire and forget: Voro
#                 reads neither output nor status, and a session already gone
#                 is not an error.
#       plan      run an interactive foreground planning session ({prompt_file})
#     `plan` may carry `{session_name}` too, but not `{task_id}`: a planning
#     session drafts a task rather than naming one.
#     `dispatch` and `plan` may also carry `{model}`, filled from this agent's
#     own model keys: `model` normally, `model_deep` for a task flagged deep
#     (`voro set <id> --deep`), and `model_plan` when planning — the last two
#     falling back to `model`. The values are opaque names Voro pastes in and
#     never interprets, so an agent with no `{model}` takes no model direction
#     and a deep task dispatches with it unchanged.
#     See docs/agent-integration.md for the full contract.
#   * override a built-in — a table named `claude` or `codex` REPLACES that
#     built-in entirely (not per-verb), so copy every verb you still want. The
#     built-ins are reproduced below, commented out, ready to copy.
#   * set `default_agent` — used for tasks with no --agent override. When unset,
#     Voro picks the first built-in found on PATH (claude, then codex).
#   * set up viewers — [viewers.<name>] tables define how a task's diff is
#     shown locally by `voro open` (DESIGN.md §8). A viewer cmd may carry
#     `{path}` (the task's worktree, or the project checkout when it has none),
#     `{branch}` (the task's branch, or empty), and `{base}` (the checkout's
#     default branch); `{base}...{branch}` spells a diff range. Viewers are
#     built in like the agents — a table named `code`, `cursor` or `zed`
#     replaces that built-in wholesale, any other name adds a viewer.
#     `default_viewer` names the one used when a project does not pick a viewer
#     itself (`voro project viewer <p> <name>`); unset, Voro uses the sole
#     viewer defined here, else the first built-in found on PATH. A single
#     anonymous [viewer] table is the older, still-valid spelling of the
#     default. A viewer must open its own window: `voro open` spawns it
#     detached with no terminal, so a pager-driven command cannot draw.
#   * price the queue — `max_running` caps how many dispatches ride at once
#     (default 5; at the cap the queue offers no more), and a [costs] table
#     divides each row's score by what its action asks of you, so a cheap
#     decision outranks an expensive review of the same raw worth. Keep the
#     band narrow (DESIGN.md §7) — it is a nudge, not a re-ranking.
"#;

/// The full skeleton `voro agent init` writes: the header, the built-ins
/// reproduced commented-out (copyable to override or model an agent), then
/// example stanzas. Every line is a comment, so the file defines nothing until
/// the user uncomments something; the commented block is derived from
/// [`BUILTIN_AGENTS`] so it cannot drift from what ships.
fn starter_config() -> String {
    let mut out = String::from(STARTER_HEADER);
    out.push_str(
        "\n# --------------------------------------------------------------------------\n\
         # Built-in agents and viewers, exactly as shipped. Uncomment a block and\n\
         # edit it to override that entry wholesale; leave it commented to keep the\n\
         # built-in, which updates with Voro. Copy a block to model one of your own.\n\
         # --------------------------------------------------------------------------\n#\n",
    );
    for line in BUILTIN_AGENTS
        .lines()
        .chain([""])
        .chain(BUILTIN_VIEWERS.lines())
    {
        if line.is_empty() {
            out.push_str("#\n");
        } else {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(
        "\n# --------------------------------------------------------------------------\n\
         # Examples (uncomment and tune):\n#\n\
         # default_agent = \"claude\"\n#\n\
         # [agents.mine]\n\
         # dispatch = \"my-agent run {prompt_file}\"\n#\n\
         # default_viewer = \"zed\"\n#\n\
         # [viewers.difftool]\n\
         # cmd = \"git -C {path} difftool -d {base}...{branch}\"\n#\n\
         # max_running = 5\n#\n\
         # [costs]\n\
         # answer = 0.8\n\
         # triage = 0.8\n\
         # dispatch = 1.0\n\
         # review = 1.4\n\
         # do = 1.8\n",
    );
    out
}

/// A named set of verb templates from `voro.toml`. `dispatch` (or its alias
/// `cmd`) is required and always contains [`PROMPT_FILE_PLACEHOLDER`]; it may
/// also carry the optional [`SESSION_NAME_PLACEHOLDER`] and
/// [`TASK_ID_PLACEHOLDER`]. The rest are optional, with their
/// `{session}`/`{prompt_file}` placeholders validated at parse time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplate {
    dispatch: Option<String>,
    /// Pre-verb alias for `dispatch`, so existing configs load unchanged.
    cmd: Option<String>,
    sessions: Option<String>,
    attach: Option<String>,
    resume: Option<String>,
    /// A *headless* send into an existing session, carrying both
    /// [`SESSION_PLACEHOLDER`] and [`PROMPT_FILE_PLACEHOLDER`]: it appends one
    /// message to that session's transcript and returns, owning no terminal
    /// (DESIGN.md §8). What the TUI's quick-message key fires.
    message: Option<String>,
    /// A session's recent output, carrying [`SESSION_PLACEHOLDER`]: whatever
    /// the agent can say about what one of its sessions is doing right now
    /// (DESIGN.md §8). Voro reads it for one thing only — whether the session
    /// is held at a usage cap ([`crate::read_cap`]) — so an agent that cannot
    /// produce output for a session simply omits it, and Voro classifies a dead
    /// session from the launch log as it always has and badges no live one.
    logs: Option<String>,
    /// Retire a session from the agent's own registry, carrying
    /// [`SESSION_PLACEHOLDER`]: fired when Voro closes the session's row, so the
    /// agent's listing converges on work actually in flight (DESIGN.md §8).
    /// Fire-and-forget — Voro reads neither its output nor its status — and
    /// wholly optional, since an agent that keeps no registry has nothing to
    /// retire and one that keeps a listing it never prunes simply lingers as it
    /// always did.
    stop: Option<String>,
    /// An interactive foreground command carrying [`PROMPT_FILE_PLACEHOLDER`],
    /// run by the TUI's planning flow (DESIGN.md §8) — no `{session}`, since a
    /// planning session belongs to no task or session row.
    plan: Option<String>,
    /// The agent-opaque model name substituted into [`MODEL_PLACEHOLDER`]: the
    /// workhorse this agent runs work with, and the fallback for the two keys
    /// below. Required once any template carries the placeholder.
    model: Option<String>,
    /// The stronger model a `deep` task dispatches with (DESIGN.md §8),
    /// falling back to `model`.
    model_deep: Option<String>,
    /// The model the `plan` verb reasons with, falling back to `model`.
    model_plan: Option<String>,
}

impl AgentTemplate {
    /// The dispatch command — `dispatch`, or its legacy alias `cmd`.
    /// Presence of exactly one is enforced at parse time.
    pub fn dispatch(&self) -> &str {
        self.dispatch
            .as_deref()
            .or(self.cmd.as_deref())
            .expect("parse validates that dispatch or cmd is set")
    }

    pub fn sessions(&self) -> Option<&str> {
        self.sessions.as_deref()
    }

    pub fn attach(&self) -> Option<&str> {
        self.attach.as_deref()
    }

    pub fn resume(&self) -> Option<&str> {
        self.resume.as_deref()
    }

    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    pub fn logs(&self) -> Option<&str> {
        self.logs.as_deref()
    }

    pub fn stop(&self) -> Option<&str> {
        self.stop.as_deref()
    }

    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn model_deep(&self) -> Option<&str> {
        self.model_deep.as_deref()
    }

    pub fn model_plan(&self) -> Option<&str> {
        self.model_plan.as_deref()
    }

    /// The optional verbs this agent defines, in roster order, as `agent list`
    /// and the Config screen name them (DESIGN.md §8). A `message` that carries
    /// [`NEW_SESSION_PLACEHOLDER`] reads `message(fork)`, because forking is
    /// what a send into a supervisor-held session needs and it moves the
    /// session reference the row afterwards addresses.
    pub fn verbs(&self) -> Vec<&'static str> {
        OPTIONAL_VERBS
            .iter()
            .filter_map(|(verb, defined)| {
                let template = defined(self)?;
                Some(
                    if *verb == "message" && template.contains(NEW_SESSION_PLACEHOLDER) {
                        "message(fork)"
                    } else {
                        *verb
                    },
                )
            })
            .collect()
    }
}

/// A verb's name beside the accessor for its template.
type VerbAccessor = (&'static str, fn(&AgentTemplate) -> Option<&str>);

/// Every verb an agent may define beyond `dispatch`, in the order they are
/// listed to the operator. One roster serves both the positive listing and the
/// dropped-verb warning under it, so the two lines cannot disagree about the
/// same agent.
const OPTIONAL_VERBS: [VerbAccessor; 7] = [
    ("sessions", AgentTemplate::sessions),
    ("attach", AgentTemplate::attach),
    ("resume", AgentTemplate::resume),
    ("message", AgentTemplate::message),
    ("logs", AgentTemplate::logs),
    ("stop", AgentTemplate::stop),
    ("plan", AgentTemplate::plan),
];

/// What a launch *is* (DESIGN.md §8): the one place a backgrounded or
/// foreground agent session's identity is composed. A launch names its session,
/// its prompt and log files, and its line in the launch log from this single
/// value, so a new flavour of launch cannot inherit one of those and forget
/// another — which is exactly how a refine came to be named `voro-{task_id}`,
/// literally, on every task at once.
///
/// The invariant it carries: every session Voro launches has a Voro-composed
/// name starting `voro-<id>` — a dispatch continues into a slug of the task's
/// title, anything else pointed at that task into its kind — so nothing Voro
/// starts shows up anonymous or duplicately named in the agent's own session
/// listing. A launch that belongs to no task is named for its project — by
/// name, not id, since a bare number there would read as a task id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// A task dispatched to a headless session (DESIGN.md §8), carrying the
    /// task's title so the session name can say what it is working on.
    Dispatch { task_id: i64, title: String },
    /// A proposed task's body rewritten by an agent (DESIGN.md §6), at either
    /// intensity — the headless note-driven one and the interactive one are the
    /// same operation, and only one of them is ever backgrounded.
    Refine { task_id: i64 },
    /// An interactive planning session drafting a new task for a project,
    /// carrying the project's name. Project names are unique (schema §5), so
    /// two projects cannot claim one session name.
    Plan { project: String },
    /// A headless agent expanding the operator's one-line intent into a task it
    /// files itself (DESIGN.md §6/§8). Like a planning session it belongs to no
    /// task — it is drafting one — so it names its project, by name, and the
    /// two task-less launches share that one convention: a bare number in a
    /// Voro-composed session name is always a task id.
    Propose { project: String },
}

impl Launch {
    /// The name the agent's session carries, filling [`SESSION_NAME_PLACEHOLDER`].
    /// `voro-<id>-<slug>` for a dispatch is the published contract
    /// (docs/agent-integration.md) and what `attach` and the `/resume` picker
    /// are read by, so anything else pointed at the same task suffixes a kind
    /// rather than colliding with it. The slug says what the task *is*, which
    /// is all the operator gets in the agents view and on the phone; a title
    /// that survives sanitization to nothing leaves the bare `voro-<id>`.
    pub fn session_name(&self) -> String {
        match self {
            Launch::Dispatch { task_id, title } => match title_slug(title) {
                Some(slug) => format!("voro-{task_id}-{slug}"),
                None => format!("voro-{task_id}"),
            },
            Launch::Refine { task_id } => format!("voro-{task_id}-refine"),
            Launch::Plan { project } => format!("voro-plan-{}", sanitize_for_name(project)),
            Launch::Propose { project } => format!("voro-propose-{}", sanitize_for_name(project)),
        }
    }

    /// The stem of this launch's prompt and log files, and the label its
    /// launch-log lines carry.
    pub fn slug(&self) -> String {
        match self {
            Launch::Dispatch { task_id, .. } => format!("task-{task_id}"),
            Launch::Refine { task_id } => format!("refine-{task_id}"),
            Launch::Plan { project } => format!("plan-{}", sanitize_for_name(project)),
            Launch::Propose { project } => format!("propose-{}", sanitize_for_name(project)),
        }
    }

    /// The task this launch is pointed at, if any — `None` for the two launches
    /// that draft a task rather than naming one, and why
    /// [`TASK_ID_PLACEHOLDER`] is refused on the `plan` verb.
    pub fn task_id(&self) -> Option<i64> {
        match self {
            Launch::Dispatch { task_id, .. } | Launch::Refine { task_id } => Some(*task_id),
            Launch::Plan { .. } | Launch::Propose { .. } => None,
        }
    }
}

/// Reduce a free-text name to what a session name and a filename can both
/// safely carry: every character outside `[A-Za-z0-9._-]` becomes `-`. A
/// session name is substituted into a shell command line and its slug becomes a
/// filename, so a project called `my stuff` or `it's "fine"` must not reach
/// either as written. Case is preserved, so `voro-plan-ODM` stays readable.
/// Two names that reduce to the same string is a collision Voro accepts:
/// project names are unique, and `a b` alongside `a-b` is not a real case.
fn sanitize_for_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The kind suffixes a per-task session name may carry after `voro-<id>`. A
/// dispatch of a task titled "Refine the scheduler" must not slug down to
/// `voro-<id>-refine` and land on the name that task's own refine round would
/// take.
const RESERVED_NAME_SUFFIXES: &[&str] = &["refine"];

/// How long a dispatch's title slug may grow before it stops taking words. The
/// agents view and the phone's session list both truncate, so the first words
/// have to carry the meaning and the rest are noise. Around twenty characters:
/// the width that takes the three-word phrase most titles open with.
const TITLE_SLUG_BUDGET: usize = 24;

/// Reduce a task title to the tail of its session name: whole words from the
/// front, lowercased and sanitized the same way a project name is, joined with
/// `-` and stopped before the budget is exceeded. Always at least one word,
/// even an over-long one — a name cut mid-word reads as a different task.
/// `None` where nothing survives, which a title of punctuation or of a script
/// outside `[A-Za-z0-9._-]` produces; the caller falls back to the bare
/// `voro-<id>`.
fn title_slug(title: &str) -> Option<String> {
    let words: Vec<String> = title
        .split_whitespace()
        .map(|word| tidy_dashes(&sanitize_for_name(&word.to_lowercase())))
        .filter(|word| !word.is_empty())
        .collect();

    let mut slug = String::new();
    let mut taken = 0;
    for word in &words {
        if !slug.is_empty() && slug.len() + 1 + word.len() > TITLE_SLUG_BUDGET {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(word);
        taken += 1;
    }

    if RESERVED_NAME_SUFFIXES.contains(&slug.as_str()) {
        // Over budget by a word, which beats colliding with a kind name; a
        // title with no further word to give falls back to the bare name.
        let next = words.get(taken)?;
        slug.push('-');
        slug.push_str(next);
    }

    (!slug.is_empty()).then_some(slug)
}

/// Collapse runs of `-` and drop them from both ends. A title is prose rather
/// than a handle, so sanitizing its punctuation leaves dashes where a project
/// name would never have them: `it's "fine"` reads better as `it-s-fine` than
/// as `it-s--fine-`.
fn tidy_dashes(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if c == '-' && (out.is_empty() || out.ends_with('-')) {
            continue;
        }
        out.push(c);
    }
    out.trim_end_matches('-').to_string()
}

/// Everything a verb template needs bound to become a command line: which
/// launch this is, the prompt file written for it, and whether the task earns
/// the deeper model. Assembled by the caller that wrote the prompt, rendered by
/// [`ResolvedAgent::launch_command`] or
/// [`ResolvedAgent::plan_launch_command`](ResolvedAgent::plan_launch_command).
#[derive(Debug, Clone)]
pub struct LaunchSpec<'a> {
    pub launch: Launch,
    pub prompt_file: &'a Path,
    /// Whether the task carries the `deep` flag; ignored by the plan template,
    /// which has no depth to read.
    pub deep: bool,
}

/// Bind every launch placeholder a verb template may carry, in one pass, so no
/// value's own braces are re-scanned. `{task_id}` goes unbound for a launch that
/// has none, which only a `plan` template could contain — and that is refused at
/// config load.
fn render_launch(template: &str, spec: &LaunchSpec, model: Option<&str>) -> String {
    let prompt_file = shell_quote(spec.prompt_file);
    let session_name = spec.launch.session_name();
    let task_id = spec.launch.task_id().map(|id| id.to_string());
    let mut bindings = vec![
        (PROMPT_FILE_PLACEHOLDER, prompt_file.as_str()),
        (SESSION_NAME_PLACEHOLDER, session_name.as_str()),
    ];
    if let Some(task_id) = &task_id {
        bindings.push((TASK_ID_PLACEHOLDER, task_id.as_str()));
    }
    if let Some(model) = model {
        bindings.push((MODEL_PLACEHOLDER, model));
    }
    render(template, &bindings)
}

/// A `message` template rendered into a runnable command line, plus the
/// reference the session will answer to afterwards where the agent forks
/// ([`NEW_SESSION_PLACEHOLDER`]). The caller records that reference only once
/// the send is under way, so a command that never ran leaves the session
/// pointing where it did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    pub command: String,
    /// The fresh reference bound to `{new_session}`, or `None` for a template
    /// that resumes its session in place.
    pub new_session_ref: Option<String>,
}

/// Bind a session verb's one placeholder: the reference Voro captured at
/// launch, shell-quoted so a reference carrying shell metacharacters reaches
/// the agent as itself. Serves `logs`, whose whole contract is a session in and
/// that session's recent output out.
pub fn render_session(template: &str, session_ref: &str) -> String {
    let session = shell_quote(Path::new(session_ref));
    render(template, &[(SESSION_PLACEHOLDER, session.as_str())])
}

/// Bind a `message` template's placeholders in one pass, so no value's own
/// braces are re-scanned: the session reference Voro captured at dispatch, the
/// file holding the message, and — for a template that forks — a freshly
/// generated v4 UUID for the session the send opens. All are shell-quoted; the
/// references are agent-opaque text, not tokens Voro may assume are bare.
pub fn render_message(template: &str, session_ref: &str, prompt_file: &Path) -> RenderedMessage {
    let session = shell_quote(Path::new(session_ref));
    let prompt_file = shell_quote(prompt_file);
    let new_session_ref = template
        .contains(NEW_SESSION_PLACEHOLDER)
        .then(|| uuid::Uuid::new_v4().to_string());
    let new_session = new_session_ref
        .as_deref()
        .map(|r| shell_quote(Path::new(r)));
    let mut bindings = vec![
        (SESSION_PLACEHOLDER, session.as_str()),
        (PROMPT_FILE_PLACEHOLDER, prompt_file.as_str()),
    ];
    if let Some(new_session) = &new_session {
        bindings.push((NEW_SESSION_PLACEHOLDER, new_session.as_str()));
    }
    RenderedMessage {
        command: render(template, &bindings),
        new_session_ref,
    }
}

/// A viewer command template from `voro.toml` (DESIGN.md §11a): a shell command
/// run in a task's checkout — or its worktree — to open its diff. Defined as a
/// named `[viewers.<name>]` table or the anonymous `[viewer]` default. The
/// placeholders `{path}` (checkout/worktree dir), `{branch}` (the task's
/// branch), and `{base}` (the checkout's default branch) are all optional, so
/// nothing is validated at parse time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTemplate {
    pub cmd: String,
}

/// Where an effective agent came from once the built-ins and `voro.toml`
/// are layered, surfaced by `voro agent list` so it is clear which half of
/// the config owns each agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Ships with the binary; no user file mentions it.
    BuiltIn,
    /// Defined only in the user's `voro.toml`.
    User,
    /// A user table that replaces a built-in of the same name wholesale.
    UserOverride,
}

impl Provenance {
    /// A short label for `agent list`.
    pub fn label(self) -> &'static str {
        match self {
            Provenance::BuiltIn => "built-in",
            Provenance::User => "user",
            Provenance::UserOverride => "user override",
        }
    }
}

/// The raw shape deserialized from `voro.toml` (or the built-in TOML) before
/// layering. Every field is optional, so a file that only sets `[viewer]`, only
/// adds an agent, or is empty all parse.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    default_agent: Option<String>,
    #[serde(default)]
    agents: BTreeMap<String, AgentTemplate>,
    #[serde(default)]
    viewer: Option<ViewerTemplate>,
    #[serde(default)]
    viewers: BTreeMap<String, ViewerTemplate>,
    #[serde(default)]
    default_viewer: Option<String>,
    #[serde(default)]
    max_running: Option<i64>,
    #[serde(default)]
    costs: Option<RawCosts>,
}

/// The `[costs]` table (DESIGN.md §7): per-action overrides of the attention
/// price band. Every key is optional and falls back to the built-in default,
/// so a table naming one action leaves the rest alone.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCosts {
    answer: Option<f64>,
    triage: Option<f64>,
    dispatch: Option<f64>,
    review: Option<f64>,
    /// Spelled `do` in the file, after the verb a human task's row asks for.
    #[serde(rename = "do")]
    human_do: Option<f64>,
}

impl RawCosts {
    /// Layer the file's overrides onto the defaults, rejecting a divisor that
    /// would invert or blow up the ranking.
    fn resolve(self, path: &Path) -> Result<AttentionCosts> {
        let defaults = AttentionCosts::default();
        let checked = |name: &str, value: Option<f64>, default: f64| -> Result<f64> {
            match value {
                None => Ok(default),
                Some(value) if value.is_finite() && value > 0.0 => Ok(value),
                Some(value) => Err(Error::AgentConfigInvalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "cost '{name}' is {value} — every [costs] divisor must be a positive \
                         number (the defaults sit between 0.8 and 1.8)"
                    ),
                }),
            }
        };
        Ok(AttentionCosts {
            answer: checked("answer", self.answer, defaults.answer)?,
            triage: checked("triage", self.triage, defaults.triage)?,
            dispatch: checked("dispatch", self.dispatch, defaults.dispatch)?,
            review: checked("review", self.review, defaults.review)?,
            human_do: checked("do", self.human_do, defaults.human_do)?,
        })
    }
}

/// Validate one agent's verb templates, shared by the built-ins and the user
/// file. `dispatch` (or its alias `cmd`) must be present and carry the
/// prompt-file placeholder; the session verbs carry their placeholders when
/// present.
fn validate_agent(name: &str, agent: &AgentTemplate, path: &Path) -> Result<()> {
    let invalid = |message: String| Error::AgentConfigInvalid {
        path: path.to_path_buf(),
        message,
    };
    let dispatch = match (&agent.dispatch, &agent.cmd) {
        (Some(_), Some(_)) => {
            return Err(invalid(format!(
                "agent '{name}' sets both dispatch and cmd — cmd is an alias for \
                 dispatch, keep one"
            )));
        }
        (Some(d), None) => d,
        (None, Some(c)) => c,
        (None, None) => {
            return Err(invalid(format!(
                "agent '{name}' is missing a dispatch (or cmd) template"
            )));
        }
    };
    if !dispatch.contains(PROMPT_FILE_PLACEHOLDER) {
        return Err(invalid(format!(
            "agent '{name}' cmd is missing the {PROMPT_FILE_PLACEHOLDER} placeholder"
        )));
    }
    for (verb, template) in [
        ("attach", &agent.attach),
        ("resume", &agent.resume),
        ("message", &agent.message),
        ("logs", &agent.logs),
        ("stop", &agent.stop),
    ] {
        if let Some(template) = template
            && !template.contains(SESSION_PLACEHOLDER)
        {
            return Err(invalid(format!(
                "agent '{name}' {verb} is missing the {SESSION_PLACEHOLDER} placeholder"
            )));
        }
    }
    // `message` carries a prompt as well as a session: it says something into
    // an existing conversation, so it needs both halves.
    for (verb, template) in [("plan", &agent.plan), ("message", &agent.message)] {
        if let Some(template) = template
            && !template.contains(PROMPT_FILE_PLACEHOLDER)
        {
            return Err(invalid(format!(
                "agent '{name}' {verb} is missing the {PROMPT_FILE_PLACEHOLDER} placeholder"
            )));
        }
    }
    // `{model}`, `{session_name}` and `{task_id}` are all resolved only where a
    // command launches work, so they are meaningful on `dispatch` and `plan`
    // and nowhere else; on a session verb they would reach the shell as literal
    // braces. No launch placeholder may survive to a command line: either a
    // renderer binds it or it is refused here.
    for (verb, template) in [
        ("sessions", &agent.sessions),
        ("attach", &agent.attach),
        ("resume", &agent.resume),
        ("message", &agent.message),
        ("logs", &agent.logs),
        ("stop", &agent.stop),
    ] {
        let Some(template) = template else { continue };
        if template.contains(MODEL_PLACEHOLDER) {
            return Err(invalid(format!(
                "agent '{name}' {verb} carries {MODEL_PLACEHOLDER}, which is resolved only on \
                 dispatch and plan — a session verb reuses the model its session started with"
            )));
        }
        for placeholder in [SESSION_NAME_PLACEHOLDER, TASK_ID_PLACEHOLDER] {
            if template.contains(placeholder) {
                return Err(invalid(format!(
                    "agent '{name}' {verb} carries {placeholder}, which is resolved only on \
                     dispatch and plan — a session verb names its session with {SESSION_PLACEHOLDER}, \
                     the reference Voro captured at launch"
                )));
            }
        }
    }
    // `{new_session}` names the session a *send* opens, so `message` is the one
    // verb that can bind it; anywhere else it would reach the shell as literal
    // braces.
    for (verb, template) in [
        ("dispatch", Some(dispatch.as_str())),
        ("sessions", agent.sessions.as_deref()),
        ("attach", agent.attach.as_deref()),
        ("resume", agent.resume.as_deref()),
        ("logs", agent.logs.as_deref()),
        ("stop", agent.stop.as_deref()),
        ("plan", agent.plan.as_deref()),
    ] {
        if template.is_some_and(|t| t.contains(NEW_SESSION_PLACEHOLDER)) {
            return Err(invalid(format!(
                "agent '{name}' {verb} carries {NEW_SESSION_PLACEHOLDER}, which is bound only on \
                 message — it names the session a headless send forks into"
            )));
        }
    }
    // `plan` serves a target that has no task: a planning session drafts a task
    // rather than naming one, so `{task_id}` there has nothing to bind to. A
    // template must render for every target its verb serves.
    if let Some(template) = &agent.plan
        && template.contains(TASK_ID_PLACEHOLDER)
    {
        return Err(invalid(format!(
            "agent '{name}' plan carries {TASK_ID_PLACEHOLDER}, but a planning session drafts a \
             task rather than naming one — use {SESSION_NAME_PLACEHOLDER}, which Voro composes \
             for every launch"
        )));
    }
    // The model keys are inert without the placeholder (a wholesale override
    // that drops `{model}` keeps loading), but the placeholder without them
    // has nothing to resolve to.
    if agent.model.is_none()
        && [dispatch.as_str(), agent.plan.as_deref().unwrap_or_default()]
            .iter()
            .any(|t| t.contains(MODEL_PLACEHOLDER))
    {
        return Err(invalid(format!(
            "agent '{name}' uses {MODEL_PLACEHOLDER} but sets no model — add model = \
             \"<model name>\" to its [agents.{name}] table (optionally model_deep for deep \
             tasks and model_plan for planning), or drop the placeholder"
        )));
    }
    Ok(())
}

/// Whether an executable named `name` is on `PATH`, for picking a default agent
/// when the user file names none. The probe is by agent name, which for the
/// built-ins is also the binary name.
fn binary_on_path(name: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(name).is_file())
}

/// The first built-in viewer installed, the last resort of viewer resolution
/// (DESIGN.md §11a). Probed by viewer name, which for the built-ins is also the
/// binary name — a user table overriding one keeps that name, so an override
/// changes what runs, not whether the probe finds it.
fn probed_builtin_viewer(probe: &dyn Fn(&str) -> bool) -> Option<&'static str> {
    BUILTIN_VIEWER_NAMES.into_iter().find(|name| probe(name))
}

/// The agent a task will be dispatched with: the task's own override if it
/// has one, otherwise the config's global default, with every verb template
/// resolved.
///
/// The `dispatch` and `plan` fields still hold their placeholders unresolved,
/// because what they bind to depends on the launch: reach them through
/// [`launch_command`](Self::launch_command) and
/// [`plan_launch_command`](Self::plan_launch_command), which return a command
/// line with nothing left to substitute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub name: String,
    pub dispatch: String,
    pub sessions: Option<String>,
    pub attach: Option<String>,
    pub resume: Option<String>,
    pub message: Option<String>,
    pub logs: Option<String>,
    pub stop: Option<String>,
    pub plan: Option<String>,
    pub model: Option<String>,
    pub model_deep: Option<String>,
    pub model_plan: Option<String>,
}

impl ResolvedAgent {
    /// The dispatch template rendered into a runnable command line: the prompt
    /// file shell-quoted, the launch's session name and task id bound, and
    /// `{model}` resolved for the task's depth (DESIGN.md §8) — `model_deep` for
    /// a deep task, falling back to `model` when the agent names no deeper one,
    /// and `model` otherwise. An agent whose template carries no placeholder
    /// renders the same string either way, the graceful degradation of the
    /// `deep` flag.
    pub fn launch_command(&self, spec: &LaunchSpec) -> String {
        let model = if spec.deep {
            self.model_deep.as_deref().or(self.model.as_deref())
        } else {
            self.model.as_deref()
        };
        render_launch(&self.dispatch, spec, model)
    }

    /// Which liveness source a session launched through this agent's
    /// `dispatch` template must be read by (DESIGN.md §8), recorded on the
    /// session row at launch. An agent defining a `sessions` verb is one whose
    /// launch may hand the work to a supervisor — `claude --bg` does — leaving
    /// Voro holding a launcher pid that dies at birth, so its listing is the
    /// only source that can answer. An agent without the verb has no listing to
    /// consult, and its spawned pid is all there is.
    ///
    /// This is the *headless* launch's answer, which the interactive `plan`
    /// verb does not share: that one is a foreground child Voro owns, so its
    /// caller records [`LivenessSource::Pid`] itself.
    pub fn dispatch_liveness_source(&self) -> LivenessSource {
        match self.sessions {
            Some(_) => LivenessSource::Listing,
            None => LivenessSource::Pid,
        }
    }

    /// The plan template rendered the same way, when the agent defines the
    /// verb, with `{model}` resolved to `model_plan` falling back to `model`.
    /// Planning has no depth: it is interactive reasoning either way, so
    /// `spec.deep` is not read.
    pub fn plan_launch_command(&self, spec: &LaunchSpec) -> Option<String> {
        let model = self.model_plan.as_deref().or(self.model.as_deref());
        self.plan
            .as_deref()
            .map(|template| render_launch(template, spec, model))
    }
}

/// The effective agent config: the built-in agents with any `voro.toml`
/// merged on top, plus the user's `default_agent` and viewers. Each agent
/// carries its [`Provenance`] so `agent list` can show where it came from.
#[derive(Debug, Clone)]
pub struct AgentsConfig {
    /// The user-set `default_agent`, if any; `None` falls back to a PATH probe.
    default: Option<String>,
    agents: BTreeMap<String, AgentTemplate>,
    provenance: BTreeMap<String, Provenance>,
    /// The anonymous `[viewer]` table — the pre-names single viewer, still
    /// honoured as a default when no `default_viewer` is set.
    viewer: Option<ViewerTemplate>,
    /// The named `[viewers.<name>]` tables a project can pick from
    /// (DESIGN.md §8/§11a).
    viewers: BTreeMap<String, ViewerTemplate>,
    /// The user-set `default_viewer`, naming a `[viewers.*]` entry.
    default_viewer: Option<String>,
    /// The attention price band the queue ranks by (DESIGN.md §7), defaults
    /// with any `[costs]` overrides layered on.
    costs: AttentionCosts,
    /// How many dispatches ride at once before the queue stops offering more.
    max_running: i64,
    path: PathBuf,
}

/// The config filename under the `voro/` config directory.
const CONFIG_FILENAME: &str = "voro.toml";

impl AgentsConfig {
    /// The config path dispatch reads: `$XDG_CONFIG_HOME/voro/voro.toml`,
    /// defaulting to `~/.config`. A fresh install resolves here even before
    /// the file exists — that is the path `agent init` writes.
    pub fn default_path() -> PathBuf {
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default();
                home.join(".config")
            });
        config_home.join("voro").join(CONFIG_FILENAME)
    }

    /// Load the effective config: the built-in agents, with the user file
    /// layered on top if it exists. A missing file is not an error — the
    /// built-ins alone dispatch — so a fresh install needs no `agent init`.
    pub fn load(path: &Path) -> Result<AgentsConfig> {
        match std::fs::read_to_string(path) {
            Ok(text) => AgentsConfig::parse(&text, path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(AgentsConfig::builtin_only(path))
            }
            Err(e) => Err(Error::AgentConfigInvalid {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        }
    }

    /// The built-in agents alone, with no user file layered on. Used when
    /// the config file is absent.
    fn builtin_only(path: &Path) -> AgentsConfig {
        let agents = builtin_agents().clone();
        let provenance = agents
            .keys()
            .map(|name| (name.clone(), Provenance::BuiltIn))
            .collect();
        AgentsConfig {
            default: None,
            agents,
            provenance,
            viewer: None,
            viewers: BTreeMap::new(),
            default_viewer: None,
            costs: AttentionCosts::default(),
            max_running: DEFAULT_MAX_RUNNING,
            path: path.to_path_buf(),
        }
    }

    /// Parse the user file's text and layer it over the built-ins: a user
    /// table replaces a built-in of the same name wholesale (whole-agent
    /// override), otherwise it adds a new agent. `default_agent`/`[viewer]`
    /// come from the file.
    fn parse(text: &str, path: &Path) -> Result<AgentsConfig> {
        let raw: RawConfig = toml::from_str(text).map_err(|e| Error::AgentConfigInvalid {
            path: path.to_path_buf(),
            message: e.message().to_string(),
        })?;
        for (name, agent) in &raw.agents {
            validate_agent(name, agent, path)?;
        }
        let mut agents = builtin_agents().clone();
        let mut provenance: BTreeMap<String, Provenance> = agents
            .keys()
            .map(|name| (name.clone(), Provenance::BuiltIn))
            .collect();
        for (name, agent) in raw.agents {
            let prov = if builtin_agents().contains_key(&name) {
                Provenance::UserOverride
            } else {
                Provenance::User
            };
            provenance.insert(name.clone(), prov);
            agents.insert(name, agent);
        }
        let max_running = match raw.max_running {
            None => DEFAULT_MAX_RUNNING,
            Some(n) if n >= 0 => n,
            Some(n) => {
                return Err(Error::AgentConfigInvalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "max_running is {n} — it counts dispatches in flight, so it cannot be \
                         negative (0 stops the queue offering dispatches at all)"
                    ),
                });
            }
        };
        let costs = match raw.costs {
            Some(costs) => costs.resolve(path)?,
            None => AttentionCosts::default(),
        };
        Ok(AgentsConfig {
            default: raw.default_agent,
            agents,
            provenance,
            viewer: raw.viewer,
            viewers: raw.viewers,
            default_viewer: raw.default_viewer,
            costs,
            max_running,
            path: path.to_path_buf(),
        })
    }

    /// The attention price band the queue ranks by (DESIGN.md §7).
    pub fn costs(&self) -> AttentionCosts {
        self.costs
    }

    /// The dispatch WIP cap (DESIGN.md §7): how many tasks may be running
    /// before the queue stops offering dispatches.
    pub fn max_running(&self) -> i64 {
        self.max_running
    }

    /// Every agent name defined in the config, for the TUI's dispatch picker
    /// (DESIGN.md §8/§9). `agents` is a `BTreeMap`, so this is already sorted.
    pub fn agent_names(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }

    /// The verb templates of a named agent, if it is configured. Used where a
    /// session already records which agent ran it — jump-in, reconciliation —
    /// so no default/override resolution applies.
    pub fn agent(&self, name: &str) -> Option<&AgentTemplate> {
        self.agents.get(name)
    }

    /// The agent for a task: its `agent` override if set, otherwise the
    /// resolved default (§8). An override or default naming an agent absent
    /// from the config is an error here, not a panic at spawn time.
    pub fn resolve(&self, task_override: Option<&str>) -> Result<ResolvedAgent> {
        self.resolve_with(task_override, &binary_on_path)
    }

    /// [`resolve`](Self::resolve) with an injectable PATH probe, so the
    /// default-resolution path is testable without depending on what happens
    /// to be installed.
    fn resolve_with(
        &self,
        task_override: Option<&str>,
        probe: &dyn Fn(&str) -> bool,
    ) -> Result<ResolvedAgent> {
        let (name, origin) = match task_override {
            Some(name) => (name.to_string(), "task agent override"),
            None => (self.effective_default(probe)?, "config default"),
        };
        let agent = self.agents.get(&name).ok_or_else(|| Error::UnknownAgent {
            name: name.clone(),
            origin,
            path: self.path.clone(),
            known: self.agents.keys().cloned().collect::<Vec<_>>().join(", "),
        })?;
        Ok(ResolvedAgent {
            name,
            dispatch: agent.dispatch().to_string(),
            sessions: agent.sessions.clone(),
            attach: agent.attach.clone(),
            resume: agent.resume.clone(),
            message: agent.message.clone(),
            logs: agent.logs.clone(),
            stop: agent.stop.clone(),
            plan: agent.plan.clone(),
            model: agent.model.clone(),
            model_deep: agent.model_deep.clone(),
            model_plan: agent.model_plan.clone(),
        })
    }

    /// The default agent's name: the user's `default` when set (honoured even
    /// if it names a missing agent, so `resolve` reports the mismatch), else
    /// the first built-in found on PATH. Errors with guidance when neither
    /// yields anything.
    fn effective_default(&self, probe: &dyn Fn(&str) -> bool) -> Result<String> {
        if let Some(default) = &self.default {
            return Ok(default.clone());
        }
        for candidate in DEFAULT_PROBE_ORDER {
            if self.agents.contains_key(candidate) && probe(candidate) {
                return Ok(candidate.to_string());
            }
        }
        Err(Error::NoDefaultAgent {
            probed: DEFAULT_PROBE_ORDER.join(", "),
            path: self.path.clone(),
        })
    }

    /// The names of the user's `[viewers.*]` tables, sorted: the *editable*
    /// set, which is why the built-ins are not in it. Everything that offers a
    /// viewer to run — the TUI's viewer picker, `viewer list` — wants
    /// [`viewer_entries`](Self::viewer_entries) instead.
    pub fn viewer_names(&self) -> Vec<String> {
        self.viewers.keys().cloned().collect()
    }

    /// Every effective viewer as `(name, cmd, provenance)`, sorted by name —
    /// the built-ins with the user's tables layered over them, mirroring the
    /// agents' [`entries`](Self::entries). What `viewer list`, the Config
    /// screen and the default/review-action pickers show, since a built-in is
    /// a legitimate thing to run, star, or pin a project to.
    pub fn viewer_entries(&self) -> Vec<(&str, &str, Provenance)> {
        let mut merged: BTreeMap<&str, (&str, Provenance)> = builtin_viewers()
            .iter()
            .map(|(name, viewer)| (name.as_str(), (viewer.cmd.as_str(), Provenance::BuiltIn)))
            .collect();
        for (name, viewer) in &self.viewers {
            let prov = if is_builtin_viewer(name) {
                Provenance::UserOverride
            } else {
                Provenance::User
            };
            merged.insert(name.as_str(), (viewer.cmd.as_str(), prov));
        }
        merged
            .into_iter()
            .map(|(name, (cmd, prov))| (name, cmd, prov))
            .collect()
    }

    /// The anonymous `[viewer]` table's command, if the file defines one — the
    /// legacy default the Config screen surfaces read-only, since it carries no
    /// name to edit or delete by.
    pub fn anonymous_viewer_cmd(&self) -> Option<&str> {
        self.viewer.as_ref().map(|v| v.cmd.as_str())
    }

    /// The name of the viewer `open` will run when nothing picks one by name,
    /// for `viewer list` and the Config screen to star: the user's
    /// `default_viewer` when set (honoured even if it names a missing viewer),
    /// else the sole `[viewers.*]` entry, else the first built-in found on
    /// PATH. The anonymous `[viewer]` table has no name, so it yields `None`
    /// here even though it resolves.
    pub fn default_viewer_name(&self) -> Option<String> {
        self.default_viewer_name_with(&binary_on_path)
    }

    /// [`default_viewer_name`](Self::default_viewer_name) with an injectable
    /// PATH probe, so the built-in fallback is testable without depending on
    /// what happens to be installed.
    fn default_viewer_name_with(&self, probe: &dyn Fn(&str) -> bool) -> Option<String> {
        if self.default_viewer.is_some() {
            return self.default_viewer.clone();
        }
        if self.viewer.is_some() {
            return None;
        }
        if self.viewers.len() == 1 {
            return self.viewers.keys().next().cloned();
        }
        probed_builtin_viewer(probe).map(str::to_string)
    }

    /// Resolve a viewer command (DESIGN.md §11a). User configuration always
    /// wins: with a name, the `[viewers.<name>]` table, falling back to the
    /// built-in of that name; without one, `default_viewer`, else the anonymous
    /// `[viewer]` table, else the sole `[viewers.*]` entry, else the first
    /// built-in viewer found on PATH. Errors carry what to install or run.
    pub fn viewer_cmd(&self, name: Option<&str>) -> Result<&str> {
        self.viewer_cmd_with(name, &binary_on_path)
    }

    /// [`viewer_cmd`](Self::viewer_cmd) with an injectable PATH probe, so the
    /// resolution order is testable without depending on what happens to be
    /// installed.
    fn viewer_cmd_with(&self, name: Option<&str>, probe: &dyn Fn(&str) -> bool) -> Result<&str> {
        match name {
            Some(name) => self.named_viewer(name),
            None => match &self.default_viewer {
                Some(default) => self.named_viewer(default),
                None => self
                    .viewer
                    .as_ref()
                    .map(|v| v.cmd.as_str())
                    .or_else(|| match self.viewers.len() {
                        1 => self.viewers.values().next().map(|v| v.cmd.as_str()),
                        _ => None,
                    })
                    .or_else(|| {
                        probed_builtin_viewer(probe).and_then(|name| self.named_viewer(name).ok())
                    })
                    .ok_or_else(|| Error::NoViewer {
                        probed: BUILTIN_VIEWER_NAMES.join("/"),
                    }),
            },
        }
    }

    /// A viewer by name: the user's table when it defines one, else the
    /// built-in of that name — the same wholesale override the agents get.
    fn named_viewer(&self, name: &str) -> Result<&str> {
        self.viewers
            .get(name)
            .or_else(|| builtin_viewers().get(name))
            .map(|v| v.cmd.as_str())
            .ok_or_else(|| Error::UnknownViewer {
                name: name.to_string(),
                known: self
                    .viewer_entries()
                    .iter()
                    .map(|(name, _, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", "),
                path: self.path.clone(),
            })
    }

    /// The name of the agent used when a task has no override, for the CLI's
    /// `agent list` to flag it. `None` when no `default` is set and no
    /// built-in is on PATH — the same condition `resolve` errors on.
    pub fn default_name(&self) -> Option<String> {
        self.effective_default(&binary_on_path).ok()
    }

    /// The provenance of a named agent, if it is configured.
    pub fn provenance(&self, name: &str) -> Option<Provenance> {
        self.provenance.get(name).copied()
    }

    /// For a user override of a built-in, the verbs the built-in defines that
    /// the override drops — the one case layering can't fix (§8), so
    /// `agent list` can warn that those verbs stopped working. Empty for
    /// built-in or purely-additive user agents.
    pub fn override_missing_verbs(&self, name: &str) -> Vec<&'static str> {
        if self.provenance.get(name) != Some(&Provenance::UserOverride) {
            return Vec::new();
        }
        let (Some(user), Some(builtin)) = (self.agents.get(name), builtin_agents().get(name))
        else {
            return Vec::new();
        };
        OPTIONAL_VERBS
            .iter()
            .filter(|(_, defined)| defined(builtin).is_some() && defined(user).is_none())
            .map(|(verb, _)| *verb)
            .collect()
    }

    /// Every agent as `(name, template, provenance)`, sorted by name, for
    /// `agent list`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &AgentTemplate, Provenance)> {
        self.agents.iter().map(|(name, agent)| {
            let prov = self
                .provenance
                .get(name)
                .copied()
                .unwrap_or(Provenance::User);
            (name.as_str(), agent, prov)
        })
    }

    /// Write the [`starter_config`] skeleton to `path`, creating parent
    /// directories. Refuses to overwrite an existing file so a hand-tuned
    /// config is never clobbered.
    pub fn write_starter(path: &Path) -> Result<()> {
        if path.exists() {
            return Err(Error::Invalid(format!(
                "{} already exists; edit it directly rather than reinitialising",
                path.display()
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::AgentConfigInvalid {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        }
        std::fs::write(path, starter_config()).map_err(|e| Error::AgentConfigInvalid {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }
}

/// One session from an agent's `sessions` command output: a JSON array of
/// objects, of which the fields below are read and everything else ignored.
/// `sessionId` (falling back to `id`) is the durable reference substituted
/// into `{session}`; `cwd` and `startedAt` (ms epoch) identify a fresh
/// dispatch's session among its siblings; `state` and `pid` together say
/// whether the session is still going ([`AgentSessionEntry::liveness`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionEntry {
    pub session_ref: String,
    pub short_id: Option<String>,
    pub cwd: Option<String>,
    pub started_at_ms: Option<i64>,
    pub state: Option<String>,
    /// The supervisor process behind this session, where the listing names one.
    /// Authoritative for liveness when present (DESIGN.md §8), since a listing
    /// keeps entries for sessions whose state it never retires.
    pub pid: Option<i64>,
}

/// What a listing entry says about its session, as far as pure logic can tell
/// (DESIGN.md §8). [`AgentSessionEntry::liveness`] classifies; the `voro` crate
/// resolves [`SessionLiveness::WhileProcessLives`] with the process check, so
/// `voro-core` still never touches a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLiveness {
    /// Still going, with nothing left to check.
    Live,
    /// Live exactly while this process exists.
    WhileProcessLives(i64),
    /// No longer running.
    Dead,
}

impl AgentSessionEntry {
    /// Whether this entry is the session a stored reference points at — either
    /// id form matches, since a log-parsed fallback may record the short id.
    pub fn matches_ref(&self, session_ref: &str) -> bool {
        self.session_ref == session_ref || self.short_id.as_deref() == Some(session_ref)
    }

    /// Classify this entry's liveness (DESIGN.md §8). A listing that keeps an
    /// entry forever is the case this exists for: an agent's own listing may
    /// leave a long-dead session sitting at `blocked` and never say `done`, so
    /// not-done cannot mean live. A named `pid` decides it — which keeps a
    /// genuinely stalled session, blocked on a permission prompt with its
    /// supervisor alive, reading as live and attachable. Failing a pid, only
    /// `working` claims the session is going; anything else, including a state
    /// this doesn't recognise or no state at all, reads as dead.
    pub fn liveness(&self) -> SessionLiveness {
        match (self.state.as_deref(), self.pid) {
            (Some("done"), _) => SessionLiveness::Dead,
            (_, Some(pid)) => SessionLiveness::WhileProcessLives(pid),
            (Some("working"), None) => SessionLiveness::Live,
            _ => SessionLiveness::Dead,
        }
    }

    /// Whether this entry says its session's turn has *ended* — the narrow
    /// reading the rest-stop rule acts on (DESIGN.md §8), which is not the same
    /// question as [`liveness`](Self::liveness). Only `done` answers yes.
    /// `blocked` is the case that makes the distinction load-bearing: it reads
    /// dead without a live pid, but it is also what a permission prompt and a
    /// supervisor mid-turn look like, and stopping either would cut a turn off
    /// mid-sentence. Every other state, and an entry with no state at all, is
    /// likewise not a hand-back.
    pub fn at_rest(&self) -> bool {
        self.state.as_deref() == Some("done")
    }
}

/// Parse a `sessions` command's stdout. Entries without any id are skipped
/// rather than failing the whole listing; anything that is not a JSON array
/// is an error, so a misconfigured `sessions` verb surfaces rather than
/// reading as "no sessions".
pub fn parse_sessions_json(json: &str) -> Result<Vec<AgentSessionEntry>> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| Error::Invalid(format!("sessions output is not JSON: {e}")))?;
    let array = value
        .as_array()
        .ok_or_else(|| Error::Invalid("sessions output is not a JSON array".into()))?;
    let mut entries = Vec::new();
    for item in array {
        let get_str = |key: &str| item.get(key).and_then(|v| v.as_str()).map(str::to_string);
        let Some(session_ref) = get_str("sessionId").or_else(|| get_str("id")) else {
            continue;
        };
        entries.push(AgentSessionEntry {
            session_ref,
            short_id: get_str("id"),
            cwd: get_str("cwd"),
            started_at_ms: item.get("startedAt").and_then(|v| v.as_i64()),
            state: get_str("state"),
            pid: item.get("pid").and_then(|v| v.as_i64()),
        });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
        default_agent = "claude"

        [agents.claude]
        cmd = "claude -p --output-format stream-json {prompt_file}"

        [agents.codex]
        cmd = "codex exec {prompt_file}"
    "#;

    fn config() -> AgentsConfig {
        AgentsConfig::parse(CONFIG, Path::new("/tmp/voro.toml")).unwrap()
    }

    fn parse(text: &str) -> Result<AgentsConfig> {
        AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
    }

    /// A PATH probe finding nothing, so a resolution test never depends on
    /// what the developer happens to have installed.
    fn none_installed(_: &str) -> bool {
        false
    }

    /// A PATH probe finding exactly one binary.
    fn only(installed: &'static str) -> impl Fn(&str) -> bool {
        move |name| name == installed
    }

    #[test]
    fn absent_costs_and_max_running_take_the_defaults() {
        // A file that says nothing about pricing prices the queue exactly as
        // the built-ins do (DESIGN.md §7) — as does a missing file.
        for config in [
            config(),
            AgentsConfig::builtin_only(Path::new("/tmp/voro.toml")),
        ] {
            assert_eq!(config.costs(), AttentionCosts::default());
            assert_eq!(config.max_running(), DEFAULT_MAX_RUNNING);
        }
    }

    #[test]
    fn costs_table_overrides_only_the_actions_it_names() {
        let config = parse(
            r#"
            max_running = 3

            [costs]
            review = 2.5
            do = 4.0
            "#,
        )
        .unwrap();
        let costs = config.costs();
        assert_eq!(costs.review, 2.5);
        assert_eq!(costs.human_do, 4.0);
        // untouched keys keep the defaults
        assert_eq!(costs.answer, AttentionCosts::default().answer);
        assert_eq!(costs.triage, AttentionCosts::default().triage);
        assert_eq!(costs.dispatch, AttentionCosts::default().dispatch);
        assert_eq!(config.max_running(), 3);
    }

    #[test]
    fn a_non_positive_cost_is_refused() {
        // A zero or negative divisor would blow up or invert the ranking, so
        // it is caught at load rather than producing a nonsense queue.
        for text in [
            "[costs]\nreview = 0",
            "[costs]\nanswer = -1.0",
            "[costs]\ntriage = nan",
        ] {
            let e = parse(text).unwrap_err().to_string();
            assert!(e.contains("must be a positive number"), "{text}: {e}");
        }
    }

    #[test]
    fn a_negative_max_running_is_refused() {
        let e = parse("max_running = -1").unwrap_err().to_string();
        assert!(e.contains("cannot be negative"), "{e}");
        // zero is legal — it is how the operator stops the queue offering
        // dispatches at all.
        assert_eq!(parse("max_running = 0").unwrap().max_running(), 0);
    }

    #[test]
    fn an_unknown_cost_key_is_refused_rather_than_ignored() {
        let e = parse("[costs]\nredispatch = 1.0").unwrap_err().to_string();
        assert!(e.contains("redispatch"), "{e}");
    }

    #[test]
    fn agent_names_lists_every_configured_agent() {
        assert_eq!(config().agent_names(), vec!["claude", "codex"]);
    }

    #[test]
    fn resolves_default_when_task_has_no_override() {
        let resolved = config().resolve(None).unwrap();
        assert_eq!(resolved.name, "claude");
        assert_eq!(
            resolved.dispatch,
            "claude -p --output-format stream-json {prompt_file}"
        );
    }

    #[test]
    fn task_override_wins_over_default() {
        let resolved = config().resolve(Some("codex")).unwrap();
        assert_eq!(resolved.name, "codex");
        assert_eq!(resolved.dispatch, "codex exec {prompt_file}");
    }

    #[test]
    fn unknown_override_errors_at_resolution() {
        let err = config().resolve(Some("gemini")).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("gemini"), "{message}");
        assert!(message.contains("task agent override"), "{message}");
        assert!(message.contains("claude, codex"), "{message}");
    }

    #[test]
    fn unknown_default_errors_at_resolution() {
        let text = r#"
            default_agent = "gemini"

            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let message = config.resolve(None).unwrap_err().to_string();
        assert!(message.contains("gemini"), "{message}");
        assert!(message.contains("config default"), "{message}");
    }

    #[test]
    fn cmd_without_prompt_file_placeholder_is_rejected() {
        let text = r#"
            default_agent = "claude"

            [agents.claude]
            cmd = "claude -p"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("{prompt_file}"), "{message}");
        assert!(message.contains("claude"), "{message}");
    }

    #[test]
    fn invalid_toml_names_the_file() {
        let message = AgentsConfig::parse("default = ", Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("/tmp/voro.toml"), "{message}");
    }

    #[test]
    fn loads_from_disk() {
        let path = std::env::temp_dir().join(format!("voro-agents-{}.toml", std::process::id()));
        std::fs::write(&path, CONFIG).unwrap();
        let config = AgentsConfig::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(config.resolve(None).unwrap().name, "claude");
    }

    #[test]
    fn missing_file_loads_the_builtins() {
        let config = AgentsConfig::load(Path::new("/nonexistent/voro.toml")).unwrap();
        assert_eq!(config.agent_names(), vec!["claude", "codex"]);
        assert_eq!(config.provenance("claude"), Some(Provenance::BuiltIn));
        let claude = config.agent("claude").unwrap();
        assert!(claude.dispatch().contains("--bg"), "{}", claude.dispatch());
        assert!(claude.sessions().is_some());
        assert!(claude.attach().is_some());
        assert!(claude.resume().is_some());
    }

    #[test]
    fn builtins_layer_under_a_user_file() {
        let text = r#"
            [agents.mycustom]
            dispatch = "mytool {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.agent_names(), vec!["claude", "codex", "mycustom"]);
        assert_eq!(config.provenance("claude"), Some(Provenance::BuiltIn));
        assert_eq!(config.provenance("codex"), Some(Provenance::BuiltIn));
        assert_eq!(config.provenance("mycustom"), Some(Provenance::User));
        assert!(config.agent("claude").unwrap().sessions().is_some());
    }

    #[test]
    fn a_user_table_overrides_a_builtin_wholesale() {
        let text = r#"
            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.provenance("claude"), Some(Provenance::UserOverride));
        let claude = config.agent("claude").unwrap();
        assert_eq!(claude.dispatch(), "claude -p {prompt_file}");
        assert_eq!(claude.sessions(), None, "override is not merged per-verb");
        assert_eq!(claude.attach(), None);
        assert_eq!(config.provenance("codex"), Some(Provenance::BuiltIn));
    }

    #[test]
    fn override_dropping_verbs_is_reported() {
        let text = r#"
            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let missing = config.override_missing_verbs("claude");
        assert!(missing.contains(&"sessions"), "{missing:?}");
        assert!(missing.contains(&"attach"), "{missing:?}");
        assert!(missing.contains(&"resume"), "{missing:?}");
        assert!(config.override_missing_verbs("codex").is_empty());
    }

    #[test]
    fn default_probes_path_when_the_user_sets_none() {
        let config = AgentsConfig::builtin_only(Path::new("/tmp/voro.toml"));
        let only_codex = |name: &str| name == "codex";
        assert_eq!(
            config.resolve_with(None, &only_codex).unwrap().name,
            "codex"
        );
        let both = |_: &str| true;
        assert_eq!(config.resolve_with(None, &both).unwrap().name, "claude");
    }

    #[test]
    fn no_default_and_nothing_on_path_errors_with_guidance() {
        let config = AgentsConfig::builtin_only(Path::new("/tmp/voro.toml"));
        let none = |_: &str| false;
        let message = config.resolve_with(None, &none).unwrap_err().to_string();
        assert!(message.contains("no default agent"), "{message}");
        assert!(message.contains("claude, codex"), "{message}");
    }

    #[test]
    fn user_default_is_honoured_over_the_path_probe() {
        let text = r#"
            default_agent = "codex"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let both = |_: &str| true;
        assert_eq!(config.resolve_with(None, &both).unwrap().name, "codex");
    }

    // --- session verbs ---

    const VERBS_CONFIG: &str = r#"
        default_agent = "claude"

        [agents.claude]
        dispatch = "claude --bg \"$(cat {prompt_file})\""
        sessions = "claude agents --json"
        attach   = "claude attach {session}"
        resume   = "claude --resume {session}"

        [agents.codex]
        dispatch = "codex exec {prompt_file}"
        resume   = "codex resume {session}"
    "#;

    #[test]
    fn verbs_parse_and_resolve() {
        let config = AgentsConfig::parse(VERBS_CONFIG, Path::new("/tmp/voro.toml")).unwrap();
        let claude = config.resolve(None).unwrap();
        assert_eq!(claude.sessions.as_deref(), Some("claude agents --json"));
        assert_eq!(claude.attach.as_deref(), Some("claude attach {session}"));
        assert_eq!(claude.resume.as_deref(), Some("claude --resume {session}"));

        let codex = config.resolve(Some("codex")).unwrap();
        assert_eq!(codex.sessions, None);
        assert_eq!(codex.attach, None);
        assert_eq!(codex.resume.as_deref(), Some("codex resume {session}"));
    }

    #[test]
    fn cmd_alias_behaves_as_dispatch_with_every_verb_absent() {
        let resolved = config().resolve(None).unwrap();
        assert_eq!(
            resolved.dispatch,
            "claude -p --output-format stream-json {prompt_file}"
        );
        assert_eq!(resolved.sessions, None);
        assert_eq!(resolved.attach, None);
        assert_eq!(resolved.resume, None);
    }

    #[test]
    fn both_dispatch_and_cmd_is_rejected() {
        let text = r#"
            default_agent = "claude"

            [agents.claude]
            cmd = "claude -p {prompt_file}"
            dispatch = "claude --bg {prompt_file}"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("alias"), "{message}");
    }

    #[test]
    fn agent_without_dispatch_or_cmd_is_rejected() {
        let text = r#"
            default_agent = "claude"

            [agents.claude]
            sessions = "claude agents --json"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("dispatch"), "{message}");
    }

    #[test]
    fn attach_and_resume_require_the_session_placeholder() {
        for verb in ["attach", "resume"] {
            let text = format!(
                "default_agent = \"a\"\n\n[agents.a]\ndispatch = \"run {{prompt_file}}\"\n\
                 {verb} = \"reopen {{prompt_file}}\"\n"
            );
            let message = AgentsConfig::parse(&text, Path::new("/tmp/voro.toml"))
                .unwrap_err()
                .to_string();
            assert!(message.contains("{session}"), "{verb}: {message}");
            assert!(message.contains(verb), "{verb}: {message}");
        }
    }

    // --- plan verb (task #112) ---

    #[test]
    fn plan_parses_resolves_and_is_optional() {
        let text = r#"
            default_agent = "a"

            [agents.a]
            dispatch = "run {prompt_file}"
            plan = "run --interactive {prompt_file}"

            [agents.b]
            dispatch = "other {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let a = config.resolve(None).unwrap();
        assert_eq!(a.plan.as_deref(), Some("run --interactive {prompt_file}"));
        assert_eq!(config.agent("a").unwrap().plan(), a.plan.as_deref());
        // an agent without the verb resolves with it absent, like the others
        let b = config.resolve(Some("b")).unwrap();
        assert_eq!(b.plan, None);
        assert_eq!(config.agent("b").unwrap().plan(), None);
    }

    #[test]
    fn plan_requires_the_prompt_file_placeholder() {
        let text = r#"
            default_agent = "a"

            [agents.a]
            dispatch = "run {prompt_file}"
            plan = "run --interactive"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("{prompt_file}"), "{message}");
        assert!(message.contains("plan"), "{message}");
    }

    #[test]
    fn builtin_claude_defines_plan_and_an_override_dropping_it_is_reported() {
        let agents = builtin_agents();
        let plan = agents["claude"].plan().unwrap();
        assert!(plan.contains(PROMPT_FILE_PLACEHOLDER), "{plan}");
        assert!(
            !plan.contains("--bg"),
            "plan runs in the foreground: {plan}"
        );
        assert!(agents["codex"].plan().is_none());

        let text = r#"
            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert!(
            config.override_missing_verbs("claude").contains(&"plan"),
            "{:?}",
            config.override_missing_verbs("claude")
        );
    }

    // --- message verb ---

    #[test]
    fn message_parses_resolves_and_is_optional() {
        let text = r#"
            default_agent = "a"

            [agents.a]
            dispatch = "run {prompt_file}"
            message = "say --into {session} {prompt_file}"

            [agents.b]
            dispatch = "other {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let a = config.resolve(None).unwrap();
        assert_eq!(
            a.message.as_deref(),
            Some("say --into {session} {prompt_file}")
        );
        assert_eq!(config.agent("a").unwrap().message(), a.message.as_deref());
        let b = config.resolve(Some("b")).unwrap();
        assert_eq!(b.message, None);
        assert_eq!(config.agent("b").unwrap().message(), None);
    }

    /// A message says something *into a session*, so it needs both halves:
    /// which session, and what to say.
    #[test]
    fn message_requires_both_the_session_and_prompt_file_placeholders() {
        for (template, missing) in [
            ("say {prompt_file}", SESSION_PLACEHOLDER),
            ("say --into {session}", PROMPT_FILE_PLACEHOLDER),
        ] {
            let text = format!(
                "default_agent = \"a\"\n\n[agents.a]\ndispatch = \"run {{prompt_file}}\"\n\
                 message = \"{template}\"\n"
            );
            let e = AgentsConfig::parse(&text, Path::new("/tmp/voro.toml"))
                .unwrap_err()
                .to_string();
            assert!(e.contains(missing), "{template}: {e}");
            assert!(e.contains("message"), "{template}: {e}");
        }
    }

    /// `message` acts on a session that already exists, so it takes the launch
    /// placeholders no more than `attach` and `resume` do.
    #[test]
    fn message_refuses_the_launch_placeholders() {
        for placeholder in [
            MODEL_PLACEHOLDER,
            SESSION_NAME_PLACEHOLDER,
            TASK_ID_PLACEHOLDER,
        ] {
            let text = format!(
                "default_agent = \"a\"\n\n[agents.a]\ndispatch = \"run {{prompt_file}}\"\n\
                 model = \"m\"\n\
                 message = \"say --into {{session}} --as {placeholder} {{prompt_file}}\"\n"
            );
            let e = AgentsConfig::parse(&text, Path::new("/tmp/voro.toml"))
                .unwrap_err()
                .to_string();
            assert!(e.contains(placeholder), "{placeholder}: {e}");
            assert!(e.contains("message"), "{placeholder}: {e}");
        }
    }

    #[test]
    fn builtin_claude_defines_message_and_an_override_dropping_it_is_reported() {
        let agents = builtin_agents();
        let message = agents["claude"].message().unwrap();
        assert!(message.contains(SESSION_PLACEHOLDER), "{message}");
        assert!(message.contains(PROMPT_FILE_PLACEHOLDER), "{message}");
        assert!(
            message.contains("-p"),
            "message is headless, not a terminal round-trip: {message}"
        );
        // codex names no message verb — the graceful-degradation case the TUI
        // reports on its status line.
        assert!(agents["codex"].message().is_none());

        let text = r#"
            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert!(
            config.override_missing_verbs("claude").contains(&"message"),
            "{:?}",
            config.override_missing_verbs("claude")
        );
    }

    /// The listing and the dropped-verb warning read the same roster, so an
    /// agent cannot be listed as lacking a verb the warning says it dropped.
    #[test]
    fn verbs_lists_every_optional_verb_and_marks_a_forking_message() {
        let agents = builtin_agents();
        // The built-in claude resumes in place, so its message is named plainly.
        assert_eq!(
            agents["claude"].verbs(),
            vec![
                "sessions", "attach", "resume", "message", "logs", "stop", "plan"
            ]
        );
        assert_eq!(agents["codex"].verbs(), vec!["resume"]);

        // A message that forks names the session it forks into, and the roster
        // says so — the marking outlives the built-in that used to carry it.
        let text = r#"
            [agents.a]
            dispatch = "run {prompt_file}"
            message = "say --into {session} --as {new_session} {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.agent("a").unwrap().verbs(), vec!["message(fork)"]);
    }

    /// Every verb the warning can name is a verb the listing can name, which is
    /// the invariant that kept the two lines disagreeing before they shared a
    /// roster: a wholesale override of claude that drops everything reports the
    /// same set the built-in row lists.
    #[test]
    fn the_listing_and_the_dropped_verb_warning_cover_the_same_verbs() {
        let text = r#"
            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let dropped = config.override_missing_verbs("claude");
        let listed: Vec<&str> = builtin_agents()["claude"]
            .verbs()
            .into_iter()
            .map(|verb| verb.split('(').next().expect("a verb name"))
            .collect();
        assert_eq!(dropped, listed);
        assert!(config.agent("claude").unwrap().verbs().is_empty());
    }

    #[test]
    fn render_message_binds_both_placeholders_shell_quoted() {
        let rendered = render_message(
            "claude -p --resume {session} \"$(cat {prompt_file})\"",
            "3f6c-1111",
            Path::new("/run/msg-1.md"),
        );
        assert_eq!(
            rendered.command,
            "claude -p --resume '3f6c-1111' \"$(cat '/run/msg-1.md')\""
        );
        // A template that resumes in place keeps the reference it was given.
        assert_eq!(rendered.new_session_ref, None);
    }

    /// A `message` template that forks names the session it forks into, and
    /// Voro supplies that name: a fresh v4 UUID, shell-quoted like the rest,
    /// handed back so the session row can follow the fork (DESIGN.md §8).
    #[test]
    fn render_message_binds_a_fresh_reference_for_a_forking_verb() {
        let rendered = render_message(
            "claude -p --resume {session} --fork-session --session-id {new_session} \
             \"$(cat {prompt_file})\"",
            "3f6c-1111",
            Path::new("/run/msg-1.md"),
        );
        let new_ref = rendered.new_session_ref.expect("a fresh reference");
        assert_ne!(new_ref, "3f6c-1111");
        assert_eq!(new_ref.len(), 36, "a v4 uuid: {new_ref}");
        assert!(
            rendered
                .command
                .contains(&format!("--session-id '{new_ref}'")),
            "{}",
            rendered.command
        );
        // and a second send forks somewhere else again
        let again = render_message(
            "claude --session-id {new_session} --resume {session} {prompt_file}",
            "3f6c-1111",
            Path::new("/run/msg-2.md"),
        );
        assert_ne!(again.new_session_ref, Some(new_ref));
    }

    /// The permission mode belongs to a launch rather than to a verb: every
    /// built-in `claude` template that hands an agent a prompt to act on
    /// carries it, headless or not. `resume` carries none because it carries no
    /// prompt either — it reopens a session for the operator and starts no work
    /// of its own, so the ask-mode default is answerable by the person sitting
    /// in front of it.
    #[test]
    fn every_builtin_claude_launch_that_prompts_carries_a_permission_mode() {
        let claude = &builtin_agents()["claude"];
        for (verb, template) in [
            ("dispatch", claude.dispatch()),
            ("message", claude.message().expect("claude defines message")),
            ("plan", claude.plan().expect("claude defines plan")),
        ] {
            assert!(
                template.contains(PROMPT_FILE_PLACEHOLDER),
                "{verb} is meant to be a prompted launch: {template}"
            );
            assert!(
                template.contains("--permission-mode auto"),
                "{verb} asks an agent to act, so it cannot stop for an approval \
                 nobody is there to give: {template}"
            );
        }
        let resume = claude.resume().expect("claude defines resume");
        assert!(!resume.contains(PROMPT_FILE_PLACEHOLDER), "{resume}");
        assert!(!resume.contains("--permission-mode"), "{resume}");
    }

    /// The placeholder is bound only where a send happens; on any other verb it
    /// would reach the shell as literal braces, so it is refused at load.
    #[test]
    fn new_session_is_refused_outside_the_message_verb() {
        for (verb, template) in [
            ("attach", "join {session} {new_session}"),
            ("resume", "reopen {session} {new_session}"),
            ("logs", "tail {session} {new_session}"),
            ("plan", "plan --session-id {new_session} {prompt_file}"),
        ] {
            let text = format!(
                "[agents.a]\ndispatch = \"run {{prompt_file}}\"\n{verb} = \"{template}\"\n"
            );
            let e = parse(&text).unwrap_err().to_string();
            assert!(e.contains("{new_session}"), "{verb}: {e}");
            assert!(e.contains(verb), "{verb}: {e}");
        }
        // and on the dispatch template itself, which starts a session rather
        // than joining one
        let e = parse("[agents.a]\ndispatch = \"run --session-id {new_session} {prompt_file}\"\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("dispatch carries {new_session}"), "{e}");
    }

    /// The built-in `claude` message verb resumes in place (DESIGN.md §8): the
    /// supervisor that refuses a headless resume has been released by the
    /// rest-stop before any send is made, so the send addresses the session's own
    /// reference and the conversation stays under the name Voro composed for it.
    #[test]
    fn the_builtin_claude_message_verb_resumes_in_place() {
        let message = builtin_agents()["claude"].message().unwrap();
        assert!(!message.contains("--fork-session"), "{message}");
        assert!(!message.contains(NEW_SESSION_PLACEHOLDER), "{message}");
        assert!(message.contains("-p --resume {session}"), "{message}");
        // The session it resumes is the one it was given, so nothing downstream
        // has a new reference to record.
        let rendered = render_message(message, "uuid-1", Path::new("/tmp/p.txt"));
        assert_eq!(rendered.new_session_ref, None);
        assert!(
            rendered.command.contains("--resume 'uuid-1'"),
            "{rendered:?}"
        );
    }

    #[test]
    fn render_session_binds_the_reference_shell_quoted() {
        assert_eq!(
            render_session("claude logs \"$(printf %.8s {session})\"", "3f6c-1111"),
            "claude logs \"$(printf %.8s '3f6c-1111')\""
        );
    }

    /// The built-in `claude` defines `logs` and `codex` does not, which is the
    /// per-verb degradation the whole verb set is built on: cap badging is a
    /// claude capability, and codex dispatches exactly as before without one.
    #[test]
    fn only_the_claude_builtin_defines_logs() {
        let config = AgentsConfig::load(Path::new("/nonexistent/voro.toml")).unwrap();
        let claude = config.agent("claude").expect("the built-in claude");
        assert!(claude.logs().expect("a logs verb").contains("claude logs"));
        assert!(config.agent("codex").expect("codex").logs().is_none());
    }

    /// `logs` joins the session verbs on both rules: it must name the session
    /// it reads, and it may not carry a launch placeholder that only `dispatch`
    /// and `plan` can resolve.
    #[test]
    fn logs_is_validated_as_a_session_verb() {
        for (logs, expected) in [
            ("agent-logs --tail", SESSION_PLACEHOLDER),
            ("agent-logs {session} --model {model}", MODEL_PLACEHOLDER),
            ("agent-logs {session} --task {task_id}", TASK_ID_PLACEHOLDER),
            (
                "agent-logs {session} --name {session_name}",
                SESSION_NAME_PLACEHOLDER,
            ),
        ] {
            let toml = format!(
                "[agents.a]\ndispatch = \"run {{prompt_file}}\"\nmodel = \"m\"\nlogs = \"{logs}\"\n"
            );
            let raw: RawConfig = toml::from_str(&toml).unwrap();
            let err = validate_agent("a", &raw.agents["a"], Path::new("/c.toml"))
                .expect_err("{logs} is refused");
            let message = err.to_string();
            assert!(message.contains("logs"), "{message}");
            assert!(message.contains(expected), "{message}");
        }
    }

    /// The built-in `stop` renders through the same session binder `logs` does,
    /// down to the truncation: `claude stop` keys on the eight-character job id,
    /// so the reference goes in shell-quoted inside the `printf` that trims it
    /// rather than whole.
    #[test]
    fn the_claude_stop_verb_renders_the_short_id() {
        let config = AgentsConfig::load(Path::new("/nonexistent/voro.toml")).unwrap();
        let stop = config
            .agent("claude")
            .expect("the built-in claude")
            .stop()
            .expect("a stop verb");
        assert_eq!(
            render_session(stop, "3f6c1111-2222-3333-4444-555555555555"),
            "claude stop \"$(printf %.8s '3f6c1111-2222-3333-4444-555555555555')\""
        );
    }

    /// The degradation the verb rides on: `codex` names no `stop`, so a close
    /// under it retires nothing and leaves exactly the behaviour Voro had.
    #[test]
    fn the_codex_builtin_defines_no_stop() {
        let config = AgentsConfig::load(Path::new("/nonexistent/voro.toml")).unwrap();
        assert!(config.agent("codex").expect("codex").stop().is_none());
    }

    /// `stop` joins the session verbs on both rules: it must name the session it
    /// retires, and it may not carry a launch placeholder only `dispatch` and
    /// `plan` can resolve.
    #[test]
    fn stop_is_validated_as_a_session_verb() {
        for (stop, expected) in [
            ("agent-stop --all", SESSION_PLACEHOLDER),
            ("agent-stop {session} --model {model}", MODEL_PLACEHOLDER),
            ("agent-stop {session} --task {task_id}", TASK_ID_PLACEHOLDER),
            (
                "agent-stop {session} --name {session_name}",
                SESSION_NAME_PLACEHOLDER,
            ),
            (
                "agent-stop {session} --into {new_session}",
                NEW_SESSION_PLACEHOLDER,
            ),
        ] {
            let toml = format!(
                "[agents.a]\ndispatch = \"run {{prompt_file}}\"\nmodel = \"m\"\nstop = \"{stop}\"\n"
            );
            let raw: RawConfig = toml::from_str(&toml).unwrap();
            let err = validate_agent("a", &raw.agents["a"], Path::new("/c.toml"))
                .expect_err("{stop} is refused");
            let message = err.to_string();
            assert!(message.contains("stop"), "{message}");
            assert!(message.contains(expected), "{message}");
        }
    }

    /// An override that drops `stop` is reported like any other dropped verb, so
    /// `agent list` can say the sessions it closes will now linger in the
    /// agent's own listing.
    #[test]
    fn an_override_dropping_stop_is_reported() {
        let config = AgentsConfig::parse(
            "[agents.claude]\ndispatch = \"claude {prompt_file}\"\n",
            Path::new("/c.toml"),
        )
        .unwrap();
        assert!(config.override_missing_verbs("claude").contains(&"stop"));
    }

    /// The one-pass rule (§8): a value carrying its own braces reaches the
    /// command line as written rather than being re-scanned.
    #[test]
    fn render_message_does_not_rescan_a_bound_value() {
        let rendered = render_message(
            "say {session} {prompt_file}",
            "{prompt_file}",
            Path::new("/run/m.md"),
        );
        assert_eq!(rendered.command, "say '{prompt_file}' '/run/m.md'");
    }

    /// A stale `continue` line — from a pre-pivot config or the old codex
    /// built-in — is now an unknown field, so the config is refused rather than
    /// silently honouring a verb Voro no longer runs (DESIGN.md §6/§8).
    #[test]
    fn a_continue_verb_is_now_an_unknown_field() {
        let text = r#"
            default_agent = "a"

            [agents.a]
            dispatch = "run {prompt_file}"
            continue = "reopen {session} {prompt_file}"
        "#;
        assert!(AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).is_err());
    }

    #[test]
    fn agent_looks_up_templates_by_name() {
        let config = AgentsConfig::parse(VERBS_CONFIG, Path::new("/tmp/voro.toml")).unwrap();
        let claude = config.agent("claude").unwrap();
        assert_eq!(claude.attach(), Some("claude attach {session}"));
        assert_eq!(claude.sessions(), Some("claude agents --json"));
        assert!(config.agent("gemini").is_none());
    }

    #[test]
    fn parse_sessions_json_reads_the_listing_shape() {
        let json = r#"[
            {"pid": 4321, "id": "deadbeef", "cwd": "/tmp/proj", "kind": "background",
             "startedAt": 1767950000000, "sessionId": "3f6c0e6e-1111-2222-3333-444455556666",
             "name": "t", "status": "idle", "state": "done"},
            {"id": "cafebabe", "cwd": "/tmp/other", "startedAt": 1767950001000},
            {"pid": 1}
        ]"#;
        let entries = parse_sessions_json(json).unwrap();
        assert_eq!(entries.len(), 2, "the id-less entry is skipped");
        assert_eq!(
            entries[0].session_ref,
            "3f6c0e6e-1111-2222-3333-444455556666"
        );
        assert_eq!(entries[0].short_id.as_deref(), Some("deadbeef"));
        assert_eq!(entries[0].cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(entries[0].started_at_ms, Some(1767950000000));
        assert_eq!(entries[0].pid, Some(4321));
        assert_eq!(entries[0].liveness(), SessionLiveness::Dead);
        assert!(entries[0].matches_ref("deadbeef"), "short id matches too");
        assert!(entries[0].matches_ref("3f6c0e6e-1111-2222-3333-444455556666"));

        assert_eq!(entries[1].session_ref, "cafebabe", "id is the fallback");
        assert_eq!(entries[1].pid, None, "the field is optional");
        assert_eq!(
            entries[1].liveness(),
            SessionLiveness::Dead,
            "an entry saying neither state nor pid claims nothing, so it is not live"
        );
    }

    /// The listing's own account of liveness (DESIGN.md §8). The case that
    /// forced it: an agent listing that never retires a finished session leaves
    /// it sitting at `blocked` indefinitely, so not-`done` cannot mean live —
    /// but a `blocked` entry whose supervisor pid is still there is a session
    /// genuinely stuck mid-turn, which must stay live and attachable.
    #[test]
    fn liveness_takes_done_first_then_pid_then_state() {
        let entry = |json: &str| {
            let listing = format!("[{{\"sessionId\": \"u\", {json}}}]");
            parse_sessions_json(&listing).unwrap().remove(0).liveness()
        };
        // `done` wins over a pid that is still around: the session is over
        // whatever process outlives it.
        assert_eq!(
            entry(r#""state": "done", "pid": 4321"#),
            SessionLiveness::Dead
        );
        assert_eq!(entry(r#""state": "done""#), SessionLiveness::Dead);
        // a pid decides every other state, including one Voro does not know
        for state in ["\"state\": \"blocked\", ", "\"state\": \"working\", ", ""] {
            assert_eq!(
                entry(&format!("{state}\"pid\": 4321")),
                SessionLiveness::WhileProcessLives(4321),
                "{state}"
            );
        }
        // without a pid, only `working` claims the session is going
        assert_eq!(entry(r#""state": "working""#), SessionLiveness::Live);
        assert_eq!(entry(r#""state": "blocked""#), SessionLiveness::Dead);
        assert_eq!(entry(r#""state": "idle""#), SessionLiveness::Dead);
    }

    /// Rest is a narrower reading than death (DESIGN.md §8): the rest-stop acts
    /// on a turn that has *ended*, and only `done` says so. `blocked` is the
    /// separation that matters — dead to the liveness question, yet a turn still
    /// under way (a permission prompt, a supervisor mid-turn) that a stop would
    /// cut off.
    #[test]
    fn at_rest_is_done_alone() {
        let entry = |json: &str| {
            let listing = format!("[{{\"sessionId\": \"u\", {json}}}]");
            parse_sessions_json(&listing).unwrap().remove(0)
        };
        assert!(entry(r#""state": "done""#).at_rest());
        // a supervisor that outlives the turn does not make it unfinished
        assert!(entry(r#""state": "done", "pid": 4321"#).at_rest());
        for json in [
            r#""state": "blocked""#,
            r#""state": "blocked", "pid": 4321"#,
            r#""state": "working""#,
            r#""state": "idle""#,
            r#""state": "something-new""#,
            r#""pid": 4321"#,
            r#""cwd": "/tmp""#,
        ] {
            assert!(!entry(json).at_rest(), "{json}");
        }
    }

    #[test]
    fn parse_sessions_json_rejects_non_arrays() {
        assert!(parse_sessions_json("{}").is_err());
        assert!(parse_sessions_json("not json").is_err());
        assert_eq!(parse_sessions_json("[]").unwrap(), vec![]);
    }

    /// Nothing installed, nothing configured: the failure asks for the one
    /// thing the operator can act on — register the viewer they already use —
    /// and only then says what was probed. It never tells them to install an
    /// editor, and never calls the config file invalid: it may not even exist
    /// (#405).
    #[test]
    fn viewer_resolution_errors_with_guidance_when_nothing_resolves() {
        let message = config()
            .viewer_cmd_with(None, &none_installed)
            .unwrap_err()
            .to_string();
        assert!(
            message.starts_with("no viewer set up — run `voro viewer add"),
            "{message}"
        );
        assert!(message.contains("'zed {path}'"), "{message}");
        // the probed built-ins are diagnosis, so they come after the action
        let (action, diagnosis) = message.split_once("; ").unwrap();
        assert!(diagnosis.contains("code/cursor/zed"), "{message}");
        assert!(!action.contains("install"), "{message}");
        assert!(!message.contains("invalid"), "{message}");
        assert!(config().viewer_names().is_empty());
        assert_eq!(config().default_viewer_name_with(&none_installed), None);
    }

    /// The whole point of the built-in layer (DESIGN.md §11a): a config that
    /// defines no viewer at all still opens a task, given an editor on PATH.
    #[test]
    fn a_built_in_viewer_resolves_with_no_viewer_configured() {
        let config = config();
        assert_eq!(
            config.viewer_cmd_with(None, &only("zed")).unwrap(),
            "zed {path}"
        );
        assert_eq!(
            config.default_viewer_name_with(&only("zed")).as_deref(),
            Some("zed")
        );
        // probe order decides between two installed built-ins
        assert_eq!(
            config
                .viewer_cmd_with(None, &|name| matches!(name, "cursor" | "zed"))
                .unwrap(),
            "cursor -n {path}"
        );
        // and a built-in is resolvable by name whether or not it is installed,
        // which is what makes `default_viewer = "code"` work with no tables
        assert_eq!(config.viewer_cmd(Some("code")).unwrap(), "code -n {path}");
    }

    /// User configuration always wins over the probe, in the documented order.
    #[test]
    fn user_viewers_outrank_the_probed_built_in() {
        let installed = |_: &str| true;

        let sole = parse("[viewers.mine]\ncmd = \"mine {path}\"").unwrap();
        assert_eq!(
            sole.viewer_cmd_with(None, &installed).unwrap(),
            "mine {path}"
        );
        assert_eq!(
            sole.default_viewer_name_with(&installed).as_deref(),
            Some("mine")
        );

        let anonymous = parse("[viewer]\ncmd = \"anon {path}\"").unwrap();
        assert_eq!(
            anonymous.viewer_cmd_with(None, &installed).unwrap(),
            "anon {path}"
        );
        // the anonymous table resolves but has no name to star
        assert_eq!(anonymous.default_viewer_name_with(&installed), None);

        let named = parse(
            "default_viewer = \"mine\"\n[viewers.mine]\ncmd = \"mine {path}\"\n\
             [viewers.other]\ncmd = \"other {path}\"",
        )
        .unwrap();
        assert_eq!(
            named.viewer_cmd_with(None, &installed).unwrap(),
            "mine {path}"
        );
    }

    /// A `[viewers.code]` table replaces the built-in wholesale, exactly as an
    /// `[agents.claude]` table does — same name, user command, and a provenance
    /// that says so.
    #[test]
    fn a_user_table_overrides_a_built_in_viewer_wholesale() {
        let config = parse("[viewers.code]\ncmd = \"code --wait {path}\"").unwrap();
        assert_eq!(
            config.viewer_cmd(Some("code")).unwrap(),
            "code --wait {path}"
        );
        // still found by the probe, still what the default resolves to
        assert_eq!(
            config.viewer_cmd_with(None, &only("code")).unwrap(),
            "code --wait {path}"
        );
        let entries = config.viewer_entries();
        let code = entries.iter().find(|(name, ..)| *name == "code").unwrap();
        assert_eq!(code.2, Provenance::UserOverride);
    }

    #[test]
    fn viewer_entries_layer_the_built_ins_under_the_user_tables() {
        let config = parse("[viewers.mine]\ncmd = \"mine {path}\"").unwrap();
        let entries: Vec<(&str, Provenance)> = config
            .viewer_entries()
            .into_iter()
            .map(|(name, _, prov)| (name, prov))
            .collect();
        assert_eq!(
            entries,
            vec![
                ("code", Provenance::BuiltIn),
                ("cursor", Provenance::BuiltIn),
                ("mine", Provenance::User),
                ("zed", Provenance::BuiltIn),
            ]
        );
        // viewer_names stays the editable set
        assert_eq!(config.viewer_names(), vec!["mine"]);
    }

    #[test]
    fn the_anonymous_viewer_table_is_the_default_viewer() {
        let text = r#"
            default_agent = "claude"

            [agents.claude]
            cmd = "claude -p {prompt_file}"

            [viewer]
            cmd = "zed {path}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.viewer_cmd(None).unwrap(), "zed {path}");
    }

    #[test]
    fn named_viewers_resolve_by_name_and_default_viewer_picks_among_them() {
        let text = r#"
            default_viewer = "zed"

            [viewers.zed]
            cmd = "zed {path}"

            [viewers.difftool]
            cmd = "git difftool -d"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.viewer_names(), vec!["difftool", "zed"]);
        assert_eq!(
            config.viewer_cmd(Some("difftool")).unwrap(),
            "git difftool -d"
        );
        assert_eq!(config.viewer_cmd(None).unwrap(), "zed {path}");
        assert_eq!(config.default_viewer_name().as_deref(), Some("zed"));
    }

    #[test]
    fn a_sole_named_viewer_is_the_default_without_being_named() {
        let text = r#"
            [viewers.zed]
            cmd = "zed {path}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.viewer_cmd(None).unwrap(), "zed {path}");
        assert_eq!(config.default_viewer_name().as_deref(), Some("zed"));
    }

    /// Two viewers and no `default_viewer` names none of them, so resolution
    /// carries on to the built-in probe rather than stopping — and says what to
    /// install when that finds nothing either.
    #[test]
    fn several_named_viewers_without_a_default_fall_through_to_the_built_ins() {
        let text = r#"
            [viewers.mine]
            cmd = "mine {path}"

            [viewers.difftool]
            cmd = "git difftool -d"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(
            config.viewer_cmd_with(None, &only("zed")).unwrap(),
            "zed {path}"
        );
        let message = config
            .viewer_cmd_with(None, &none_installed)
            .unwrap_err()
            .to_string();
        assert!(message.contains("no viewer set up"), "{message}");
    }

    #[test]
    fn an_unknown_viewer_name_errors_listing_the_known_ones() {
        let text = r#"
            [viewers.zed]
            cmd = "zed {path}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let message = config.viewer_cmd(Some("emacs")).unwrap_err().to_string();
        assert!(
            message.starts_with("no viewer named 'emacs' — run"),
            "{message}"
        );
        // the known set is every viewer that resolves, built-ins included
        assert!(message.contains("code, cursor, zed"), "{message}");
        assert!(!message.contains("invalid"), "{message}");
        // a default_viewer naming a missing table reports the same way
        let text = r#"default_viewer = "gone""#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let message = config.viewer_cmd(None).unwrap_err().to_string();
        assert!(message.contains("gone"), "{message}");
    }

    #[test]
    fn starter_config_defines_nothing_and_leaves_the_builtins() {
        let config = AgentsConfig::parse(&starter_config(), Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.agent_names(), vec!["claude", "codex"]);
        assert_eq!(config.provenance("claude"), Some(Provenance::BuiltIn));
        assert!(config.viewer_names().is_empty());
        assert!(config.viewer_cmd_with(None, &none_installed).is_err());
        let claude = config.agent("claude").unwrap();
        assert!(claude.dispatch().contains("--bg"), "{}", claude.dispatch());
        assert!(
            claude.dispatch().contains(SESSION_NAME_PLACEHOLDER),
            "{}",
            claude.dispatch()
        );
        assert!(claude.sessions().is_some());
        assert!(claude.attach().is_some());
        assert!(claude.resume().is_some());
    }

    #[test]
    fn starter_config_reproduces_the_builtins_commented_for_copying() {
        let skeleton = starter_config();
        for line in BUILTIN_AGENTS
            .lines()
            .chain(BUILTIN_VIEWERS.lines())
            .filter(|l| !l.is_empty())
        {
            let commented = format!("# {line}");
            assert!(
                skeleton.contains(&commented),
                "skeleton is missing built-in line: {commented}"
            );
        }
        // Uncommenting the reproduced claude block must yield a valid override.
        let uncommented: String = BUILTIN_AGENTS
            .lines()
            .take_while(|l| !l.starts_with("[agents.codex]"))
            .collect::<Vec<_>>()
            .join("\n");
        let config = AgentsConfig::parse(&uncommented, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(config.provenance("claude"), Some(Provenance::UserOverride));
        assert!(config.override_missing_verbs("claude").is_empty());
    }

    #[test]
    fn entries_carry_name_template_and_provenance() {
        // CONFIG overrides both built-ins wholesale, hence UserOverride below.
        let config = config();
        let entries: Vec<_> = config.entries().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "claude");
        assert_eq!(
            entries[0].1.dispatch(),
            "claude -p --output-format stream-json {prompt_file}"
        );
        assert_eq!(entries[0].2, Provenance::UserOverride);
        assert_eq!(entries[1].0, "codex");
        assert_eq!(entries[1].1.dispatch(), "codex exec {prompt_file}");
        assert_eq!(entries[1].2, Provenance::UserOverride);
    }

    #[test]
    fn write_starter_creates_parent_and_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!("voro-init-{}", std::process::id()));
        let path = dir.join("voro/voro.toml");
        let _ = std::fs::remove_dir_all(&dir);

        AgentsConfig::write_starter(&path).unwrap();
        let config = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.agent_names(), vec!["claude", "codex"]);

        let err = AgentsConfig::write_starter(&path).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn builtins_parse_and_validate() {
        let agents = builtin_agents();
        assert!(agents.contains_key("claude"));
        assert!(agents.contains_key("codex"));
        assert!(agents["claude"].sessions().is_some());
        assert!(agents["codex"].resume().is_some());
    }

    // --- launch identity and rendered commands (task #326) ---

    /// A dispatch of task 7 with a fixed prompt file, so a rendered command is
    /// a stable string to assert on.
    fn spec(deep: bool) -> LaunchSpec<'static> {
        LaunchSpec {
            launch: Launch::Dispatch {
                task_id: 7,
                title: "Widen the strip".into(),
            },
            prompt_file: Path::new("/tmp/p.md"),
            deep,
        }
    }

    #[test]
    fn a_launch_names_its_session_and_its_files() {
        let dispatch = Launch::Dispatch {
            task_id: 42,
            title: "Widen the strip".into(),
        };
        let refine = Launch::Refine { task_id: 42 };
        let plan = Launch::Plan {
            project: "mote".into(),
        };
        // The dispatch name is the published contract; anything else pointed at
        // the same task suffixes a kind rather than colliding with it. A
        // planning session names its project, so the bare number in a session
        // name is always a task id.
        assert_eq!(dispatch.session_name(), "voro-42-widen-the-strip");
        assert_eq!(refine.session_name(), "voro-42-refine");
        assert_eq!(plan.session_name(), "voro-plan-mote");
        assert_ne!(dispatch.session_name(), refine.session_name());
        // The file slugs are exactly what the three paths computed before the
        // identity was factored out, so no prompt or log filename moved.
        assert_eq!(dispatch.slug(), "task-42");
        assert_eq!(refine.slug(), "refine-42");
        assert_eq!(plan.slug(), "plan-mote");
        assert_eq!(dispatch.task_id(), Some(42));
        assert_eq!(refine.task_id(), Some(42));
        assert_eq!(plan.task_id(), None);
    }

    #[test]
    fn a_quick_propose_names_its_project_and_has_no_task() {
        // Like a planning session it is drafting a task rather than naming one,
        // so it carries no task id and names its project the way `N`'s session
        // does — leaving a bare number in a session name always a task id.
        let propose = Launch::Propose {
            project: "mote".into(),
        };
        assert_eq!(propose.session_name(), "voro-propose-mote");
        assert_eq!(propose.slug(), "propose-mote");
        assert_eq!(propose.task_id(), None);
        assert_ne!(
            propose.session_name(),
            Launch::Dispatch {
                task_id: 2,
                title: "Propose".into(),
            }
            .session_name()
        );
    }

    /// A dispatch says what it is working on, because the name is all the
    /// operator gets in the agents view, the `/resume` picker and the phone's
    /// session list.
    #[test]
    fn a_dispatch_names_its_session_for_the_task_title() {
        let named = |id: i64, title: &str| {
            Launch::Dispatch {
                task_id: id,
                title: title.into(),
            }
            .session_name()
        };
        assert_eq!(
            named(428, "Deliver quick messages"),
            "voro-428-deliver-quick-messages"
        );
        // Whole words only: the budget stops the name before the word that
        // would overrun it rather than cutting that word in half.
        assert_eq!(
            named(1, "Make the score decomposition legible"),
            "voro-1-make-the-score"
        );
        // At least one word, even where that word alone is over budget: a name
        // cut mid-word reads as a different task.
        assert_eq!(
            named(2, "Internationalisation everywhere"),
            "voro-2-internationalisation"
        );
        // Every name still starts `voro-<id>`, so prefix reading is untouched.
        for name in [named(7, "Anything at all"), named(7, "")] {
            assert!(name.starts_with("voro-7"), "{name}");
        }
    }

    #[test]
    fn a_dispatch_slug_survives_punctuation_and_unsanitizable_titles() {
        let named = |title: &str| {
            Launch::Dispatch {
                task_id: 9,
                title: title.into(),
            }
            .session_name()
        };
        // The name reaches a shell command line, so nothing outside
        // `[A-Za-z0-9._-]` may survive — and the dashes punctuation leaves
        // behind are collapsed rather than kept.
        assert_eq!(named("It's \"fine\"; rm -rf /"), "voro-9-it-s-fine-rm-rf");
        assert_eq!(named("Fix voro-core_v1.2"), "voro-9-fix-voro-core_v1.2");
        // Case is dropped, unlike a project name: a title is a sentence.
        assert_eq!(named("ODM handoff"), "voro-9-odm-handoff");
        // A title that sanitizes to nothing leaves the bare name rather than a
        // row of dashes.
        assert_eq!(named(""), "voro-9");
        assert_eq!(named("   "), "voro-9");
        assert_eq!(named("!!! ???"), "voro-9");
        assert_eq!(named("日本語"), "voro-9");
    }

    /// The one name a dispatch may not take: its own task's refine session.
    #[test]
    fn a_dispatch_cannot_slug_onto_a_kind_suffix() {
        let named = |title: &str| {
            Launch::Dispatch {
                task_id: 42,
                title: title.into(),
            }
            .session_name()
        };
        let refine = Launch::Refine { task_id: 42 }.session_name();
        // A second word takes the slug clear of the suffix on its own.
        assert_eq!(named("Refine the rewrite"), "voro-42-refine-the-rewrite");
        // Where the second word is over budget the guard takes it anyway,
        // since overrunning beats colliding.
        assert_eq!(
            named("Refine internationalisation"),
            "voro-42-refine-internationalisation"
        );
        assert_eq!(
            named("Refine"),
            "voro-42",
            "a one-word title has no next word to extend with"
        );
        for title in ["Refine the rewrite", "Refine", "refine!", "  refine  "] {
            assert_ne!(named(title), refine, "collided on {title}");
        }
    }

    #[test]
    fn a_task_less_launch_sanitizes_its_project_name() {
        // The session name is substituted into a shell command line and the
        // slug becomes a filename, so nothing outside `[A-Za-z0-9._-]` may
        // survive either. Case does, so a project named in capitals reads as
        // itself.
        let plan = |name: &str| Launch::Plan {
            project: name.to_string(),
        };
        assert_eq!(plan("odm 2").session_name(), "voro-plan-odm-2");
        assert_eq!(plan("odm 2").slug(), "plan-odm-2");
        assert_eq!(plan("ODM").session_name(), "voro-plan-ODM");
        assert_eq!(
            plan("it's \"fine\"; rm -rf /").session_name(),
            "voro-plan-it-s--fine---rm--rf--"
        );
        assert_eq!(plan("a/b").slug(), "plan-a-b");
        // The characters a name may keep pass through untouched.
        assert_eq!(plan("voro-core_v1.2").slug(), "plan-voro-core_v1.2");

        // The other task-less launch reduces a name the same way, so the two
        // sessions a project can have without a task read alike.
        let propose = |name: &str| Launch::Propose {
            project: name.to_string(),
        };
        assert_eq!(propose("odm 2").session_name(), "voro-propose-odm-2");
        assert_eq!(propose("odm 2").slug(), "propose-odm-2");
        assert_eq!(propose("ODM").session_name(), "voro-propose-ODM");
        assert_eq!(
            propose("it's \"fine\"; rm -rf /").session_name(),
            "voro-propose-it-s--fine---rm--rf--"
        );
        assert_eq!(propose("a/b").slug(), "propose-a-b");
        assert_eq!(propose("voro-core_v1.2").slug(), "propose-voro-core_v1.2");
    }

    #[test]
    fn builtin_claude_names_the_session_from_the_launch() {
        let config = AgentsConfig::builtin_only(Path::new("/tmp/voro.toml"));
        let claude = config.resolve(Some("claude")).unwrap();
        let dispatch = claude.launch_command(&spec(false));
        assert!(
            dispatch.contains("--name \"voro-7-widen-the-strip\""),
            "{dispatch}"
        );

        let refined = claude.launch_command(&LaunchSpec {
            launch: Launch::Refine { task_id: 7 },
            ..spec(false)
        });
        assert!(refined.contains("--name \"voro-7-refine\""), "{refined}");
        assert_ne!(dispatch, refined);

        // A planning session is named too, and `--name` is not a --bg-only
        // flag, so the foreground plan verb carries it.
        let planned = claude
            .plan_launch_command(&LaunchSpec {
                launch: Launch::Plan {
                    project: "mote".into(),
                },
                ..spec(false)
            })
            .unwrap();
        assert!(planned.contains("--name \"voro-plan-mote\""), "{planned}");
        assert!(!planned.contains("--bg"), "{planned}");

        // Nothing reaches the shell as literal braces on any of them.
        for rendered in [dispatch, refined, planned] {
            assert!(!rendered.contains('{'), "unsubstituted: {rendered}");
        }
    }

    #[test]
    fn the_prompt_file_is_shell_quoted_into_the_command() {
        let config = AgentsConfig::builtin_only(Path::new("/tmp/voro.toml"));
        let claude = config.resolve(Some("claude")).unwrap();
        let rendered = claude.launch_command(&LaunchSpec {
            prompt_file: Path::new("/tmp/a dir/p.md"),
            ..spec(false)
        });
        assert!(rendered.contains("cat '/tmp/a dir/p.md'"), "{rendered}");
    }

    #[test]
    fn builtin_claude_renders_a_model_per_purpose_and_depth() {
        let config = AgentsConfig::builtin_only(Path::new("/tmp/voro.toml"));
        let claude = config.resolve(Some("claude")).unwrap();
        // A workhorse for ordinary implementation, the stronger model for a
        // deep task and for interactive planning; all `claude` model aliases,
        // so none churns with a release.
        assert!(
            claude.launch_command(&spec(false)).contains("--model opus"),
            "{}",
            claude.launch_command(&spec(false))
        );
        assert!(
            claude.launch_command(&spec(true)).contains("--model fable"),
            "{}",
            claude.launch_command(&spec(true))
        );
        let planned = claude.plan_launch_command(&spec(false)).unwrap();
        assert!(planned.contains("--model fable"), "{planned}");
        for rendered in [
            claude.launch_command(&spec(false)),
            claude.launch_command(&spec(true)),
            planned,
        ] {
            assert!(
                !rendered.contains(MODEL_PLACEHOLDER),
                "placeholder left unresolved: {rendered}"
            );
        }
    }

    #[test]
    fn an_agent_without_the_placeholder_ignores_depth_entirely() {
        let config = AgentsConfig::builtin_only(Path::new("/tmp/voro.toml"));
        let codex = config.resolve(Some("codex")).unwrap();
        assert_eq!(
            codex.launch_command(&spec(true)),
            codex.launch_command(&spec(false))
        );
        assert_eq!(
            codex.launch_command(&spec(true)),
            "codex exec \"$(cat '/tmp/p.md')\""
        );
        assert_eq!(codex.plan_launch_command(&spec(false)), None);
    }

    #[test]
    fn model_deep_and_model_plan_fall_back_to_model() {
        let text = r#"
            [agents.a]
            dispatch = "run --model {model} {prompt_file}"
            plan     = "run -i --model {model} {prompt_file}"
            model    = "workhorse"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let a = config.resolve(Some("a")).unwrap();
        assert_eq!(
            a.launch_command(&spec(false)),
            "run --model workhorse '/tmp/p.md'"
        );
        assert_eq!(
            a.launch_command(&spec(true)),
            "run --model workhorse '/tmp/p.md'"
        );
        assert_eq!(
            a.plan_launch_command(&spec(false)).unwrap(),
            "run -i --model workhorse '/tmp/p.md'"
        );
    }

    #[test]
    fn the_placeholder_without_a_model_key_is_a_config_error() {
        let text = r#"
            [agents.a]
            dispatch = "run --model {model} {prompt_file}"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("{model}"), "{message}");
        assert!(message.contains("model = "), "{message}");
        assert!(message.contains("'a'"), "{message}");

        // ...and the same when only `plan` carries it.
        let text = r#"
            [agents.a]
            dispatch = "run {prompt_file}"
            plan     = "run -i --model {model} {prompt_file}"
        "#;
        assert!(AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).is_err());
    }

    /// Wholesale overrides written before the model map existed carry no
    /// `{model}`; the keys are inert there rather than newly required.
    #[test]
    fn model_keys_without_the_placeholder_are_inert_not_an_error() {
        let text = r#"
            [agents.claude]
            dispatch   = "claude -p {prompt_file}"
            model      = "opus"
            model_deep = "fable"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let claude = config.resolve(Some("claude")).unwrap();
        assert_eq!(claude.launch_command(&spec(true)), "claude -p '/tmp/p.md'");
        assert_eq!(claude.launch_command(&spec(false)), "claude -p '/tmp/p.md'");
    }

    #[test]
    fn a_session_verb_carrying_the_placeholder_is_rejected() {
        let text = r#"
            [agents.a]
            dispatch = "run {prompt_file}"
            attach   = "reopen --model {model} {session}"
            model    = "workhorse"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("attach"), "{message}");
        assert!(message.contains("{model}"), "{message}");
    }

    /// No launch placeholder may survive to a command line: one a renderer does
    /// not bind on that verb is refused at load rather than reaching the shell
    /// as literal braces (DESIGN.md §8).
    #[test]
    fn launch_placeholders_are_refused_on_the_verbs_that_cannot_bind_them() {
        for (verb, template) in [
            ("sessions", "list --name {session_name}"),
            ("attach", "reopen --name {session_name} {session}"),
            ("resume", "reopen {session} --for {task_id}"),
        ] {
            let text =
                format!("[agents.a]\ndispatch = \"run {{prompt_file}}\"\n{verb} = '{template}'\n");
            let message = AgentsConfig::parse(&text, Path::new("/tmp/voro.toml"))
                .unwrap_err()
                .to_string();
            assert!(message.contains(verb), "{verb}: {message}");
            assert!(message.contains("dispatch and plan"), "{verb}: {message}");
        }

        // `plan` serves a target that has no task, so `{task_id}` is refused
        // there even though `dispatch` still honours it.
        let text = r#"
            [agents.a]
            dispatch = "run {prompt_file}"
            plan     = "run -i --name \"voro-{task_id}\" {prompt_file}"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/voro.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("plan"), "{message}");
        assert!(message.contains(TASK_ID_PLACEHOLDER), "{message}");
        assert!(message.contains(SESSION_NAME_PLACEHOLDER), "{message}");
    }

    #[test]
    fn the_session_name_placeholder_is_accepted_on_dispatch_and_plan() {
        let text = r#"
            [agents.a]
            dispatch = "run --name {session_name} --for {task_id} {prompt_file}"
            plan     = "run -i --name {session_name} {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let a = config.resolve(Some("a")).unwrap();
        assert_eq!(
            a.launch_command(&spec(false)),
            "run --name voro-7-widen-the-strip --for 7 '/tmp/p.md'"
        );
        assert_eq!(
            a.plan_launch_command(&LaunchSpec {
                launch: Launch::Plan {
                    project: "mote".into(),
                },
                ..spec(false)
            })
            .unwrap(),
            "run -i --name voro-plan-mote '/tmp/p.md'"
        );
    }

    /// What a headless launch records on its session row (DESIGN.md §8, task
    /// #387): an agent with a `sessions` verb may hand the work to a supervisor,
    /// so its listing is the authority; one without has only the pid Voro
    /// spawned.
    #[test]
    fn a_sessions_verb_makes_a_launch_listing_authoritative() {
        let text = r#"
            [agents.supervised]
            dispatch = "run --bg {prompt_file}"
            sessions = "run sessions --json"

            [agents.plain]
            dispatch = "run {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        assert_eq!(
            config
                .resolve(Some("supervised"))
                .unwrap()
                .dispatch_liveness_source(),
            LivenessSource::Listing
        );
        assert_eq!(
            config
                .resolve(Some("plain"))
                .unwrap()
                .dispatch_liveness_source(),
            LivenessSource::Pid
        );
    }

    #[test]
    fn default_agent_key_sets_the_default() {
        let text = r#"
            default_agent = "codex"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let both = |_: &str| true;
        assert_eq!(config.resolve_with(None, &both).unwrap().name, "codex");
    }
}
