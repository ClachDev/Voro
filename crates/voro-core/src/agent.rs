//! Agent dispatch templates (DESIGN.md §5, §8): command templates, not state,
//! so they live outside the database. Owns the built-in `claude`/`codex`
//! definitions, layers the user's `~/.config/voro/voro.toml` on top, and
//! resolves which agent a task dispatches with.
//!
//! An agent is a set of verb templates; only `dispatch` is required (`cmd` is
//! an accepted alias). The optional `sessions`/`attach`/`resume` verbs unlock
//! session-aware dispatch and `plan` unlocks the TUI's interactive planning
//! sessions (DESIGN.md §8); each degrades gracefully when absent
//! (docs/agent-integration.md). Config is layered: built-ins under `voro.toml`,
//! which may add agents, override a built-in wholesale, and set `default_agent`
//! and viewers. A missing file is not an error.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::error::{Error, Result};

/// The prompt-file substitution in the `dispatch` template. The working
/// directory is handled by the spawner, not the template.
pub const PROMPT_FILE_PLACEHOLDER: &str = "{prompt_file}";

/// The task-id substitution in the `dispatch` template, the numeric id of the
/// task. Optional — a template that omits it dispatches unchanged — so agents
/// with a session-naming flag can tie the session back to its task.
pub const TASK_ID_PLACEHOLDER: &str = "{task_id}";

/// The session-reference substitution in the `attach` and `resume` templates:
/// the agent-opaque reference captured at dispatch (a Claude session UUID, a
/// Codex session id, a tmux session name).
pub const SESSION_PLACEHOLDER: &str = "{session}";

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
/// The claude verbs carry per-purpose `--model` defaults: `dispatch` a
/// workhorse implementation model, `plan` a stronger reasoning model. They
/// pass the `claude` model *aliases* (`opus`, `fable`), not pinned model ids,
/// so they resolve to the current model of that class and do not churn with
/// each release; an operator wanting other models overrides the agent
/// wholesale in `voro.toml`.
const BUILTIN_AGENTS: &str = "\
[agents.claude]
dispatch = \"claude --bg --name \\\"voro-{task_id}\\\" --permission-mode auto --model opus \\\"$(cat {prompt_file})\\\"\"
sessions = \"claude agents --json\"
attach   = \"claude attach {session}\"
resume   = \"claude --resume {session}\"
plan     = \"claude --permission-mode auto --model fable \\\"$(cat {prompt_file})\\\"\"

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

/// Header prose for the skeleton `agent init` writes. [`starter_config`]
/// appends the current built-ins (commented) and example stanzas after it.
const STARTER_HEADER: &str = r#"# Voro configuration (~/.config/voro/voro.toml).
#
# This file is OPTIONAL. Voro ships with built-in `claude` and `codex` agents,
# so a fresh install with one of those on PATH can dispatch with no config here.
# Run `voro agent list` to see the effective agents and where each comes from.
#
# Use this file to extend or override the built-ins, and to set app options:
#
#   * add your own agent — a new [agents.<name>] table. Only `dispatch` is
#     required (`cmd` is an alias): it starts a session on a task, with
#     `{prompt_file}` replaced by the prompt file's path and the optional
#     `{task_id}` by the task's numeric id. The optional session verbs unlock
#     attachable dispatch, and each degrades gracefully when absent:
#       sessions  list the agent's sessions as JSON (liveness + ref capture)
#       attach    open a running session interactively    ({session})
#       resume    reopen a finished session interactively  ({session})
#       plan      run an interactive foreground planning session ({prompt_file})
#     See docs/agent-integration.md for the full contract.
#   * override a built-in — a table named `claude` or `codex` REPLACES that
#     built-in entirely (not per-verb), so copy every verb you still want. The
#     built-ins are reproduced below, commented out, ready to copy.
#   * set `default_agent` — used for tasks with no --agent override. When unset,
#     Voro picks the first built-in found on PATH (claude, then codex).
#   * set up viewers — [viewers.<name>] tables define how a task's diff is
#     shown locally when `voro pr`/`voro open` resolve to the viewer medium
#     (DESIGN.md §8). A viewer cmd may carry `{path}` (the task's worktree, or
#     the project checkout when it has none), `{branch}` (the task's branch, or
#     empty), and `{base}` (the checkout's default branch); `{base}...{branch}`
#     spells a diff range. `default_viewer` names the one used when a project
#     does not pick a viewer itself (`voro project action <p> viewer:<name>`); a
#     single anonymous [viewer] table is the older, still-valid spelling of
#     the default.
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
         # Built-in agents, exactly as shipped. Uncomment a block and edit it to\n\
         # override that agent wholesale; leave it commented to keep the built-in,\n\
         # which updates with Voro. Copy a block to model a new agent of your own.\n\
         # --------------------------------------------------------------------------\n#\n",
    );
    for line in BUILTIN_AGENTS.lines() {
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
         # [viewers.zed]\n\
         # cmd = \"zed {path}\"\n#\n\
         # [viewers.difftool]\n\
         # cmd = \"git -C {path} difftool -d {base}...{branch}\"\n",
    );
    out
}

/// A named set of verb templates from `voro.toml`. `dispatch` (or its alias
/// `cmd`) is required and always contains [`PROMPT_FILE_PLACEHOLDER`]; it may
/// also carry the optional [`TASK_ID_PLACEHOLDER`]. The rest are optional, with
/// their `{session}`/`{prompt_file}` placeholders validated at parse time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplate {
    dispatch: Option<String>,
    /// Pre-verb alias for `dispatch`, so existing configs load unchanged.
    cmd: Option<String>,
    sessions: Option<String>,
    attach: Option<String>,
    resume: Option<String>,
    /// An interactive foreground command carrying [`PROMPT_FILE_PLACEHOLDER`],
    /// run by the TUI's planning flow (DESIGN.md §8) — no `{session}`, since a
    /// planning session belongs to no task or session row.
    plan: Option<String>,
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

    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
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
    for (verb, template) in [("attach", &agent.attach), ("resume", &agent.resume)] {
        if let Some(template) = template
            && !template.contains(SESSION_PLACEHOLDER)
        {
            return Err(invalid(format!(
                "agent '{name}' {verb} is missing the {SESSION_PLACEHOLDER} placeholder"
            )));
        }
    }
    if let Some(template) = &agent.plan
        && !template.contains(PROMPT_FILE_PLACEHOLDER)
    {
        return Err(invalid(format!(
            "agent '{name}' plan is missing the {PROMPT_FILE_PLACEHOLDER} placeholder"
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

/// The agent a task will be dispatched with: the task's own override if it
/// has one, otherwise the config's global default, with every verb template
/// resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub name: String,
    pub dispatch: String,
    pub sessions: Option<String>,
    pub attach: Option<String>,
    pub resume: Option<String>,
    pub plan: Option<String>,
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
    /// The named `[viewers.<name>]` tables a project's review action can
    /// pick from (DESIGN.md §8/§11a).
    viewers: BTreeMap<String, ViewerTemplate>,
    /// The user-set `default_viewer`, naming a `[viewers.*]` entry.
    default_viewer: Option<String>,
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
        Ok(AgentsConfig {
            default: raw.default_agent,
            agents,
            provenance,
            viewer: raw.viewer,
            viewers: raw.viewers,
            default_viewer: raw.default_viewer,
            path: path.to_path_buf(),
        })
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
            plan: agent.plan.clone(),
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

    /// The names of the `[viewers.*]` tables, sorted, for the TUI's
    /// review-action picker and `viewer list`.
    pub fn viewer_names(&self) -> Vec<String> {
        self.viewers.keys().cloned().collect()
    }

    /// A named viewer's command, without the default-resolution [`viewer_cmd`]
    /// applies — for the Config screen, which lists each viewer beside its own
    /// template.
    ///
    /// [`viewer_cmd`]: Self::viewer_cmd
    pub fn named_viewer_cmd(&self, name: &str) -> Option<&str> {
        self.viewers.get(name).map(|v| v.cmd.as_str())
    }

    /// The anonymous `[viewer]` table's command, if the file defines one — the
    /// legacy default the Config screen surfaces read-only, since it carries no
    /// name to edit or delete by.
    pub fn anonymous_viewer_cmd(&self) -> Option<&str> {
        self.viewer.as_ref().map(|v| v.cmd.as_str())
    }

    /// The name of the viewer used when nothing picks one by name, for
    /// `viewer list` to flag: the user's `default_viewer` when set (honoured
    /// even if it names a missing viewer), else the sole `[viewers.*]` entry.
    /// The anonymous `[viewer]` table has no name, so it yields `None` here
    /// even though it resolves.
    pub fn default_viewer_name(&self) -> Option<String> {
        if self.default_viewer.is_some() {
            return self.default_viewer.clone();
        }
        if self.viewer.is_none() && self.viewers.len() == 1 {
            return self.viewers.keys().next().cloned();
        }
        None
    }

    /// Resolve a viewer command (DESIGN.md §11a): the named `[viewers.<name>]`
    /// when a name is given, otherwise the default — `default_viewer` when set,
    /// else the anonymous `[viewer]` table, else the sole `[viewers.*]` entry.
    /// Errors carry what to configure.
    pub fn viewer_cmd(&self, name: Option<&str>) -> Result<&str> {
        let invalid = |message: String| Error::AgentConfigInvalid {
            path: self.path.clone(),
            message,
        };
        let named =
            |name: &str| {
                self.viewers.get(name).map(|v| v.cmd.as_str()).ok_or_else(|| {
                let known = if self.viewers.is_empty() {
                    "none are defined".to_string()
                } else {
                    format!("defined viewers: {}", self.viewer_names().join(", "))
                };
                invalid(format!(
                    "no viewer named '{name}' — {known}; add a [viewers.{name}] table with a \
                     cmd such as 'zed {{path}}' or 'git difftool -d'"
                ))
            })
            };
        match name {
            Some(name) => named(name),
            None => match &self.default_viewer {
                Some(default) => named(default),
                None => self
                    .viewer
                    .as_ref()
                    .map(|v| v.cmd.as_str())
                    .or_else(|| match self.viewers.len() {
                        1 => self.viewers.values().next().map(|v| v.cmd.as_str()),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        invalid(
                            "no viewer configured; add a [viewers.<name>] table with a cmd \
                             such as 'zed {path}' or 'git difftool -d' to see a task's diff \
                             (set `default_viewer` when defining several)"
                                .to_string(),
                        )
                    }),
            },
        }
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
        [
            (
                "sessions",
                builtin.sessions.is_some(),
                user.sessions.is_some(),
            ),
            ("attach", builtin.attach.is_some(), user.attach.is_some()),
            ("resume", builtin.resume.is_some(), user.resume.is_some()),
            ("plan", builtin.plan.is_some(), user.plan.is_some()),
        ]
        .into_iter()
        .filter_map(|(verb, in_builtin, in_user)| (in_builtin && !in_user).then_some(verb))
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
/// dispatch's session among its siblings; `state` is `"done"` once finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSessionEntry {
    pub session_ref: String,
    pub short_id: Option<String>,
    pub cwd: Option<String>,
    pub started_at_ms: Option<i64>,
    pub state: Option<String>,
}

impl AgentSessionEntry {
    /// Whether this entry is the session a stored reference points at — either
    /// id form matches, since a log-parsed fallback may record the short id.
    pub fn matches_ref(&self, session_ref: &str) -> bool {
        self.session_ref == session_ref || self.short_id.as_deref() == Some(session_ref)
    }

    /// A finished session: still listed, but no longer running.
    pub fn is_finished(&self) -> bool {
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
        assert!(entries[0].is_finished());
        assert!(entries[0].matches_ref("deadbeef"), "short id matches too");
        assert!(entries[0].matches_ref("3f6c0e6e-1111-2222-3333-444455556666"));

        assert_eq!(entries[1].session_ref, "cafebabe", "id is the fallback");
        assert!(!entries[1].is_finished(), "no state means not finished");
    }

    #[test]
    fn parse_sessions_json_rejects_non_arrays() {
        assert!(parse_sessions_json("{}").is_err());
        assert!(parse_sessions_json("not json").is_err());
        assert_eq!(parse_sessions_json("[]").unwrap(), vec![]);
    }

    #[test]
    fn viewer_resolution_errors_with_guidance_when_nothing_is_configured() {
        let message = config().viewer_cmd(None).unwrap_err().to_string();
        assert!(message.contains("no viewer configured"), "{message}");
        assert!(message.contains("[viewers.<name>]"), "{message}");
        assert!(message.contains("/tmp/voro.toml"), "{message}");
        assert!(config().viewer_names().is_empty());
        assert_eq!(config().default_viewer_name(), None);
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

    #[test]
    fn several_named_viewers_without_a_default_error_with_guidance() {
        let text = r#"
            [viewers.zed]
            cmd = "zed {path}"

            [viewers.difftool]
            cmd = "git difftool -d"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let message = config.viewer_cmd(None).unwrap_err().to_string();
        assert!(message.contains("default_viewer"), "{message}");
    }

    #[test]
    fn an_unknown_viewer_name_errors_listing_the_known_ones() {
        let text = r#"
            [viewers.zed]
            cmd = "zed {path}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/voro.toml")).unwrap();
        let message = config.viewer_cmd(Some("emacs")).unwrap_err().to_string();
        assert!(message.contains("emacs"), "{message}");
        assert!(message.contains("zed"), "{message}");
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
        assert!(config.viewer_cmd(None).is_err());
        let claude = config.agent("claude").unwrap();
        assert!(claude.dispatch().contains("--bg"), "{}", claude.dispatch());
        assert!(
            claude.dispatch().contains("voro-{task_id}"),
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
        for line in BUILTIN_AGENTS.lines().filter(|l| !l.is_empty()) {
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

    #[test]
    fn builtin_claude_carries_per_purpose_model_defaults() {
        let claude = &builtin_agents()["claude"];
        // dispatch runs a workhorse implementation model, plan a stronger one;
        // both via `claude` model aliases, so neither churns with a release.
        assert!(
            claude.dispatch().contains("--model opus"),
            "{}",
            claude.dispatch()
        );
        let plan = claude.plan().unwrap();
        assert!(plan.contains("--model fable"), "{plan}");
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
