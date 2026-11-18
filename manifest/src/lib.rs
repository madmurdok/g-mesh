//! The shape of `plugin.toml`, one set of types for every reader of the file.
//!
//! Three programs read a plugin's manifest: core at discovery
//! (`plugin_manifest::read_manifest`), the plugin SDK for its walk
//! (`g_mesh_plugin_sdk::ResolvedSpec`) and the SDK's LSP bridge for its
//! server (`g_mesh_plugin_sdk_lsp::SemanticConfig`). Each needs a different
//! part of the file and treats a problem differently: core refuses a manifest
//! it cannot use, the SDK falls back to the spec the plugin declared in code,
//! and the LSP bridge reports a broken `[plugin.semantic]` while ignoring the
//! rest. So this crate owns the shape and no rule:
//!
//! - **Lenient.** Every field is an `Option` or has a default, and no type
//!   denies unknown keys, so a manifest parses here whatever a given reader
//!   requires of it. Requiring `language`, `spawn` or `extensions` is the
//!   reader's job, with the reader's own message.
//! - **Typed where every reader agrees on the type.** A key present with the
//!   wrong type (`semantic_pass = "yes"`) fails [`parse`] for every reader.
//! - **Raw where only one reader looks.** `[plugin.non_symbol_queries]`,
//!   `[plugin.symbol_query_prefixes]` and `[plugin.reexports]` are core's
//!   alone and stay [`toml::Value`]s here; core decodes them with its own
//!   strict types ([`decode_table`] keeps their `deny_unknown_fields`).
//!   `[plugin.semantic]` is the LSP bridge's alone and stays raw too, so a
//!   typo in a server setting is the bridge's error to report
//!   ([`PluginSection::semantic_section`]) and never stops core discovering
//!   the plugin's structural tier.

use serde::de::DeserializeOwned;
use serde::Deserialize;

pub use toml::de::Error;
pub use toml::Value;

/// Parses a `plugin.toml`. Fails only on malformed TOML or a known key of the
/// wrong type; a missing section is `None` or its default.
pub fn parse(text: &str) -> Result<Manifest, Error> {
    toml::from_str(text)
}

/// Decodes one raw table with the caller's own type, or `T::default()` when
/// the table is absent. A strict `T` (`deny_unknown_fields`) stays strict.
pub fn decode_table<T: DeserializeOwned + Default>(table: Option<&Value>) -> Result<T, Error> {
    match table {
        Some(value) => value.clone().try_into(),
        None => Ok(T::default()),
    }
}

/// The whole file.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Manifest {
    /// `[plugin]`. Every key a reader uses lives under it.
    pub plugin: Option<PluginSection>,
}

/// `[plugin]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PluginSection {
    /// The plugin's wire identifier; equals its directory's name.
    pub language: Option<String>,
    pub protocol_version: Option<u32>,
    pub plugin_version: Option<String>,
    /// `[plugin.spawn]`: how core starts the plugin.
    pub spawn: Option<SpawnSection>,
    /// `[plugin.languages]`: the extensions the plugin claims.
    pub languages: Option<LanguagesSection>,
    /// `[plugin.fingerprint]`.
    #[serde(default)]
    pub fingerprint: FingerprintSection,
    /// `[plugin.capabilities]`.
    #[serde(default)]
    pub capabilities: CapabilitiesSection,
    /// `[plugin.workspace]`.
    #[serde(default)]
    pub workspace: WorkspaceSection,
    /// `[plugin.semantic]`, raw: see [`PluginSection::semantic_section`].
    pub semantic: Option<Value>,
    /// `[plugin.non_symbol_queries]`, raw: core decodes it.
    pub non_symbol_queries: Option<Value>,
    /// `[plugin.symbol_query_prefixes]`, raw: core decodes it.
    pub symbol_query_prefixes: Option<Value>,
    /// `[plugin.reexports]`, raw: core decodes it.
    pub reexports: Option<Value>,
}

impl PluginSection {
    /// `[plugin.semantic]` decoded, `None` when the table is absent. An error
    /// is a table present with a key of the wrong type.
    pub fn semantic_section(&self) -> Result<Option<SemanticSection>, Error> {
        self.semantic.clone().map(Value::try_into).transpose()
    }
}

/// `[plugin.spawn]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SpawnSection {
    /// argv\[0\]: a bare name, a path relative to the manifest, or one under
    /// `${G_MESH_BIN_DIR}`.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

/// `[plugin.languages]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct LanguagesSection {
    /// Lowercase, dot-prefixed (`".py"`).
    pub extensions: Option<Vec<String>>,
}

/// `[plugin.fingerprint]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct FingerprintSection {
    /// Directory names skipped when fingerprinting the plugin's own files.
    #[serde(default)]
    pub ignore: Vec<String>,
}

/// `[plugin.capabilities]`, each key as written. The enum-valued keys are
/// strings here; core maps them to its own enums and rejects an unknown word.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CapabilitiesSection {
    pub semantic_pass: Option<bool>,
    pub semantic_sweep: Option<bool>,
    pub semantic_prepare: Option<bool>,
    pub files_created: Option<bool>,
    pub resolution_delta: Option<bool>,
    /// `"resolved"` or `"unresolved"`.
    pub receiver_calls: Option<String>,
    /// `"resolved"` or `"unresolved"`.
    pub receiver_calls_structural: Option<String>,
    /// `"none"`, `"by_name"` or `"declared"`.
    pub member_overrides: Option<String>,
}

/// `[plugin.workspace]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WorkspaceSection {
    /// Exact file names or globs whose change reindexes the language.
    #[serde(default)]
    pub watch_files: Vec<String>,
    /// Directory names the plugin's walk and core's watcher skip.
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
    /// Names a miss-path lookup treats as a container's entry point.
    #[serde(default)]
    pub entry_points: Vec<String>,
}

/// `[plugin.semantic]`: the language server the LSP bridge runs.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct SemanticSection {
    /// Required by the bridge, which reports its absence.
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    pub engine: Option<String>,
    #[serde(default)]
    pub implementation_kinds: Vec<String>,
    pub initialization_options: Option<Value>,
    pub settings: Option<Value>,
    /// `"indexed"` or `"on-demand"`, checked by the bridge.
    pub readiness: Option<String>,
    /// `"none"` or `"hover"`, checked by the bridge.
    pub overload_disambiguation: Option<String>,
}
