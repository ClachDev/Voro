//! Comment-preserving edits to the user's `voro.toml` (DESIGN.md §5). Where
//! [`crate::agent::AgentsConfig`] *reads* the file, this *writes* it: the single
//! helper the TUI Config screen and the `voro viewer` CLI verbs both route
//! through, so the two never diverge on formatting or validation.
//!
//! Edits go through `toml_edit`, which round-trips a document preserving its
//! existing content, whitespace, and comments — only the touched key changes.
//! A missing file is created (with its parent directory); the built-ins mean an
//! absent file is still a working config, so the first edit is what brings the
//! file into existence.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, value};

use crate::agent::VIEWER_PATH_PLACEHOLDER;
use crate::error::{Error, Result};
use crate::model::{Project, ReviewAction};

/// Add a `[viewers.<name>]` table with the given command, refusing an empty
/// name/command or a name that collides with an existing viewer. Existing
/// content and comments in the file are preserved.
pub fn add_viewer(path: &Path, name: &str, cmd: &str) -> Result<()> {
    let name = name.trim();
    let cmd = cmd.trim();
    validate_viewer(name, cmd)?;
    let mut doc = load_doc(path)?;
    if viewer_exists(&doc, name) {
        return Err(invalid(format!(
            "a viewer named '{name}' already exists — edit it, or pick another name"
        )));
    }
    set_viewer_cmd(&mut doc, name, cmd)?;
    write_doc(path, &doc)
}

/// Replace an existing viewer's command, refusing an empty command or a name
/// that names no viewer. The rest of the table (and file) is untouched.
pub fn edit_viewer(path: &Path, name: &str, cmd: &str) -> Result<()> {
    let name = name.trim();
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Err(invalid("viewer command is required".into()));
    }
    let mut doc = load_doc(path)?;
    if !viewer_exists(&doc, name) {
        return Err(invalid(format!("no viewer named '{name}' to edit")));
    }
    set_viewer_cmd(&mut doc, name, cmd)?;
    write_doc(path, &doc)
}

/// Remove the named viewer, returning whether `default_viewer` was cleared
/// because it pointed at the deleted viewer. Refuses a name that names no
/// viewer. The referenced-by-a-project refusal lives at the call site, which
/// has the project list to name the offenders (see [`projects_referencing_viewer`]).
pub fn delete_viewer(path: &Path, name: &str) -> Result<bool> {
    let name = name.trim();
    let mut doc = load_doc(path)?;
    if !viewer_exists(&doc, name) {
        return Err(invalid(format!("no viewer named '{name}' to delete")));
    }
    remove_viewer(&mut doc, name);
    let cleared = default_viewer_matches(&doc, name);
    if cleared {
        doc.remove("default_viewer");
    }
    write_doc(path, &doc)?;
    Ok(cleared)
}

/// Set `default_viewer` to an existing viewer, validated so the picker can't
/// point the default at a viewer that isn't there.
pub fn set_default_viewer(path: &Path, name: &str) -> Result<()> {
    let name = name.trim();
    let mut doc = load_doc(path)?;
    if !viewer_exists(&doc, name) {
        return Err(invalid(format!(
            "no viewer named '{name}' — define it before making it the default"
        )));
    }
    doc["default_viewer"] = value(name);
    write_doc(path, &doc)
}

/// Set `default_agent`. Existence against the built-in + user agent set is the
/// caller's to validate (the built-ins live in code, not the file); the picker
/// only offers configured names, so this just records the choice.
pub fn set_default_agent(path: &Path, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        return Err(invalid("agent name is required".into()));
    }
    let mut doc = load_doc(path)?;
    doc["default_agent"] = value(name);
    write_doc(path, &doc)
}

/// Whether a viewer command lacks the `{path}` placeholder — a warning, not an
/// error (DESIGN.md §5): such a command runs in the checkout's own directory,
/// which is occasionally what a `git difftool -d` wants but usually a mistake.
pub fn missing_path_placeholder(cmd: &str) -> bool {
    !cmd.contains(VIEWER_PATH_PLACEHOLDER)
}

/// The projects whose review action names this viewer explicitly
/// (`viewer:<name>`), so deleting it can be refused with them named (DESIGN.md
/// §5). A project on `viewer` (the unnamed default) is not counted — it follows
/// whatever the default resolves to rather than pinning this name.
pub fn projects_referencing_viewer<'a>(projects: &'a [Project], name: &str) -> Vec<&'a Project> {
    projects
        .iter()
        .filter(|p| matches!(&p.review_action, ReviewAction::Viewer(Some(n)) if n == name))
        .collect()
}

fn validate_viewer(name: &str, cmd: &str) -> Result<()> {
    if name.is_empty() {
        return Err(invalid("viewer name is required".into()));
    }
    // A name with whitespace or a colon cannot be referenced as `viewer:<name>`
    // by a project's review action, so refuse it rather than create an
    // unreachable viewer.
    if name.chars().any(|c| c.is_whitespace() || c == ':') {
        return Err(invalid(format!(
            "viewer name '{name}' cannot contain spaces or ':'"
        )));
    }
    if cmd.is_empty() {
        return Err(invalid("viewer command is required".into()));
    }
    Ok(())
}

fn invalid(message: String) -> Error {
    Error::Invalid(message)
}

/// Read the file into an editable document, or an empty one when it does not
/// exist yet — the first edit creates the file.
fn load_doc(path: &Path) -> Result<DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<DocumentMut>()
            .map_err(|e| Error::AgentConfigInvalid {
                path: path.to_path_buf(),
                message: e.to_string(),
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DocumentMut::new()),
        Err(e) => Err(Error::AgentConfigInvalid {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
    }
}

fn write_doc(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::AgentConfigInvalid {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
    }
    std::fs::write(path, doc.to_string()).map_err(|e| Error::AgentConfigInvalid {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

fn viewer_exists(doc: &DocumentMut, name: &str) -> bool {
    doc.get("viewers")
        .and_then(Item::as_table_like)
        .is_some_and(|t| t.contains_key(name))
}

/// Set `viewers.<name>.cmd`, creating the `[viewers]` table (implicit, so no
/// bare `[viewers]` header is emitted) and the named subtable as needed. An
/// existing named table keeps its formatting and any sibling keys.
fn set_viewer_cmd(doc: &mut DocumentMut, name: &str, cmd: &str) -> Result<()> {
    if doc.get("viewers").is_none() {
        let mut table = Table::new();
        table.set_implicit(true);
        doc.insert("viewers", Item::Table(table));
    }
    let viewers = doc["viewers"].as_table_mut().ok_or_else(|| {
        invalid("`viewers` in voro.toml is not a table — fix it by hand first".into())
    })?;
    // Update an existing subtable in place (keeping its formatting), or insert a
    // fresh `[viewers.<name>]` table. Inserting a whole subtable — rather than a
    // dotted `viewers.<name>.cmd` key — is what keeps `viewers` a header-less
    // implicit parent and renders the new entry as its own `[viewers.<name>]`.
    match viewers.get_mut(name).and_then(Item::as_table_mut) {
        Some(existing) => existing["cmd"] = value(cmd),
        None => {
            let mut table = Table::new();
            table["cmd"] = value(cmd);
            viewers.insert(name, Item::Table(table));
        }
    }
    Ok(())
}

fn remove_viewer(doc: &mut DocumentMut, name: &str) {
    if let Some(viewers) = doc.get_mut("viewers").and_then(Item::as_table_mut) {
        viewers.remove(name);
        if viewers.is_empty() {
            doc.remove("viewers");
        }
    }
}

fn default_viewer_matches(doc: &DocumentMut, name: &str) -> bool {
    doc.get("default_viewer").and_then(Item::as_str) == Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentsConfig;

    /// A unique scratch path per test, cleaned up by the caller.
    fn scratch(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "voro-config-edit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn add_viewer_creates_the_file_when_missing() {
        let dir = scratch("create");
        let path = dir.join("voro/voro.toml");
        assert!(!path.exists());

        add_viewer(&path, "zed", "zed {path}").unwrap();
        assert!(path.exists());

        let config = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.viewer_cmd(Some("zed")).unwrap(), "zed {path}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_viewer_preserves_existing_content_and_comments() {
        let dir = scratch("preserve");
        let path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();
        let original = "\
# my hand-written config
default_agent = \"claude\"

[viewers.difftool]
cmd = \"git -C {path} difftool -d {base}...{branch}\"  # inline note
";
        std::fs::write(&path, original).unwrap();

        add_viewer(&path, "zed", "zed {path}").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my hand-written config"), "{text}");
        assert!(text.contains("# inline note"), "{text}");
        assert!(text.contains("[viewers.difftool]"), "{text}");
        assert!(text.contains("[viewers.zed]"), "{text}");

        let config = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.viewer_names(), vec!["difftool", "zed"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn add_viewer_rejects_duplicates_and_empty_fields() {
        let dir = scratch("reject");
        let path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();

        add_viewer(&path, "zed", "zed {path}").unwrap();
        let dup = add_viewer(&path, "zed", "zed .").unwrap_err().to_string();
        assert!(dup.contains("already exists"), "{dup}");

        let empty_cmd = add_viewer(&path, "emacs", "  ").unwrap_err().to_string();
        assert!(empty_cmd.contains("command is required"), "{empty_cmd}");

        let empty_name = add_viewer(&path, "  ", "zed .").unwrap_err().to_string();
        assert!(empty_name.contains("name is required"), "{empty_name}");

        let bad_name = add_viewer(&path, "a:b", "zed .").unwrap_err().to_string();
        assert!(bad_name.contains("cannot contain"), "{bad_name}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_viewer_updates_the_command() {
        let dir = scratch("edit");
        let path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();

        add_viewer(&path, "zed", "zed {path}").unwrap();
        edit_viewer(&path, "zed", "zed --new {path}").unwrap();
        let config = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.viewer_cmd(Some("zed")).unwrap(), "zed --new {path}");

        let missing = edit_viewer(&path, "nope", "x {path}")
            .unwrap_err()
            .to_string();
        assert!(missing.contains("no viewer named 'nope'"), "{missing}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_viewer_removes_it_and_clears_a_matching_default() {
        let dir = scratch("delete");
        let path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();

        add_viewer(&path, "zed", "zed {path}").unwrap();
        add_viewer(&path, "emacs", "emacsclient {path}").unwrap();
        set_default_viewer(&path, "zed").unwrap();

        // deleting a non-default viewer leaves the default alone
        let cleared = delete_viewer(&path, "emacs").unwrap();
        assert!(!cleared);
        assert_eq!(
            AgentsConfig::load(&path)
                .unwrap()
                .default_viewer_name()
                .as_deref(),
            Some("zed")
        );

        // deleting the default clears default_viewer
        let cleared = delete_viewer(&path, "zed").unwrap();
        assert!(cleared);
        let config = AgentsConfig::load(&path).unwrap();
        assert!(config.viewer_names().is_empty());
        assert_eq!(config.default_viewer_name(), None);

        let missing = delete_viewer(&path, "zed").unwrap_err().to_string();
        assert!(missing.contains("no viewer named"), "{missing}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_defaults_validate_and_record() {
        let dir = scratch("defaults");
        let path = dir.join("voro.toml");
        std::fs::create_dir_all(&dir).unwrap();

        add_viewer(&path, "zed", "zed {path}").unwrap();
        set_default_viewer(&path, "zed").unwrap();
        set_default_agent(&path, "codex").unwrap();
        let config = AgentsConfig::load(&path).unwrap();
        assert_eq!(config.default_viewer_name().as_deref(), Some("zed"));
        assert_eq!(config.default_name().as_deref(), Some("codex"));

        let bad = set_default_viewer(&path, "ghost").unwrap_err().to_string();
        assert!(bad.contains("no viewer named 'ghost'"), "{bad}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_path_placeholder_flags_a_bare_command() {
        assert!(missing_path_placeholder("git difftool -d"));
        assert!(!missing_path_placeholder("zed {path}"));
    }
}
