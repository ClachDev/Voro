//! Agent dispatch templates, per DESIGN.md §5 and §8. Agents are command
//! templates, not state, so they live in `~/.config/voro/agents.toml` rather
//! than the database. This module only loads the file and resolves which agent
//! a task should be dispatched with — spawning is the dispatcher's job.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// The only substitution in a v1 command template. The working directory is
/// handled by the spawner, not the template.
pub const PROMPT_FILE_PLACEHOLDER: &str = "{prompt_file}";

/// A working starter config, written by `voro agents init` so a fresh install
/// can dispatch without hand-authoring TOML. The default agent is a headless
/// Claude Code invocation reading the task prompt from `{prompt_file}`; the
/// commented second agent shows the shape for adding others. This must parse
/// and pass [`AgentsConfig::parse`]'s validation — it is exercised by a test.
pub const STARTER_CONFIG: &str = "\
# Voro agent command templates (~/.config/voro/agents.toml).
#
# Each [agents.<name>] table is a shell command Voro runs (via `sh -c`) in a
# task's project checkout to dispatch it to a coding agent. `{prompt_file}` is
# replaced with the path to a file holding the task's title and body as the
# prompt. `default` names the agent used for tasks without an --agent override.
#
# Tune the commands below for the agents you have installed. A dispatched
# session runs unattended, so most agents need a non-interactive permission
# flag (for Claude Code, e.g. --permission-mode acceptEdits or, to run fully
# hands-off, --dangerously-skip-permissions).

default = \"claude\"

[agents.claude]
cmd = \"claude -p --permission-mode acceptEdits \\\"$(cat {prompt_file})\\\"\"

# [agents.codex]
# cmd = \"codex exec \\\"$(cat {prompt_file})\\\"\"
";

/// A named command template from `agents.toml`. `cmd` always contains
/// [`PROMPT_FILE_PLACEHOLDER`]; parsing rejects templates without it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTemplate {
    pub cmd: String,
}

/// The agent a task will be dispatched with: the task's own override if it
/// has one, otherwise the config's global default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAgent {
    pub name: String,
    pub cmd: String,
}

/// The parsed `agents.toml`: a global default plus named templates.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    default: String,
    agents: BTreeMap<String, AgentTemplate>,
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
            if !agent.cmd.contains(PROMPT_FILE_PLACEHOLDER) {
                return Err(invalid(format!(
                    "agent '{name}' cmd is missing the {PROMPT_FILE_PLACEHOLDER} placeholder"
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
            cmd: agent.cmd.clone(),
        })
    }

    /// The name of the agent used when a task has no override, for the CLI's
    /// `agents list` to flag it.
    pub fn default_name(&self) -> &str {
        &self.default
    }

    /// Every agent as `(name, cmd)`, sorted by name, for `agents list`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.agents
            .iter()
            .map(|(name, agent)| (name.as_str(), agent.cmd.as_str()))
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
            resolved.cmd,
            "claude -p --output-format stream-json {prompt_file}"
        );
    }

    #[test]
    fn task_override_wins_over_default() {
        let resolved = config().resolve(Some("codex")).unwrap();
        assert_eq!(resolved.name, "codex");
        assert_eq!(resolved.cmd, "codex exec {prompt_file}");
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

    #[test]
    fn starter_config_parses_and_resolves() {
        let config = AgentsConfig::parse(STARTER_CONFIG, Path::new("/tmp/agents.toml")).unwrap();
        assert_eq!(config.default_name(), "claude");
        assert_eq!(config.resolve(None).unwrap().name, "claude");
        assert_eq!(config.agent_names(), vec!["claude"]);
    }

    #[test]
    fn entries_lists_name_and_cmd() {
        let config = config();
        let entries: Vec<_> = config.entries().collect();
        assert_eq!(
            entries,
            vec![
                (
                    "claude",
                    "claude -p --output-format stream-json {prompt_file}"
                ),
                ("codex", "codex exec {prompt_file}"),
            ]
        );
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
