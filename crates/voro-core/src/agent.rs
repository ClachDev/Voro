//! Agent dispatch templates, per DESIGN.md §5 and §8. Agents are command
//! templates, not state, so they live in `~/.config/voro/agents.toml` rather
//! than the database. This module only loads the file and resolves which agent
//! a task should be dispatched with — spawning is the dispatcher's job.
//!
//! An agent is a set of verb templates. Only `dispatch` is required (`cmd` is
//! accepted as an alias, so pre-verb configs load unchanged); the optional
//! verbs — `sessions`, `attach`, `resume`, `continue` — unlock session-aware
//! dispatch for agents that have a session layer of their own, and every one
//! of them degrades gracefully when absent (docs/agent-integration.md).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// The prompt-file substitution in `dispatch` and `continue` templates. The
/// working directory is handled by the spawner, not the template.
pub const PROMPT_FILE_PLACEHOLDER: &str = "{prompt_file}";

/// The session-reference substitution in `attach`, `resume`, and `continue`
/// templates: the agent-opaque reference captured at dispatch (a Claude
/// session UUID, a Codex session id, a tmux session name).
pub const SESSION_PLACEHOLDER: &str = "{session}";

/// The substitution in the `[viewer]` command template (DESIGN.md §11a): the
/// checkout path of the task's project. Optional — a viewer that operates on
/// the current directory (`git difftool -d`) needs no placeholder, since the
/// command is run in the project's path regardless.
pub const VIEWER_PATH_PLACEHOLDER: &str = "{path}";

/// A working starter config, written by `voro agents init` so a fresh install
/// can dispatch without hand-authoring TOML. The default agent is Claude Code
/// launched attachably (`--bg`), with its session verbs wired; the commented
/// second agent shows the shape for adding others. This must parse and pass
/// [`AgentsConfig::parse`]'s validation — it is exercised by a test.
pub const STARTER_CONFIG: &str = "\
# Voro agent command templates (~/.config/voro/agents.toml).
#
# Each [agents.<name>] table describes how Voro drives a coding agent, as a
# set of shell commands run via `sh -c` in a task's project checkout. Only
# `dispatch` is required (`cmd` is accepted as an alias): it starts a session
# on a task, with `{prompt_file}` replaced by the path to a file holding the
# task's title and body as the prompt. `default` names the agent used for
# tasks without an --agent override.
#
# The other verbs are optional, and each degrades gracefully when absent:
#
#   sessions  print the agent's sessions as a JSON array of objects carrying
#             `sessionId` (or `id`), `cwd`, `startedAt` (ms epoch), `state`
#             ('done' when finished). Used to capture a session reference at
#             dispatch and to check session liveness instead of pid-checking.
#   attach    open the *running* session interactively; `{session}` is
#             replaced with the captured reference.
#   resume    reopen a *finished* session interactively; `{session}` likewise.
#   continue  continue a session headless with new input; takes `{session}`
#             and `{prompt_file}` (the new input, e.g. an answer).
#
# A dispatched session runs unattended, so most agents need a non-interactive
# permission flag. With `attach` configured you can jump into a live session
# from the TUI (the a key) and answer permission prompts yourself. See
# docs/agent-integration.md for the codex equivalents and a tmux recipe for
# agents with no session layer of their own.

default = \"claude\"

[agents.claude]
dispatch = \"claude --bg --permission-mode acceptEdits \\\"$(cat {prompt_file})\\\"\"
sessions = \"claude agents --json\"
attach   = \"claude attach {session}\"
resume   = \"claude --resume {session}\"

# [agents.codex]
# dispatch = \"codex exec \\\"$(cat {prompt_file})\\\"\"
# resume   = \"codex resume {session}\"
# continue = \"codex exec resume {session} \\\"$(cat {prompt_file})\\\"\"

# An optional [viewer] command opens a review (or running) task's checkout so
# you can see its diff — `voro open <task-id>`, or the open key in the TUI.
# `{path}` is replaced with the project's checkout path, and the command runs
# in that directory. Uncomment and tune for the tool you use.
#
# [viewer]
# cmd = \"zed {path}\"
";

/// A named set of verb templates from `agents.toml`. `dispatch` (or its alias
/// `cmd`) is required and always contains [`PROMPT_FILE_PLACEHOLDER`]; the
/// rest are optional, with their `{session}`/`{prompt_file}` placeholders
/// validated at parse time so a bad template fails at load, not at use.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplate {
    dispatch: Option<String>,
    /// Pre-verb alias for `dispatch`, so existing configs load unchanged.
    cmd: Option<String>,
    sessions: Option<String>,
    attach: Option<String>,
    resume: Option<String>,
    #[serde(rename = "continue")]
    continue_: Option<String>,
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

    pub fn continue_cmd(&self) -> Option<&str> {
        self.continue_.as_deref()
    }
}

/// The `[viewer]` command template from `agents.toml` (DESIGN.md §11a): a
/// shell command run in a task's checkout to open its diff. Unlike an agent
/// template, `{path}` is optional, so nothing is validated at parse time.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewerTemplate {
    pub cmd: String,
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
    pub continue_cmd: Option<String>,
}

/// The parsed `agents.toml`: a global default plus named templates.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    default: String,
    agents: BTreeMap<String, AgentTemplate>,
    #[serde(default)]
    viewer: Option<ViewerTemplate>,
    #[serde(skip)]
    path: PathBuf,
}

impl AgentsConfig {
    /// `$XDG_CONFIG_HOME/voro/agents.toml`, defaulting to `~/.config`.
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
        config_home.join("voro/agents.toml")
    }

    pub fn load(path: &Path) -> Result<AgentsConfig> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::AgentConfigMissing(path.to_path_buf())
            } else {
                Error::AgentConfigInvalid {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                }
            }
        })?;
        AgentsConfig::parse(&text, path)
    }

    fn parse(text: &str, path: &Path) -> Result<AgentsConfig> {
        let invalid = |message: String| Error::AgentConfigInvalid {
            path: path.to_path_buf(),
            message,
        };
        let mut config: AgentsConfig =
            toml::from_str(text).map_err(|e| invalid(e.message().to_string()))?;
        config.path = path.to_path_buf();
        for (name, agent) in &config.agents {
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
                ("continue", &agent.continue_),
            ] {
                if let Some(template) = template
                    && !template.contains(SESSION_PLACEHOLDER)
                {
                    return Err(invalid(format!(
                        "agent '{name}' {verb} is missing the {SESSION_PLACEHOLDER} placeholder"
                    )));
                }
            }
            if let Some(template) = &agent.continue_
                && !template.contains(PROMPT_FILE_PLACEHOLDER)
            {
                return Err(invalid(format!(
                    "agent '{name}' continue is missing the {PROMPT_FILE_PLACEHOLDER} placeholder"
                )));
            }
        }
        Ok(config)
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
    /// global default. An override or default naming an agent absent from
    /// the config is an error here, not a panic at spawn time.
    pub fn resolve(&self, task_override: Option<&str>) -> Result<ResolvedAgent> {
        let (name, origin) = match task_override {
            Some(name) => (name, "task agent override"),
            None => (self.default.as_str(), "config default"),
        };
        let agent = self.agents.get(name).ok_or_else(|| Error::UnknownAgent {
            name: name.to_string(),
            origin,
            path: self.path.clone(),
            known: self.agents.keys().cloned().collect::<Vec<_>>().join(", "),
        })?;
        Ok(ResolvedAgent {
            name: name.to_string(),
            dispatch: agent.dispatch().to_string(),
            sessions: agent.sessions.clone(),
            attach: agent.attach.clone(),
            resume: agent.resume.clone(),
            continue_cmd: agent.continue_.clone(),
        })
    }

    /// The `[viewer]` command template, if one is configured (DESIGN.md §11a).
    /// `None` when the config has no `[viewer]` table, which the open-in-viewer
    /// action turns into "add a `[viewer]` entry" rather than a silent no-op.
    pub fn viewer(&self) -> Option<&str> {
        self.viewer.as_ref().map(|v| v.cmd.as_str())
    }

    /// The name of the agent used when a task has no override, for the CLI's
    /// `agents list` to flag it.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// Every agent as `(name, template)`, sorted by name, for `agents list`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &AgentTemplate)> {
        self.agents
            .iter()
            .map(|(name, agent)| (name.as_str(), agent))
    }

    /// Write [`STARTER_CONFIG`] to `path`, creating parent directories. Refuses
    /// to overwrite an existing file so a hand-tuned config is never clobbered
    /// — `agents init` is a one-time bootstrap, not a reset.
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
        std::fs::write(path, STARTER_CONFIG).map_err(|e| Error::AgentConfigInvalid {
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
    /// Whether this entry is the session a stored reference points at —
    /// either form of the id matches, since a log-parsed fallback capture may
    /// have recorded the short id where the listing carries the full one.
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
        default = "claude"

        [agents.claude]
        cmd = "claude -p --output-format stream-json {prompt_file}"

        [agents.codex]
        cmd = "codex exec {prompt_file}"
    "#;

    fn config() -> AgentsConfig {
        AgentsConfig::parse(CONFIG, Path::new("/tmp/agents.toml")).unwrap()
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
            default = "gemini"

            [agents.claude]
            cmd = "claude -p {prompt_file}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/agents.toml")).unwrap();
        let message = config.resolve(None).unwrap_err().to_string();
        assert!(message.contains("gemini"), "{message}");
        assert!(message.contains("config default"), "{message}");
    }

    #[test]
    fn cmd_without_prompt_file_placeholder_is_rejected() {
        let text = r#"
            default = "claude"

            [agents.claude]
            cmd = "claude -p"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/agents.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("{prompt_file}"), "{message}");
        assert!(message.contains("claude"), "{message}");
    }

    #[test]
    fn invalid_toml_names_the_file() {
        let message = AgentsConfig::parse("default = ", Path::new("/tmp/agents.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("/tmp/agents.toml"), "{message}");
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
    fn missing_file_names_the_expected_path() {
        let message = AgentsConfig::load(Path::new("/nonexistent/agents.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("/nonexistent/agents.toml"), "{message}");
    }

    // --- session verbs (task #75) ---

    const VERBS_CONFIG: &str = r#"
        default = "claude"

        [agents.claude]
        dispatch = "claude --bg \"$(cat {prompt_file})\""
        sessions = "claude agents --json"
        attach   = "claude attach {session}"
        resume   = "claude --resume {session}"

        [agents.codex]
        dispatch = "codex exec {prompt_file}"
        resume   = "codex resume {session}"
        continue = "codex exec resume {session} \"$(cat {prompt_file})\""
    "#;

    #[test]
    fn verbs_parse_and_resolve() {
        let config = AgentsConfig::parse(VERBS_CONFIG, Path::new("/tmp/agents.toml")).unwrap();
        let claude = config.resolve(None).unwrap();
        assert_eq!(claude.sessions.as_deref(), Some("claude agents --json"));
        assert_eq!(claude.attach.as_deref(), Some("claude attach {session}"));
        assert_eq!(claude.resume.as_deref(), Some("claude --resume {session}"));
        assert_eq!(claude.continue_cmd, None);

        let codex = config.resolve(Some("codex")).unwrap();
        assert_eq!(codex.sessions, None);
        assert_eq!(codex.attach, None);
        assert_eq!(
            codex.continue_cmd.as_deref(),
            Some("codex exec resume {session} \"$(cat {prompt_file})\"")
        );
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
        assert_eq!(resolved.continue_cmd, None);
    }

    #[test]
    fn both_dispatch_and_cmd_is_rejected() {
        let text = r#"
            default = "claude"

            [agents.claude]
            cmd = "claude -p {prompt_file}"
            dispatch = "claude --bg {prompt_file}"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/agents.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("alias"), "{message}");
    }

    #[test]
    fn agent_without_dispatch_or_cmd_is_rejected() {
        let text = r#"
            default = "claude"

            [agents.claude]
            sessions = "claude agents --json"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/agents.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("dispatch"), "{message}");
    }

    #[test]
    fn attach_resume_continue_require_the_session_placeholder() {
        for verb in ["attach", "resume", "continue"] {
            let text = format!(
                "default = \"a\"\n\n[agents.a]\ndispatch = \"run {{prompt_file}}\"\n\
                 {verb} = \"reopen {{prompt_file}}\"\n"
            );
            let message = AgentsConfig::parse(&text, Path::new("/tmp/agents.toml"))
                .unwrap_err()
                .to_string();
            assert!(message.contains("{session}"), "{verb}: {message}");
            assert!(message.contains(verb), "{verb}: {message}");
        }
    }

    #[test]
    fn continue_requires_the_prompt_file_placeholder() {
        let text = r#"
            default = "a"

            [agents.a]
            dispatch = "run {prompt_file}"
            continue = "reopen {session}"
        "#;
        let message = AgentsConfig::parse(text, Path::new("/tmp/agents.toml"))
            .unwrap_err()
            .to_string();
        assert!(message.contains("{prompt_file}"), "{message}");
        assert!(message.contains("continue"), "{message}");
    }

    #[test]
    fn agent_looks_up_templates_by_name() {
        let config = AgentsConfig::parse(VERBS_CONFIG, Path::new("/tmp/agents.toml")).unwrap();
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
    fn viewer_is_none_without_a_viewer_table() {
        assert_eq!(config().viewer(), None);
    }

    #[test]
    fn viewer_is_read_from_the_viewer_table() {
        let text = r#"
            default = "claude"

            [agents.claude]
            cmd = "claude -p {prompt_file}"

            [viewer]
            cmd = "zed {path}"
        "#;
        let config = AgentsConfig::parse(text, Path::new("/tmp/agents.toml")).unwrap();
        assert_eq!(config.viewer(), Some("zed {path}"));
    }

    #[test]
    fn starter_config_has_no_active_viewer() {
        let config = AgentsConfig::parse(STARTER_CONFIG, Path::new("/tmp/agents.toml")).unwrap();
        assert_eq!(config.viewer(), None);
    }

    #[test]
    fn starter_config_parses_and_resolves() {
        let config = AgentsConfig::parse(STARTER_CONFIG, Path::new("/tmp/agents.toml")).unwrap();
        assert_eq!(config.default_name(), "claude");
        assert_eq!(config.agent_names(), vec!["claude"]);
        let claude = config.resolve(None).unwrap();
        assert_eq!(claude.name, "claude");
        assert!(claude.dispatch.contains("--bg"), "{}", claude.dispatch);
        assert!(claude.sessions.is_some());
        assert!(claude.attach.is_some());
        assert!(claude.resume.is_some());
    }

    #[test]
    fn entries_lists_name_and_template() {
        let config = config();
        let entries: Vec<_> = config.entries().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "claude");
        assert_eq!(
            entries[0].1.dispatch(),
            "claude -p --output-format stream-json {prompt_file}"
        );
        assert_eq!(entries[1].0, "codex");
        assert_eq!(entries[1].1.dispatch(), "codex exec {prompt_file}");
    }

    #[test]
    fn write_starter_creates_parent_and_refuses_to_clobber() {
        let dir = std::env::temp_dir().join(format!("voro-init-{}", std::process::id()));
        let path = dir.join("voro/agents.toml");
        let _ = std::fs::remove_dir_all(&dir);

        AgentsConfig::write_starter(&path).unwrap();
        // the written file loads back into a usable config
        assert_eq!(AgentsConfig::load(&path).unwrap().default_name(), "claude");

        // a second init refuses rather than overwriting a tuned config
        let err = AgentsConfig::write_starter(&path).unwrap_err().to_string();
        assert!(err.contains("already exists"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
