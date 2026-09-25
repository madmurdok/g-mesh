//! Reads and validates one plugin's `plugin.toml` manifest, and discovers the
//! plugins under the plugin roots. Schema: `docs/architecture/plugin-modularity.md`
//! and `docs/architecture/multi-language-plugins.md`; decisions:
//! `docs/adr/0005-plugin-manifest.md`.
//!
//! Validation is a hard failure: every check bails with an error naming the
//! manifest path and the problem, never a partial or best-guess manifest.
//! `command` and path-like `args` are resolved here (see `resolve_path_entry`).
//! An absent `[plugin.capabilities]` or `[plugin.workspace]` takes the
//! conservative default: a manifest that says nothing can do the least.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::Glob;
use serde::Deserialize;

use crate::protocol::types::CURRENT_PROTOCOL_VERSION;

const MANIFEST_FILE_NAME: &str = "plugin.toml";

/// The `command` prefix that stands for [`current_bin_dir`]. Expanded only in
/// `command` and only as a prefix; anywhere else it is an error.
pub const BIN_DIR_PLACEHOLDER: &str = "${G_MESH_BIN_DIR}";

/// The directory of the running g-mesh executable, which [`BIN_DIR_PLACEHOLDER`]
/// expands to. `None` only when this process cannot resolve its own path.
pub fn current_bin_dir() -> Option<PathBuf> {
    bin_dir_of(&std::env::current_exe().ok()?)
}

/// [`current_bin_dir`] for `exe`. A parent named `deps` (a cargo test binary in
/// `target/<profile>/deps/`) is stepped over, so a test resolves the same
/// profile directory the real binary would.
fn bin_dir_of(exe: &Path) -> Option<PathBuf> {
    let parent = exe.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("deps") {
        return parent.parent().map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

/// Where a spawned plugin is told to find the manifest core read about it.
/// Must match `g_mesh_plugin_sdk::MANIFEST_PATH_ENV` (core does not depend on
/// the SDK); a divergence silently drops an SDK plugin to its in-code spec.
pub const MANIFEST_PATH_ENV: &str = "G_MESH_PLUGIN_MANIFEST";

/// Overrides [`default_roots`]'s entire return value with a single directory
/// (tests point discovery at one fixture directory). Real installs never set it.
pub const PLUGIN_ROOTS_OVERRIDE_ENV: &str = "G_MESH_PLUGIN_ROOTS_OVERRIDE";

/// The roots [`discover`] scans, in precedence order: `~/.g-mesh/plugins/`
/// (skipped if the home directory cannot be resolved), then [`bundled_roots`].
/// With [`PLUGIN_ROOTS_OVERRIDE_ENV`] set, that one path replaces them all.
pub fn default_roots() -> Vec<PathBuf> {
    if let Ok(over) = std::env::var(PLUGIN_ROOTS_OVERRIDE_ENV) {
        return vec![PathBuf::from(over)];
    }

    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".g-mesh").join("plugins"));
    }
    roots.extend(bundled_roots());
    roots
}

/// Where the plugins that ship with g-mesh live: `plugins/` beside the
/// executable (installed) first, then `CARGO_MANIFEST_DIR/../plugins`
/// (checkout). A missing root contributes nothing, so both are always listed;
/// a failing [`std::env::current_exe`] is not an error.
pub fn bundled_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(installed) = installed_bundled_root() {
        roots.push(installed);
    }
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins"));
    roots
}

/// `plugins/` next to the running executable - see [`bundled_roots`]. `None`
/// only when this process cannot resolve its own path at all.
pub fn installed_bundled_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("plugins"))
}

/// Whether receiver calls (`x.foo()`) resolve to an edge, at one of the two
/// tiers `[plugin.capabilities]` asks about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiverCallResolution {
    Resolved,
    Unresolved,
}

impl std::fmt::Display for ReceiverCallResolution {
    /// The same lowercase word the manifest uses, so `cli::plugins` needs no
    /// second mapping.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReceiverCallResolution::Resolved => "resolved",
            ReceiverCallResolution::Unresolved => "unresolved",
        })
    }
}

impl Default for ReceiverCallResolution {
    /// `Unresolved`, the conservative default wherever the field is missing:
    /// assuming "resolved" would silently drop edges a caller was told to expect.
    fn default() -> Self {
        ReceiverCallResolution::Unresolved
    }
}

/// What core may ask this plugin to do, and how far each tier's receiver-call
/// resolution can be trusted. Read from the manifest, not the handshake: it is
/// needed before any plugin process exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// Whether core may send this plugin a `semanticPass` request (per file after
    /// a reparse, whole project after a walk). `false`: core never sends one and
    /// never requires an answer to one.
    pub semantic_pass: bool,
    /// Whether receiver calls resolve to edges once this plugin's best available
    /// tier has run. `Resolved` means against the receiver's declared or inferred
    /// type, never its run-time type (`mcp::instructions`' `P4_STATIC_RECEIVER`
    /// discloses this). `Resolved`: the MCP instructions do not list the open
    /// receiver-call gap for this language; `Unresolved` (the default): they do.
    pub receiver_calls: ReceiverCallResolution,
    /// Whether the structural tier alone resolves receiver calls. Separate from
    /// `receiver_calls`: a plugin can be `Unresolved` here and `Resolved` there
    /// (resolved once `semanticPassAt` is set for the language).
    pub receiver_calls_structural: ReceiverCallResolution,
}

/// Which files and directories route to this plugin beyond its extensions, and
/// which names a miss-path lookup treats as a container's entry point. An
/// absent `[plugin.workspace]` leaves all three empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// Exact file names (any directory) or glob patterns (`*.csproj`), compiled
    /// and validated by [`read_manifest`]; an exact name is a glob matching only
    /// itself. A change to a matching file triggers a per-language reindex.
    pub watch_files: Vec<Glob>,
    /// Directory names this plugin's walk never descends into and the watcher
    /// never routes to it. Exact names, not globs.
    pub exclude_dirs: Vec<String>,
    /// File or directory names a miss-path lookup treats as this container's
    /// entry point (e.g. rust: `lib.rs`, `main.rs`, `mod.rs`; typescript:
    /// `index`). Exact names, not globs.
    pub entry_points: Vec<String>,
}

/// One plugin directory's fully resolved manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    /// The plugin's wire identifier (`Handshake.language`). Always equal to
    /// `manifest_dir`'s final path component, enforced by [`read_manifest`].
    pub language: String,
    pub protocol_version: u32,
    pub plugin_version: String,
    /// Resolved argv\[0\]: a bare command name (looked up on `$PATH` at spawn
    /// time) or an absolute path joined against `manifest_dir`.
    pub command: PathBuf,
    /// Resolved extra argv entries, in declared order.
    pub args: Vec<String>,
    /// Lowercase, leading-dot extensions this plugin claims (e.g. `".py"`). A
    /// manifest convention, not checked here.
    pub extensions: Vec<String>,
    /// Directory names to skip, on top of the built-in baseline, when
    /// fingerprinting this plugin's own files. Empty without `[plugin.fingerprint]`.
    pub fingerprint_ignore: Vec<String>,
    /// This plugin's own directory, the one `plugin.toml` was read from; used for
    /// error messages and fingerprinting.
    pub manifest_dir: PathBuf,
    /// Parsed `[plugin.capabilities]`, or [`Capabilities::default`] if the table
    /// is absent.
    pub capabilities: Capabilities,
    /// Parsed `[plugin.workspace]`, or [`WorkspaceConfig::default`] if the table
    /// is absent.
    pub workspace: WorkspaceConfig,
}

impl PluginManifest {
    /// The file this manifest was read from - what every spawn site hands the
    /// plugin as [`MANIFEST_PATH_ENV`], so the plugin reads the same file core
    /// did rather than looking for one beside its own binary.
    pub fn path(&self) -> PathBuf {
        self.manifest_dir.join(MANIFEST_FILE_NAME)
    }
}

/// Reads and validates `<dir>/plugin.toml`. Hard error on malformed TOML
/// (including an unknown `receiver_calls` value), a missing field, `language`
/// not equal to `dir`'s name, an unknown `protocol_version`, or an invalid
/// `watch_files` glob. Every error names the manifest path; the `language` and
/// `protocol_version` errors also name the declared and expected values.
pub fn read_manifest(dir: &Path) -> Result<PluginManifest> {
    let manifest_path = dir.join(MANIFEST_FILE_NAME);

    let contents = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read plugin manifest at {}", manifest_path.display()))?;
    let raw: RawManifest = toml::from_str(&contents)
        .with_context(|| format!("failed to parse plugin manifest at {}", manifest_path.display()))?;
    let plugin = raw.plugin;

    let dir_name = dir.file_name().and_then(|name| name.to_str()).with_context(|| {
        format!("plugin manifest directory {} has no valid (UTF-8) directory name", dir.display())
    })?;
    if plugin.language != dir_name {
        bail!(
            "plugin manifest at {} declares language \"{}\", but its containing directory is \
             named \"{}\" - the manifest's language must match the directory it lives in",
            manifest_path.display(),
            plugin.language,
            dir_name,
        );
    }

    if plugin.protocol_version != CURRENT_PROTOCOL_VERSION {
        bail!(
            "plugin manifest at {} declares protocol_version {}, but core expects protocol \
             version {} - refusing to load; update the plugin to match",
            manifest_path.display(),
            plugin.protocol_version,
            CURRENT_PROTOCOL_VERSION,
        );
    }

    let command =
        resolve_command(&plugin.spawn.command, dir, current_bin_dir().as_deref()).with_context(|| {
            format!("invalid [plugin.spawn] command in plugin manifest at {}", manifest_path.display())
        })?;
    let args = plugin.spawn.args.iter().map(|arg| resolve_arg(arg, dir)).collect();

    // Globs are validated here, not by TOML parsing: any string is valid TOML.
    let watch_files = plugin
        .workspace
        .watch_files
        .iter()
        .map(|pattern| {
            Glob::new(pattern).with_context(|| {
                format!(
                    "plugin manifest at {} declares an invalid glob \"{}\" in \
                     [plugin.workspace] watch_files",
                    manifest_path.display(),
                    pattern,
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(PluginManifest {
        language: plugin.language,
        protocol_version: plugin.protocol_version,
        plugin_version: plugin.plugin_version,
        command,
        args,
        extensions: plugin.languages.extensions,
        fingerprint_ignore: plugin.fingerprint.ignore,
        manifest_dir: dir.to_path_buf(),
        capabilities: plugin.capabilities,
        workspace: WorkspaceConfig {
            watch_files,
            exclude_dirs: plugin.workspace.exclude_dirs,
            entry_points: plugin.workspace.entry_points,
        },
    })
}

/// Discovery's output: every plugin found, keyed by language, plus the
/// extension routing table derived from them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredPlugins {
    /// language -> manifest.
    pub manifests: HashMap<String, PluginManifest>,
    /// lowercase, leading-dot extension -> language.
    pub routing: HashMap<String, String>,
}

impl DiscoveredPlugins {
    /// Which language claims `file_path` by its extension alone, lowercased
    /// first (the routing table's keys are lowercase); `None` if none does.
    pub fn language_for(&self, file_path: &str) -> Option<&str> {
        let extension = extension_of(file_path)?;
        self.routing.get(&extension).map(String::as_str)
    }

    /// The language whose plugin indexes `file_path`: the one claiming its
    /// extension, unless the file is under one of that same language's
    /// `exclude_dirs`. Exclusions are per language, never a union. The single
    /// answer to "should the index hold this file?", shared by the watcher's
    /// routing and `g-mesh status`'s coverage walk so they cannot disagree.
    pub fn indexing_language(&self, file_path: &str) -> Option<&str> {
        let language = self.language_for(file_path)?;
        match self.manifests.get(language) {
            Some(manifest) if under_excluded_dir(file_path, &manifest.workspace.exclude_dirs) => None,
            _ => Some(language),
        }
    }
}

/// `src/App.TSX` -> `".tsx"`: the lowercase, leading-dot form the routing
/// table is keyed by. `None` for a path with no (UTF-8) extension.
pub(crate) fn extension_of(file_path: &str) -> Option<String> {
    let extension = Path::new(file_path).extension()?.to_str()?;
    Some(format!(".{}", extension.to_lowercase()))
}

/// Whether `file_path` sits under a directory named one of `exclude_dirs`,
/// checked against every path segment except the file name, at any depth.
/// Whole-segment match only: `exclude_dirs = ["vendor"]` excludes
/// `pkg/vendor/x` but not `vendored-tools/x`.
pub(crate) fn under_excluded_dir(file_path: &str, exclude_dirs: &[String]) -> bool {
    if exclude_dirs.is_empty() {
        return false;
    }
    let mut segments = file_path.split('/');
    segments.next_back(); // the file name itself names no directory
    segments.any(|segment| exclude_dirs.iter().any(|excluded| excluded == segment))
}

/// Every language among `manifests` whose `capabilities.semantic_pass` is
/// `true`. The one filter behind both `PluginRegistry::semantic_pass_languages`
/// and `daemon::semantic::run_once`. Sorted for determinism: the passes run
/// sequentially, and `HashMap` order would vary which runs first.
pub fn semantic_pass_capable_languages(manifests: &HashMap<String, PluginManifest>) -> Vec<String> {
    let mut languages: Vec<String> = manifests
        .values()
        .filter(|manifest| manifest.capabilities.semantic_pass)
        .map(|manifest| manifest.language.clone())
        .collect();
    languages.sort();
    languages
}

/// Scans `roots` in order for `<root>/<language-dir>/plugin.toml` and builds
/// the extension routing table. A language in an earlier root shadows the same
/// language in a later one (logged, not an error: the user-override path). A
/// missing or empty root contributes nothing. Two different languages
/// claiming one extension is a hard error naming both manifests.
pub fn discover(roots: &[PathBuf]) -> Result<DiscoveredPlugins> {
    let mut manifests: HashMap<String, PluginManifest> = HashMap::new();

    for root in roots {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            // A missing or unreadable root contributes nothing.
            Err(_) => continue,
        };

        for entry in entries {
            let entry =
                entry.with_context(|| format!("failed to read plugin discovery root {}", root.display()))?;
            let dir = entry.path();
            if !dir.is_dir() || !dir.join(MANIFEST_FILE_NAME).is_file() {
                continue;
            }

            let manifest = read_manifest(&dir)?;

            if let Some(existing) = manifests.get(&manifest.language) {
                eprintln!(
                    "g-mesh daemon: plugin \"{}\" at {} shadows the same language already \
                     found at {} - the earlier one wins",
                    manifest.language,
                    manifest.manifest_dir.display(),
                    existing.manifest_dir.display(),
                );
                continue;
            }

            manifests.insert(manifest.language.clone(), manifest);
        }
    }

    let mut routing: HashMap<String, String> = HashMap::new();
    for manifest in manifests.values() {
        for extension in &manifest.extensions {
            match routing.get(extension) {
                Some(existing_language) if existing_language != &manifest.language => {
                    let existing = &manifests[existing_language];
                    bail!(
                        "extension \"{extension}\" is claimed by both plugin \"{}\" ({}) and \
                         plugin \"{}\" ({}) - two different languages cannot claim the same \
                         file extension",
                        existing_language,
                        existing.manifest_dir.join(MANIFEST_FILE_NAME).display(),
                        manifest.language,
                        manifest.manifest_dir.join(MANIFEST_FILE_NAME).display(),
                    );
                }
                Some(_) => {} // same language already claims it; nothing to do
                None => {
                    routing.insert(extension.clone(), manifest.language.clone());
                }
            }
        }
    }

    Ok(DiscoveredPlugins { manifests, routing })
}

/// Whether `value` contains a platform path separator: the line between "look
/// up on `$PATH`" and "resolve against the manifest's directory".
fn has_path_separator(value: &str) -> bool {
    value.chars().any(std::path::is_separator)
}

/// A bare command name stays as is; anything with a path separator is joined
/// against `dir` and gets the [`resolve_exe_suffix`] fallback.
fn resolve_path_entry(value: &str, dir: &Path) -> PathBuf {
    if has_path_separator(value) {
        resolve_exe_suffix(dir.join(value), std::env::consts::EXE_SUFFIX)
    } else {
        PathBuf::from(value)
    }
}

/// Resolves `command`: a leading [`BIN_DIR_PLACEHOLDER`] becomes `bin_dir`,
/// anything else goes through [`resolve_path_entry`]. Errors when `bin_dir` is
/// unknown or the placeholder is anywhere but at the start.
fn resolve_command(value: &str, dir: &Path, bin_dir: Option<&Path>) -> Result<PathBuf> {
    let Some(rest) = value.strip_prefix(BIN_DIR_PLACEHOLDER) else {
        if value.contains(BIN_DIR_PLACEHOLDER) {
            bail!("`{BIN_DIR_PLACEHOLDER}` is only expanded at the start of `command`, got \"{value}\"");
        }
        return Ok(resolve_path_entry(value, dir));
    };
    let bin_dir = bin_dir.with_context(|| {
        format!("`command` \"{value}\" uses `{BIN_DIR_PLACEHOLDER}`, but the running executable's directory is unknown")
    })?;
    let rest = rest.trim_start_matches(['/', '\\']);
    if rest.is_empty() || rest.contains(BIN_DIR_PLACEHOLDER) {
        bail!("`command` \"{value}\" must be `{BIN_DIR_PLACEHOLDER}/<binary>`");
    }
    Ok(resolve_exe_suffix(bin_dir.join(rest), std::env::consts::EXE_SUFFIX))
}

/// Falls back to the platform's suffixed spelling (`.exe` on Windows) for a
/// `command` path that does not exist as written. Switches only once the
/// suffixed file is confirmed to exist; otherwise the path stays as the
/// manifest wrote it. `suffix` is a parameter so tests cover Windows anywhere.
fn resolve_exe_suffix(resolved: PathBuf, suffix: &str) -> PathBuf {
    if resolved.is_file() {
        return resolved;
    }
    match exe_suffixed(&resolved, suffix) {
        Some(suffixed) if suffixed.is_file() => suffixed,
        _ => resolved,
    }
}

/// Appends `suffix` to `path`'s file name, unless `path` already has an
/// extension or `suffix` is empty (a no-op off Windows). Pure; the one
/// definition of the suffixed spelling, shared with
/// `daemon::plugin::missing_workspace_binary_hint`.
pub(crate) fn exe_suffixed(path: &Path, suffix: &str) -> Option<PathBuf> {
    if suffix.is_empty() || path.extension().is_some() {
        return None;
    }
    let mut name = path.file_name()?.to_os_string();
    name.push(suffix);
    Some(path.with_file_name(name))
}

/// Windows' extended-length prefix on a canonicalized path, written back the
/// ordinary way: `\\?\C:\x` as `C:\x`, `\\?\UNC\srv\share` as `\\srv\share`.
/// `None` when no rewrite applies (not extended-length, or no ordinary
/// spelling such as `\\?\pipe\...`) or the result would exceed 259 UTF-16
/// code units (`MAX_PATH` without its NUL). No `#[cfg(windows)]`, so the
/// Windows arm is testable on any host; a Unix path never matches.
pub(crate) fn plain_win32_path(path: &str) -> Option<String> {
    const VERBATIM: &str = r"\\?\";
    const VERBATIM_UNC: &str = r"\\?\UNC\";
    const MAX_PATH_WITHOUT_NUL: usize = 259;

    let plain = if let Some(share) = path.strip_prefix(VERBATIM_UNC) {
        format!(r"\\{share}")
    } else {
        let disk = path.strip_prefix(VERBATIM)?;
        let mut chars = disk.chars();
        let is_disk = matches!(chars.next(), Some(letter) if letter.is_ascii_alphabetic())
            && chars.next() == Some(':')
            && matches!(chars.next(), None | Some('\\'));
        if !is_disk {
            return None;
        }
        disk.to_string()
    };
    (plain.encode_utf16().count() <= MAX_PATH_WITHOUT_NUL).then_some(plain)
}

/// [`plain_win32_path`] over an owned path, left as it came when no rewrite
/// applies (including a path that is not valid UTF-8).
pub(crate) fn plain_spelling(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(plain_win32_path) {
        Some(plain) => PathBuf::from(plain),
        None => path,
    }
}

/// Resolves one `args` entry like [`resolve_path_entry`], back into a
/// `String`. An entry with no path separator (a flag, a bare word) is not a
/// path and must not go through `PathBuf`.
fn resolve_arg(value: &str, dir: &Path) -> String {
    if has_path_separator(value) {
        dir.join(value).to_string_lossy().into_owned()
    } else {
        value.to_string()
    }
}

// Raw (on-disk) shape.

#[derive(Debug, Deserialize)]
struct RawManifest {
    plugin: RawPlugin,
}

#[derive(Debug, Deserialize)]
struct RawPlugin {
    language: String,
    protocol_version: u32,
    plugin_version: String,
    spawn: RawSpawn,
    languages: RawLanguages,
    /// Optional: absent means the built-in baseline ignore list only.
    #[serde(default)]
    fingerprint: RawFingerprint,
    /// Optional, as is every field inside it (`Capabilities` is
    /// `#[serde(default)]`), so a partial table fills the rest conservatively.
    #[serde(default)]
    capabilities: Capabilities,
    /// `[plugin.workspace]` is optional - see [`RawWorkspace`].
    #[serde(default)]
    workspace: RawWorkspace,
}

#[derive(Debug, Deserialize)]
struct RawSpawn {
    command: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawLanguages {
    extensions: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFingerprint {
    #[serde(default)]
    ignore: Vec<String>,
}

/// `[plugin.workspace]`'s on-disk shape: plain strings, which
/// [`read_manifest`] compiles into globs after parsing.
#[derive(Debug, Default, Deserialize)]
struct RawWorkspace {
    #[serde(default)]
    watch_files: Vec<String>,
    #[serde(default)]
    exclude_dirs: Vec<String>,
    #[serde(default)]
    entry_points: Vec<String>,
}

#[cfg(test)]
mod tests;
