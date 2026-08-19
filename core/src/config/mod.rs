//! Per-project and global TOML configuration.
//!
//! Two files, both optional:
//!
//! - `~/.g-mesh/projects/<hash>/config.toml` - [`ProjectConfig`], one
//!   developer's preferences for one project (plugin idle timeout, core idle
//!   timeout, embedding model). Lives next to the project's `index.db`,
//!   under the same per-project state directory
//!   `storage::connection::project_dir` already computes from the project
//!   root's hash - so this module does not invent a second way to find "the"
//!   directory for a project, it reuses the one the index already lives in.
//! - `~/.g-mesh/config.toml` - [`GlobalConfig`], settings that apply across
//!   every project (currently just the GC warning switch and threshold).
//!
//! Neither file is ever inside a project's own git repo - both live under
//! the user's home directory, the same as the index itself - so there is
//! nothing here for a project's `.gitignore` to need to know about.
//!
//! # Missing file is not an error
//!
//! A project (or a machine) that has never run `g-mesh config` has no
//! `config.toml` at all, and that has to be exactly as valid as one that
//! spells out every field: [`read_project_config`] and [`read_global_config`]
//! (and the lower-level [`read_toml_or_default`] they share) return the
//! documented defaults for a missing file rather than erroring. A config file
//! that exists but only sets *some* fields gets the same treatment field by
//! field, via `#[serde(default)]` on every struct here - so hand-editing in
//! just one setting does not require filling in the rest.
//!
//! # Why plain structs instead of hand-rolled parsing
//!
//! Every field already has a documented default (see the architecture doc's
//! Lifecycle & Operational Model section), so the natural representation is
//! a `serde`-derived struct with a `Default` impl per section - the same
//! pattern this crate already uses for its `serde_json` types, just fed
//! through the `toml` crate's (de)serializer instead of JSON's.
//!
//! # Not wired to runtime behavior yet
//!
//! Reading/writing/round-tripping these files is what this module is for.
//! Nothing here changes how long a plugin process actually stays alive or
//! which embedding model actually runs - that is each field's own
//! consumer's job (e.g. task #38 for `plugin.idleTimeoutMinutes`), wiring up
//! later against a config surface that already round-trips correctly today.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::storage::connection::project_dir;

const CONFIG_FILE_NAME: &str = "config.toml";

// ---------------------------------------------------------------------------
// Per-project config
// ---------------------------------------------------------------------------

/// One project's preferences, stored at
/// `~/.g-mesh/projects/<hash>/config.toml`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub plugin: PluginConfig,
    pub daemon: DaemonConfig,
    pub embedding: EmbeddingConfig,
}

/// `[plugin]`: how long the language plugin process is allowed to sit idle
/// before it is put to sleep. See `daemon::plugin` / task #38 for the
/// consumer - not wired to the real timeout yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginConfig {
    pub idle_timeout_minutes: u64,
}

impl Default for PluginConfig {
    fn default() -> Self {
        // Documented default: architecture doc, "plugin.idleTimeoutMinutes
        // (default 1h)".
        Self { idle_timeout_minutes: 60 }
    }
}

/// `[daemon]`: how long the daemon core is allowed to sit idle (zero MCP
/// requests) before it exits cleanly. Not wired to the real daemon lifecycle
/// yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DaemonConfig {
    pub core_idle_timeout_hours: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        // Documented default: architecture doc,
        // "daemon.coreIdleTimeoutHours (default 24h)".
        Self { core_idle_timeout_hours: 24 }
    }
}

/// `[embedding]`: which embedding model this project's semantic search
/// (not yet shipped - see architecture doc's Open Questions) should use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct EmbeddingConfig {
    pub model: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        // Documented default: architecture doc's planned default
        // (`meta.embedding_model` / jina-embeddings-v2-base-code).
        Self { model: "jina-embeddings-v2-base-code".to_string() }
    }
}

// ---------------------------------------------------------------------------
// Global config
// ---------------------------------------------------------------------------

/// Settings shared across every project, stored at `~/.g-mesh/config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct GlobalConfig {
    pub cleanup: CleanupConfig,
}

/// `[cleanup]`: the GC warning `cli::clean` and every other interactive
/// command are documented to print (never an automatic deletion - see
/// `cli::clean`'s module docs), plus the threshold `cli::clean::run` uses to
/// decide what `clean expired` treats as stale. `idle_threshold_days` is
/// wired to that deletion decision; `enabled` is not consulted there - it
/// only gates the warning print, a separate consumer (task #62).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CleanupConfig {
    /// Whether interactive commands print the idle-project warning at all.
    /// The architecture doc does not pin a default for the switch itself
    /// (only for the threshold) - `true` is chosen here so the warning this
    /// feature exists to show is on by default, matching the "warn, never
    /// delete automatically" framing the rest of `cleanup` follows.
    pub enabled: bool,
    pub idle_threshold_days: u64,
}

impl Default for CleanupConfig {
    fn default() -> Self {
        // Documented default: architecture doc, "cleanup.idleThresholdDays
        // (default 90)" - what `cli::clean::run` falls back to when no
        // `~/.g-mesh/config.toml` exists.
        Self { enabled: true, idle_threshold_days: 90 }
    }
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

/// `~/.g-mesh/projects/<hash>/config.toml` for the given (canonicalized)
/// project root - the same per-project directory
/// `storage::connection::project_dir` computes for `index.db`, so a
/// project's config and its index always agree on where they live.
pub fn project_config_path(root: &Path) -> Result<PathBuf> {
    Ok(project_dir(root)?.join(CONFIG_FILE_NAME))
}

/// `~/.g-mesh/config.toml`, or `$G_MESH_HOME/config.toml`.
pub fn global_config_path() -> Result<PathBuf> {
    Ok(crate::paths::g_mesh_home()?.join(CONFIG_FILE_NAME))
}

// ---------------------------------------------------------------------------
// Read / write
// ---------------------------------------------------------------------------

/// Reads the project's `config.toml`, or the documented defaults if it does
/// not exist.
pub fn read_project_config(root: &Path) -> Result<ProjectConfig> {
    read_toml_or_default(&project_config_path(root)?)
}

/// Writes the project's `config.toml`, creating its state directory
/// (`~/.g-mesh/projects/<hash>/`) if this is the first thing to touch it.
pub fn write_project_config(root: &Path, config: &ProjectConfig) -> Result<()> {
    write_toml(&project_config_path(root)?, config)
}

/// Reads the global `config.toml`, or the documented defaults if it does
/// not exist.
pub fn read_global_config() -> Result<GlobalConfig> {
    read_toml_or_default(&global_config_path()?)
}

/// Writes the global `config.toml`, creating `~/.g-mesh/` if needed.
pub fn write_global_config(config: &GlobalConfig) -> Result<()> {
    write_toml(&global_config_path()?, config)
}

/// Deserializes `T` from `path`, or `T::default()` if `path` does not exist.
/// Any other I/O error (permissions, a directory where a file was expected)
/// or a file that exists but does not parse as valid TOML for `T` is still
/// reported - only "there is no file here yet" is treated as "use the
/// defaults".
fn read_toml_or_default<T: Default + DeserializeOwned>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents)
            .with_context(|| format!("failed to parse config at {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read config at {}", path.display()))
        }
    }
}

fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    let contents =
        toml::to_string_pretty(value).context("failed to serialize config to TOML")?;
    fs::write(path, contents)
        .with_context(|| format!("failed to write config at {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_project_config_reads_as_documented_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config: ProjectConfig = read_toml_or_default(&path).unwrap();
        assert_eq!(config, ProjectConfig::default());
        assert_eq!(config.plugin.idle_timeout_minutes, 60);
        assert_eq!(config.daemon.core_idle_timeout_hours, 24);
        assert_eq!(config.embedding.model, "jina-embeddings-v2-base-code");
    }

    #[test]
    fn a_missing_global_config_reads_as_documented_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config: GlobalConfig = read_toml_or_default(&path).unwrap();
        assert_eq!(config, GlobalConfig::default());
        assert!(config.cleanup.enabled);
        assert_eq!(config.cleanup.idle_threshold_days, 90);
    }

    #[test]
    fn writing_then_reading_a_project_config_round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config = ProjectConfig {
            plugin: PluginConfig { idle_timeout_minutes: 15 },
            daemon: DaemonConfig { core_idle_timeout_hours: 8 },
            embedding: EmbeddingConfig { model: "custom-model".to_string() },
        };
        write_toml(&path, &config).unwrap();

        let read_back: ProjectConfig = read_toml_or_default(&path).unwrap();
        assert_eq!(read_back, config);
    }

    #[test]
    fn writing_then_reading_a_global_config_round_trips_every_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let config =
            GlobalConfig { cleanup: CleanupConfig { enabled: false, idle_threshold_days: 30 } };
        write_toml(&path, &config).unwrap();

        let read_back: GlobalConfig = read_toml_or_default(&path).unwrap();
        assert_eq!(read_back, config);
    }

    #[test]
    fn a_partial_project_config_falls_back_to_defaults_for_omitted_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[plugin]\nidleTimeoutMinutes = 5\n").unwrap();

        let config: ProjectConfig = read_toml_or_default(&path).unwrap();
        assert_eq!(config.plugin.idle_timeout_minutes, 5);
        // Omitted [daemon] and [embedding] tables still fall back to their
        // documented defaults.
        assert_eq!(config.daemon.core_idle_timeout_hours, 24);
        assert_eq!(config.embedding.model, "jina-embeddings-v2-base-code");
    }

    #[test]
    fn writing_a_project_config_creates_the_state_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("does").join("not").join("exist");
        let path = nested.join("config.toml");

        write_toml(&path, &ProjectConfig::default()).unwrap();

        assert!(path.exists());
    }

    /// Field names on disk are the camelCase form the architecture doc
    /// documents (`idleTimeoutMinutes`, `coreIdleTimeoutHours`,
    /// `idleThresholdDays`), not Rust's snake_case.
    #[test]
    fn field_names_on_disk_are_camel_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_toml(&path, &ProjectConfig::default()).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("idleTimeoutMinutes"), "{contents}");
        assert!(contents.contains("coreIdleTimeoutHours"), "{contents}");

        let global_dir = tempfile::tempdir().unwrap();
        let global_path = global_dir.path().join("config.toml");
        write_toml(&global_path, &GlobalConfig::default()).unwrap();
        let global_contents = fs::read_to_string(&global_path).unwrap();
        assert!(global_contents.contains("idleThresholdDays"), "{global_contents}");
    }

    /// [`project_config_path`] and [`global_config_path`] resolve without
    /// touching the filesystem beyond a directory canonicalization - neither
    /// creates anything, so pointing them at real inputs in a test cannot
    /// leave anything behind on the developer's machine.
    #[test]
    fn project_config_path_is_config_toml_inside_the_projects_state_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = project_dir(tmp.path()).unwrap().join("config.toml");
        assert_eq!(project_config_path(tmp.path()).unwrap(), expected);
    }

    #[test]
    fn global_config_path_is_config_toml_directly_under_the_g_mesh_home() {
        let home = crate::paths::g_mesh_home().unwrap();
        assert_eq!(global_config_path().unwrap(), home.join("config.toml"));
    }
}
