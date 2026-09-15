//! Reads and validates one plugin's `plugin.toml` manifest.
//!
//! See `docs/architecture/plugin-modularity.md`'s Data Model and Interfaces
//! sections - this module implements exactly the schema and validation rules
//! documented there. TOML, not JSON, to match the house style
//! `core/src/config` already established (this is local config-shaped data,
//! read once at discovery time, never sent over the wire protocol).
//!
//! # Validation is a hard failure, not best-effort
//!
//! A manifest is read once, at daemon startup (discovery, not built yet -
//! see the architecture doc's `discover()`), and a bad one means the plugin
//! it describes cannot possibly be spawned correctly. Following
//! `protocol::handshake::verify`'s philosophy ("a protocol is code, not
//! data - a mismatch is a hard load failure with a clear, actionable
//! error"), every check here bails with an error naming both the manifest
//! path and the specific problem, rather than returning a partial or
//! best-guess manifest.
//!
//! # Command/args resolution
//!
//! A manifest's `command` and any `args` entry that looks like a relative
//! path are resolved eagerly, here, rather than left as raw strings for a
//! spawner to reinterpret later - see the architecture doc's Data Model
//! section for the schema comment this mirrors. A bare command (no path
//! separator, e.g. `"node"`) is left alone so `std::process::Command`
//! resolves it on `$PATH` at spawn time, exactly like a shell would; anything
//! with a path separator is joined against the manifest's own directory, so
//! a plugin can ship a relative path to its own entry point (`"./g-mesh-
//! plugin-python"`, `"dist/src/index.js"`) without knowing where it was
//! installed. `Path::join` already does the right thing for an
//! already-absolute value (it replaces the base entirely), so no separate
//! "is it absolute" branch is needed.
//!
//! # Capabilities and workspace
//!
//! `[plugin.capabilities]` and `[plugin.workspace]` (see
//! `docs/architecture/multi-language-plugins.md`'s "`plugin.toml`
//! additions" section) are read and validated exactly like everything else
//! in this file - same hard-failure rule, same "a manifest is read once at
//! startup" reasoning - but this module only parses and exposes them.
//! Nothing here schedules a semantic pass, routes a watched file, or picks
//! an entry point; those are later consumers reading [`PluginManifest::capabilities`]
//! and [`PluginManifest::workspace`], not this one. Both sections are
//! optional: a manifest that omits one entirely gets the conservative
//! default documented on [`Capabilities::default`] and
//! [`WorkspaceConfig::default`] respectively - "says nothing" is treated as
//! "can do the least", not "can do the most", so an old or hand-written
//! manifest predating this task does not silently opt into a semantic pass
//! or a resolved receiver-call claim it was never written to back up.
//!
//! `watch_files` entries are glob patterns (`globset::Glob`), compiled and
//! validated here rather than left as raw strings - see this file's
//! `Cargo.toml` comment for why `globset` and not something else. An exact
//! file name (`"go.mod"`) is already a valid glob that matches only itself,
//! so there is no separate "exact name" code path to keep in sync with the
//! glob one; the architecture doc's paper stress test is what forced globs
//! onto this field at all (`*.csproj`, `*.sln` - C#'s project files have no
//! fixed name).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use globset::Glob;
use serde::Deserialize;

use crate::protocol::types::CURRENT_PROTOCOL_VERSION;

const MANIFEST_FILE_NAME: &str = "plugin.toml";

/// Overrides [`default_roots`]'s entire return value with a single directory.
/// Generalizes `daemon::plugin::PLUGIN_PATH_ENV` (the JS/TS-only, test-only
/// entry-point override that predates this module): a test drops whatever
/// `<language>/plugin.toml` fixtures it needs under one directory and points
/// discovery at just that, rather than needing one override variable per
/// language. Real installs never set this - the default already resolves to
/// the user and bundled roots.
pub const PLUGIN_ROOTS_OVERRIDE_ENV: &str = "G_MESH_PLUGIN_ROOTS_OVERRIDE";

/// The roots [`discover`] scans, in precedence order, honoring
/// [`PLUGIN_ROOTS_OVERRIDE_ENV`].
///
/// With no override set, returns the user root followed by [`bundled_roots`]:
/// 1. `~/.g-mesh/plugins/` - user-installed, global (not per-project),
///    resolved the same way `config::global_config_path` resolves everything
///    else under `~/.g-mesh/...`. Skipped (not an error - matches this
///    module's "a root that does not exist contributes nothing" philosophy
///    in [`discover`]) if the home directory cannot be resolved at all.
/// 2. The bundled roots - see [`bundled_roots`] for why there are two of them
///    and why listing both costs nothing.
///
/// With [`PLUGIN_ROOTS_OVERRIDE_ENV`] set, every standard root is replaced
/// entirely by a single-element vec of just that one path.
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

/// Where the plugins that ship *with* g-mesh live, installed root first.
///
/// The two entries are the two shapes this project exists in, and exactly one
/// of them is real on any given machine - which is why both can be listed
/// unconditionally rather than switched between:
///
/// 1. An **installed** layout: `plugins/` beside the core executable, which is
///    what a release archive unpacks to (`scripts/build-targets.sh`). In a
///    checkout this points inside `core/target/<profile>/`, where nothing
///    creates a `plugins/` directory, so it contributes nothing.
/// 2. A **checkout** layout: `CARGO_MANIFEST_DIR/../plugins/`, baked in at
///    compile time, since `core/` and `plugins/` are sibling directories here.
///    An installed binary's compile-time path is a directory on the build
///    machine that does not exist on the user's, so it too contributes
///    nothing - a root that does not exist is skipped by [`discover`].
///
/// Installed first so that if a release archive is ever unpacked *inside* a
/// checkout, the plugin that shipped with the binary wins over whatever happens
/// to be built in the tree around it - the same "closest to the artifact that is
/// actually running" rule that makes `~/.g-mesh/plugins/` outrank both.
///
/// [`std::env::current_exe`] failing is not an error here: it means the process
/// cannot locate itself, and the checkout root below is still worth trying.
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
/// tiers `[plugin.capabilities]` asks about separately - see
/// [`Capabilities::receiver_calls`] and
/// [`Capabilities::receiver_calls_structural`]'s own doc comments, and the
/// architecture doc's `plugin.toml additions` section, for what "resolved"
/// means at each tier. A plain `bool` was rejected on purpose: at the call
/// site (the MCP instruction assembler, a later task) `resolved`/`unresolved`
/// reads as what it is, where `true`/`false` would leave a reader re-deriving
/// which boolean state means which English word every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReceiverCallResolution {
    Resolved,
    Unresolved,
}

impl std::fmt::Display for ReceiverCallResolution {
    /// The same lowercase word the manifest itself uses (`"resolved"` /
    /// `"unresolved"`) - used by `cli::plugins`' `render` to show
    /// capabilities without a second mapping to keep in sync with
    /// [`ReceiverCallResolution`]'s `Deserialize` impl above.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReceiverCallResolution::Resolved => "resolved",
            ReceiverCallResolution::Unresolved => "unresolved",
        })
    }
}

impl Default for ReceiverCallResolution {
    /// The conservative default this field takes wherever it is missing -
    /// an absent `[plugin.capabilities]` table (via [`Capabilities::default`])
    /// or, once semantic scheduling exists, a plugin that never claims a tier
    /// resolves receiver calls. Assuming "unresolved" costs an honest gap in
    /// the MCP instructions; assuming "resolved" would silently drop edges a
    /// caller was told to expect.
    fn default() -> Self {
        ReceiverCallResolution::Unresolved
    }
}

/// What core is allowed to ask this plugin to do, and how far each tier's
/// receiver-call resolution can be trusted - see this module's doc comment
/// and the architecture doc's `plugin.toml additions` section for what each
/// field gates. Read from the manifest, not the handshake: routing and MCP
/// instruction assembly need these before any plugin process exists, and the
/// manifest is already the startup-time source of truth (same rationale the
/// architecture doc gives for this choice).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    /// Whether core may send this plugin a `semanticPass` request - per file
    /// after a reparse, whole project after a walk. `false` (the default)
    /// means core never sends one and never requires an (empty-diff) answer
    /// to one; there is no partial-semantic-pass state to represent.
    pub semantic_pass: bool,
    /// Whether receiver calls resolve to edges once this plugin's best
    /// available tier has run (its semantic tier, if it has one and it has
    /// run - otherwise the structural tier alone). `Resolved` means the MCP
    /// instructions do not need to list the receiver-call gap for this
    /// language; `Unresolved` (the default) means they do.
    pub receiver_calls: ReceiverCallResolution,
    /// Whether the *structural* tier alone - before any semantic pass runs,
    /// or for a plugin with no semantic tier at all - resolves receiver
    /// calls. Separate from `receiver_calls` because a plugin can be
    /// `Unresolved` here and `Resolved` there: resolved once
    /// `semanticPassAt` is set for the language, per the architecture doc's
    /// comment on this exact pair of fields.
    pub receiver_calls_structural: ReceiverCallResolution,
}

/// Which files and directories route to this plugin outside of its claimed
/// extensions, and which names a miss-path lookup treats as a container's
/// entry point - see this module's doc comment and the architecture doc's
/// `plugin.toml additions` section for what each field gates. A manifest
/// with no `[plugin.workspace]` table at all defaults to all three fields
/// empty: nothing watched beyond extension-based routing, nothing excluded,
/// no entry points - the same "says nothing, assumed to do the least" rule
/// [`Capabilities::default`] follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceConfig {
    /// Exact file names (any directory) or glob patterns (`*.csproj`),
    /// already compiled and validated by [`read_manifest`] - see this
    /// module's doc comment for why an exact name needs no separate
    /// representation from a glob. A change to a matching file triggers a
    /// per-language reindex (module paths / crate roots may have moved) -
    /// the watcher-routing consumer of this field, not built by this task.
    pub watch_files: Vec<Glob>,
    /// Directory names this plugin's own walk never descends into, and the
    /// watcher should never route to it either - matched by exact name, not
    /// glob, mirroring `fingerprint_ignore`'s shape rather than
    /// `watch_files`'s.
    pub exclude_dirs: Vec<String>,
    /// File or directory names a miss-path lookup treats as this container's
    /// entry point (e.g. rust: `lib.rs`, `main.rs`, `mod.rs`; typescript:
    /// `index`). Exact names, not globs.
    pub entry_points: Vec<String>,
}

/// One plugin directory's fully-resolved manifest - see this module's doc
/// comment and the architecture doc's Interfaces section for what each field
/// means and how `command`/`args` got resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    /// The plugin's wire identifier (`Handshake.language`). Always equal to
    /// `manifest_dir`'s final path component - [`read_manifest`] enforces
    /// that at read time, so nothing downstream has to re-check it.
    pub language: String,
    pub protocol_version: u32,
    pub plugin_version: String,
    /// Resolved argv\[0\]: a bare command name (looked up on `$PATH` at spawn
    /// time) or an absolute path joined against `manifest_dir`.
    pub command: PathBuf,
    /// Resolved extra argv entries, in declared order.
    pub args: Vec<String>,
    /// Lowercase, leading-dot file extensions this plugin claims (e.g.
    /// `".py"`) - not validated for case or leading dot by this module; that
    /// is an authoring convention documented in the architecture doc, not a
    /// parse-time hard failure.
    pub extensions: Vec<String>,
    /// Directory names to skip, on top of the built-in baseline ignore list,
    /// when fingerprinting this plugin's own files. Empty when
    /// `[plugin.fingerprint]` is absent entirely.
    pub fingerprint_ignore: Vec<String>,
    /// This plugin's own directory - the one `plugin.toml` was read from.
    /// Kept on the resolved struct for error messages and fingerprinting
    /// (walking every file under it), not just as a read-time detail.
    pub manifest_dir: PathBuf,
    /// Parsed `[plugin.capabilities]`, or [`Capabilities::default`] if the
    /// table is absent - see this module's doc comment.
    pub capabilities: Capabilities,
    /// Parsed `[plugin.workspace]`, or [`WorkspaceConfig::default`] if the
    /// table is absent - see this module's doc comment.
    pub workspace: WorkspaceConfig,
}

/// Reads and validates `<dir>/plugin.toml`.
///
/// Hard error on: malformed TOML, a missing required field, `language` not
/// equal to `dir`'s final path component, an unrecognized
/// `protocol_version`, an unrecognized `[plugin.capabilities]
/// receiver_calls`/`receiver_calls_structural` value (caught by TOML parsing
/// itself - see [`ReceiverCallResolution`]'s `Deserialize` impl - so it
/// fails alongside any other malformed-TOML error, with the manifest path
/// named the same way), or an invalid `[plugin.workspace] watch_files` glob
/// pattern. Every error names the manifest path; the two semantic checks on
/// `language`/`protocol_version` additionally name both the declared and
/// expected value, matching `protocol::handshake::verify`'s error style.
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

    let command = resolve_path_entry(&plugin.spawn.command, dir);
    let args = plugin.spawn.args.iter().map(|arg| resolve_arg(arg, dir)).collect();

    // `watch_files` entries parse as plain strings (`RawWorkspace`) rather
    // than straight into `Glob` - unlike `receiver_calls` above, a bad glob
    // is not a shape TOML itself can reject (any string is syntactically
    // valid TOML), so it needs its own validation pass, here, with its own
    // error naming the manifest path, the section, and the bad pattern
    // itself - the same "validation is a hard failure, not best-effort" rule
    // as `language`/`protocol_version` above, just for a check TOML parsing
    // cannot do on its behalf.
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
/// extension routing table derived from them - see the architecture doc's
/// Interfaces section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscoveredPlugins {
    /// language -> manifest.
    pub manifests: HashMap<String, PluginManifest>,
    /// lowercase, leading-dot extension -> language.
    pub routing: HashMap<String, String>,
}

/// Scans `roots` in order for `<root>/<language-dir>/plugin.toml`, calling
/// [`read_manifest`] on each one found, then builds the extension routing
/// table from the results.
///
/// A language found in an earlier root shadows the same language name found
/// in a later root - logged, not an error (the deliberate override path: a
/// higher-precedence root, e.g. `~/.g-mesh/plugins/`, is scanned before the
/// bundled root, so a user-installed plugin can intentionally replace a
/// bundled one). A root that does not exist, or exists but is empty,
/// contributes nothing - also not an error.
///
/// Two *different* languages claiming the same file extension while
/// building the routing table is a hard error naming both languages and
/// both manifest paths, matching this module's "validation is a hard
/// failure" philosophy (see the module doc comment).
pub fn discover(roots: &[PathBuf]) -> Result<DiscoveredPlugins> {
    let mut manifests: HashMap<String, PluginManifest> = HashMap::new();

    for root in roots {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            // A missing (or otherwise unreadable) root contributes nothing -
            // not an error; see this function's doc comment.
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

/// `true` if `value` contains a platform path separator - the signal this
/// module uses (mirroring the architecture doc's schema comment) to decide
/// between "look this up on `$PATH`" and "resolve this relative to the
/// manifest's own directory".
fn has_path_separator(value: &str) -> bool {
    value.chars().any(std::path::is_separator)
}

/// Resolves `command` per this module's doc comment: unchanged if it looks
/// like a bare command name, else joined against `dir`.
fn resolve_path_entry(value: &str, dir: &Path) -> PathBuf {
    if has_path_separator(value) {
        dir.join(value)
    } else {
        PathBuf::from(value)
    }
}

/// Resolves one `args` entry the same way as [`resolve_path_entry`], but
/// back into a `String` - `args` crosses into `Command::args` as strings,
/// and an entry with no path separator (a flag, a bare word) is not a path
/// at all and must not be forced through `PathBuf`'s platform formatting.
fn resolve_arg(value: &str, dir: &Path) -> String {
    if has_path_separator(value) {
        dir.join(value).to_string_lossy().into_owned()
    } else {
        value.to_string()
    }
}

// ---------------------------------------------------------------------------
// Raw (on-disk) shape
// ---------------------------------------------------------------------------

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
    /// `[plugin.fingerprint]` is optional - a manifest that never mentions
    /// it fingerprints with the built-in baseline ignore list only (see the
    /// architecture doc's Data Model section).
    #[serde(default)]
    fingerprint: RawFingerprint,
    /// `[plugin.capabilities]` is optional, and so is every field inside it -
    /// `Capabilities` itself carries `#[serde(default)]`, so a table present
    /// but missing one field (e.g. `semantic_pass` alone, no
    /// `receiver_calls`) still fills the rest conservatively rather than
    /// erroring. No separate raw type needed: nothing in this section needs
    /// resolving beyond what `toml`'s `Deserialize` already does (unlike
    /// `workspace.watch_files` below, which needs glob compilation
    /// `read_manifest` does by hand).
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

/// `[plugin.workspace]`'s on-disk shape - plain strings, unlike
/// [`WorkspaceConfig`]'s `watch_files: Vec<Glob>`, because a glob pattern's
/// validity cannot be checked by `toml`'s `Deserialize` alone (any string is
/// syntactically valid TOML); [`read_manifest`] compiles and validates each
/// entry by hand after parsing, the same "raw strings in, resolved values
/// out" shape `RawSpawn`'s `command`/`args` already use for a different
/// reason (path resolution instead of glob compilation).
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
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Guards every test below that touches [`PLUGIN_ROOTS_OVERRIDE_ENV`]: it
    /// is process-wide state, and `cargo test` runs this module's tests on
    /// multiple threads by default, so two of them setting/clearing the same
    /// variable at once would be a genuine race - same reasoning as
    /// `daemon::lifecycle`'s `ENV_LOCK` for its own env-var tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// The bundled JS/TS plugin's source directory, reached the same way
    /// [`bundled_roots`] reaches it.
    fn bundled_typescript_plugin_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript")
    }

    fn json_at(path: &Path) -> serde_json::Value {
        let contents =
            fs::read_to_string(path).unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        serde_json::from_str(&contents)
            .unwrap_or_else(|err| panic!("failed to parse {} as JSON: {err}", path.display()))
    }

    /// The bundled plugin states its version in three files, and nothing but
    /// this test makes them agree.
    ///
    /// The one that matters at runtime is `plugin.toml`: `read_manifest`
    /// parses it and `g-mesh plugins list` prints it, so it is the number a
    /// user sees when diagnosing. `package.json` is what the npm side uses,
    /// and the lock file records the same version twice more. They have
    /// already drifted once - the 2.1.0 bump left the lock naming 2.0.0, and
    /// nothing failed, because `npm ci` checks dependency sync rather than
    /// the root package's own version field. A wrong number reported to the
    /// only person who ever looks is the cost, so it is worth one test.
    ///
    /// Reading the real files rather than a fixture is the point: a fixture
    /// would prove the comparison works, not that these four declarations do.
    #[test]
    fn every_declaration_of_the_bundled_plugins_version_agrees() {
        let dir = bundled_typescript_plugin_dir();
        let manifest = read_manifest(&dir).expect("failed to read the bundled plugin's manifest");
        let package = json_at(&dir.join("package.json"));
        let lock = json_at(&dir.join("package-lock.json"));

        let declared = [
            ("plugin.toml [plugin] plugin_version", manifest.plugin_version.clone()),
            ("package.json .version", string_at(&package, &["version"])),
            ("package-lock.json .version", string_at(&lock, &["version"])),
            ("package-lock.json .packages[\"\"].version", string_at(&lock, &["packages", "", "version"])),
        ];

        let (first_source, first_version) = &declared[0];
        for (source, version) in &declared[1..] {
            assert_eq!(
                version, first_version,
                "the bundled plugin's version has drifted: {first_source} says {first_version}, \
                 {source} says {version}. Bump package.json and plugin.toml together, and \
                 regenerate the lock with `npm install --package-lock-only`."
            );
        }
    }

    /// The version that actually leaves the plugin at runtime, reported by
    /// `sendHandshake` in `plugins/typescript/src/index.ts`.
    ///
    /// No longer a declaration of its own - `scripts/generate-version.js`
    /// writes it from `package.json` on every build - so this test now asks
    /// the question that survives that: whether the manifest core reads
    /// *without* running a plugin agrees with what the running plugin says.
    /// Those two can still drift, because `plugin.toml` is read by
    /// `g-mesh plugins list` on plugins that were never built and so cannot
    /// be derived from anything at runtime.
    ///
    /// Core prints it when `handshake::verify` refuses a protocol mismatch -
    /// "protocol version mismatch with typescript plugin (plugin version X)" -
    /// while `g-mesh plugins list` prints the manifest's copy. The two had
    /// drifted: the manifest said 2.1.0 and the wire said 0.1.0, so the two
    /// screens a person consults about one plugin named different versions,
    /// and the one shown at the worst possible moment was three releases
    /// stale.
    ///
    /// Checked by spawning the real plugin rather than by reading its source:
    /// what matters is the value that arrives on core's stdin. A source-text
    /// check would pass just as happily on a build that was never rerun after
    /// the version changed.
    #[test]
    fn the_bundled_plugins_handshake_reports_the_version_its_manifest_declares() {
        let dir = bundled_typescript_plugin_dir();
        let manifest = read_manifest(&dir).expect("failed to read the bundled plugin's manifest");

        let mut plugin = std::process::Command::new(&manifest.command)
            .args(&manifest.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("failed to spawn the bundled plugin - is it built? (`npm run build`)");
        let mut stdout = std::io::BufReader::new(plugin.stdout.take().expect("the plugin has no stdout"));
        let handshake = crate::protocol::handshake::perform(&mut stdout);
        drop(plugin.stdin.take());
        let _ = plugin.kill();
        let _ = plugin.wait();

        let handshake = handshake.expect("the bundled plugin did not complete a handshake");
        assert_eq!(
            handshake.plugin_version, manifest.plugin_version,
            "the bundled plugin's handshake reports {}, but its plugin.toml declares {} - \
             the handshake follows package.json (via scripts/generate-version.js), so either \
             plugin.toml is stale or the plugin needs rebuilding",
            handshake.plugin_version, manifest.plugin_version
        );
    }

    /// The string at a path of keys, so a missing or retyped field fails as
    /// "this file no longer declares a version there" rather than as a
    /// comparison against `null`.
    fn string_at(value: &serde_json::Value, keys: &[&str]) -> String {
        let mut current = value;
        for key in keys {
            current = current
                .get(key)
                .unwrap_or_else(|| panic!("no `{}` in the JSON being checked", keys.join(".")));
        }
        current.as_str().unwrap_or_else(|| panic!("`{}` is not a string", keys.join("."))).to_string()
    }

    #[test]
    fn default_roots_bundled_entry_resolves_to_the_sibling_plugins_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PLUGIN_ROOTS_OVERRIDE_ENV);

        let roots = default_roots();
        let bundled = roots.last().expect("default_roots must return at least the bundled root");

        assert!(
            bundled.join("typescript").join(MANIFEST_FILE_NAME).is_file(),
            "expected {} to contain typescript/plugin.toml",
            bundled.display()
        );
    }

    /// The half of the answer a release archive needs: an unpacked install has
    /// no `CARGO_MANIFEST_DIR` to resolve, so discovery has to be able to find
    /// `plugins/` beside the binary that is running. Ordered ahead of the
    /// checkout root - see [`bundled_roots`].
    #[test]
    fn the_installed_root_sits_beside_the_executable_and_outranks_the_checkout_root() {
        let roots = bundled_roots();

        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(
            roots.first(),
            Some(&exe_dir.join("plugins")),
            "the installed root must be `plugins/` next to the running executable"
        );
        assert_eq!(roots.len(), 2, "installed and checkout roots, in that order");
    }

    #[test]
    fn the_override_env_var_replaces_the_entire_default_roots_list() {
        let _guard = ENV_LOCK.lock().unwrap();
        let override_dir = tempfile::tempdir().unwrap();
        std::env::set_var(PLUGIN_ROOTS_OVERRIDE_ENV, override_dir.path());

        let roots = default_roots();

        std::env::remove_var(PLUGIN_ROOTS_OVERRIDE_ENV);

        assert_eq!(roots, vec![override_dir.path().to_path_buf()]);
    }

    /// Writes `plugin.toml` under a directory named `dir_name` (inside a
    /// fresh tempdir) with `body` as its contents, and returns that
    /// directory - the shape every test here needs, since `language` must
    /// equal the directory's own name to parse successfully.
    fn plugin_dir(dir_name: &str, body: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let plugin_dir = root.path().join(dir_name);
        fs::create_dir_all(&plugin_dir).unwrap();
        fs::write(plugin_dir.join(MANIFEST_FILE_NAME), body).unwrap();
        (root, plugin_dir)
    }

    /// Builds a fresh tempdir root containing one `<dir_name>/plugin.toml`
    /// per entry in `plugins` - the multi-language, single-root analog of
    /// [`plugin_dir`], for `discover()` tests that need more than one
    /// language directory under a root. Returns the root path itself
    /// (unlike `plugin_dir`, which returns a plugin subdirectory).
    fn discovery_root(plugins: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (dir_name, body) in plugins {
            let plugin_dir = root.path().join(dir_name);
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(plugin_dir.join(MANIFEST_FILE_NAME), body).unwrap();
        }
        let path = root.path().to_path_buf();
        (root, path)
    }

    /// A well-formed `plugin.toml` body for `discover()` tests, parameterized
    /// over the fields those tests actually vary - `language` (must match
    /// its containing directory name, same as [`well_formed_toml`]),
    /// `plugin_version` (used to tell two same-language manifests apart),
    /// and `extensions` (used to construct routing conflicts).
    fn manifest_toml(language: &str, plugin_version: &str, extensions: &[&str]) -> String {
        let extensions = extensions.iter().map(|ext| format!("\"{ext}\"")).collect::<Vec<_>>().join(", ");
        format!(
            r#"
[plugin]
language = "{language}"
protocol_version = {version}
plugin_version = "{plugin_version}"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [{extensions}]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        )
    }

    #[test]
    fn a_language_in_an_earlier_root_shadows_the_same_language_in_a_later_root() {
        let (_root1, root1) = discovery_root(&[("python", &manifest_toml("python", "1.0.0", &[".py"]))]);
        let (_root2, root2) = discovery_root(&[("python", &manifest_toml("python", "2.0.0", &[".py"]))]);

        let discovered = discover(&[root1, root2]).unwrap();

        assert_eq!(discovered.manifests.len(), 1);
        assert_eq!(discovered.manifests["python"].plugin_version, "1.0.0");
        assert_eq!(discovered.routing.get(".py"), Some(&"python".to_string()));
    }

    #[test]
    fn two_different_languages_claiming_the_same_extension_is_a_hard_error() {
        let (_root, root) = discovery_root(&[
            ("python", &manifest_toml("python", "1.0.0", &[".foo"])),
            ("go", &manifest_toml("go", "1.0.0", &[".foo"])),
        ]);

        let err = discover(std::slice::from_ref(&root)).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("python"), "{message}");
        assert!(message.contains("go"), "{message}");
        assert!(
            message.contains(&root.join("python").join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()),
            "{message}"
        );
        assert!(
            message.contains(&root.join("go").join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()),
            "{message}"
        );
    }

    #[test]
    fn a_root_that_does_not_exist_contributes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let missing_root = root.path().join("does-not-exist");

        let discovered = discover(&[missing_root]).unwrap();

        assert!(discovered.manifests.is_empty());
        assert!(discovered.routing.is_empty());
    }

    #[test]
    fn an_empty_root_directory_contributes_nothing() {
        let root = tempfile::tempdir().unwrap();

        let discovered = discover(&[root.path().to_path_buf()]).unwrap();

        assert!(discovered.manifests.is_empty());
        assert!(discovered.routing.is_empty());
    }

    #[test]
    fn discovery_with_two_languages_and_no_conflicts_populates_manifests_and_routing() {
        let (_root, root) = discovery_root(&[
            ("python", &manifest_toml("python", "1.0.0", &[".py", ".pyi"])),
            ("go", &manifest_toml("go", "1.0.0", &[".go"])),
        ]);

        let discovered = discover(&[root]).unwrap();

        assert_eq!(discovered.manifests.len(), 2);
        assert!(discovered.manifests.contains_key("python"));
        assert!(discovered.manifests.contains_key("go"));
        assert_eq!(discovered.routing.get(".py"), Some(&"python".to_string()));
        assert_eq!(discovered.routing.get(".pyi"), Some(&"python".to_string()));
        assert_eq!(discovered.routing.get(".go"), Some(&"go".to_string()));
    }

    fn well_formed_toml() -> String {
        format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py", ".pyi"]

[plugin.fingerprint]
ignore = ["node_modules"]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        )
    }

    #[test]
    fn parses_a_well_formed_manifest_into_the_expected_struct() {
        let (_root, dir) = plugin_dir("python", &well_formed_toml());

        let manifest = read_manifest(&dir).unwrap();

        assert_eq!(manifest.language, "python");
        assert_eq!(manifest.protocol_version, CURRENT_PROTOCOL_VERSION);
        assert_eq!(manifest.plugin_version, "0.1.0");
        // "node" has no path separator, so it stays a bare command for
        // `$PATH` lookup rather than being joined against `dir`.
        assert_eq!(manifest.command, PathBuf::from("node"));
        // "dist/src/index.js" does have a separator, so it is resolved
        // against the manifest's own directory.
        assert_eq!(manifest.args, vec![dir.join("dist/src/index.js").to_string_lossy().into_owned()]);
        assert_eq!(manifest.extensions, vec![".py".to_string(), ".pyi".to_string()]);
        assert_eq!(manifest.fingerprint_ignore, vec!["node_modules".to_string()]);
        assert_eq!(manifest.manifest_dir, dir);
    }

    #[test]
    fn rejects_malformed_toml_naming_the_manifest_path() {
        let (_root, dir) = plugin_dir("python", "this is not [ valid toml");

        let err = read_manifest(&dir).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
    }

    #[test]
    fn rejects_a_manifest_missing_a_required_field_naming_the_path_and_field() {
        // No `plugin_version` under `[plugin]`.
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}

[plugin.spawn]
command = "node"

[plugin.languages]
extensions = [".py"]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let err = read_manifest(&dir).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
        assert!(message.contains("plugin_version"), "{message}");
    }

    #[test]
    fn rejects_language_not_matching_the_directory_name() {
        let body = well_formed_toml(); // declares language = "python"
        let (_root, dir) = plugin_dir("not-python", &body);

        let err = read_manifest(&dir).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("python"), "{message}");
        assert!(message.contains("not-python"), "{message}");
    }

    #[test]
    fn rejects_an_unrecognized_protocol_version() {
        let bad_version = CURRENT_PROTOCOL_VERSION + 1;
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {bad_version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]
"#,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let err = read_manifest(&dir).unwrap_err();
        let message = err.to_string();
        assert!(message.contains(&bad_version.to_string()), "{message}");
        assert!(message.contains(&CURRENT_PROTOCOL_VERSION.to_string()), "{message}");
    }

    #[test]
    fn a_manifest_with_no_fingerprint_table_defaults_to_an_empty_ignore_list() {
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let manifest = read_manifest(&dir).unwrap();

        assert_eq!(manifest.fingerprint_ignore, Vec::<String>::new());
    }

    /// This task's own acceptance criterion, stated directly: a manifest
    /// that never mentions `[plugin.capabilities]` or `[plugin.workspace]`
    /// at all parses to the conservative defaults documented on
    /// [`Capabilities::default`] and [`WorkspaceConfig::default`] - no
    /// semantic pass, receiver calls unresolved at both tiers, nothing
    /// watched, nothing excluded, no entry points. Same fixture body as
    /// [`a_manifest_with_no_fingerprint_table_defaults_to_an_empty_ignore_list`],
    /// applying the same "an absent optional table is not an error" rule to
    /// the two newer sections.
    #[test]
    fn a_manifest_with_no_capabilities_or_workspace_table_defaults_conservatively() {
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let manifest = read_manifest(&dir).unwrap();

        assert_eq!(manifest.capabilities, Capabilities::default());
        assert!(!manifest.capabilities.semantic_pass);
        assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Unresolved);
        assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);
        assert_eq!(manifest.workspace, WorkspaceConfig::default());
        assert!(manifest.workspace.watch_files.is_empty());
        assert!(manifest.workspace.exclude_dirs.is_empty());
        assert!(manifest.workspace.entry_points.is_empty());
    }

    /// The positive case: every `[plugin.capabilities]` and
    /// `[plugin.workspace]` field set to a non-default value parses into
    /// the expected typed struct - including a genuine glob (`*.csproj`,
    /// straight from the architecture doc's paper-stress-test example)
    /// alongside an exact file name (`go.mod`), proving both live in
    /// `watch_files` the same way (see this module's doc comment).
    #[test]
    fn parses_capabilities_and_workspace_from_a_well_formed_manifest() {
        let body = format!(
            r#"
[plugin]
language = "go"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "./g-mesh-plugin-go"

[plugin.languages]
extensions = [".go"]

[plugin.capabilities]
semantic_pass = true
receiver_calls = "resolved"
receiver_calls_structural = "unresolved"

[plugin.workspace]
watch_files = ["go.mod", "go.work", "*.csproj"]
exclude_dirs = ["vendor", "testdata"]
entry_points = ["lib.rs", "main.rs", "mod.rs"]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("go", &body);

        let manifest = read_manifest(&dir).unwrap();

        assert!(manifest.capabilities.semantic_pass);
        assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Resolved);
        assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);

        let watch_file_patterns: Vec<&str> = manifest.workspace.watch_files.iter().map(Glob::glob).collect();
        assert_eq!(watch_file_patterns, vec!["go.mod", "go.work", "*.csproj"]);
        // An exact name matches only itself; a glob matches the shape the
        // paper stress test needed it for (C#'s project files, which have
        // no fixed name) - both through the same `Glob::compile_matcher`,
        // proving `watch_files` needs no separate "exact name" code path.
        let go_mod = manifest.workspace.watch_files[0].compile_matcher();
        assert!(go_mod.is_match("go.mod"));
        assert!(!go_mod.is_match("other.mod"));
        let csproj = manifest.workspace.watch_files[2].compile_matcher();
        assert!(csproj.is_match("MyProject.csproj"));
        assert!(!csproj.is_match("MyProject.sln"));

        assert_eq!(manifest.workspace.exclude_dirs, vec!["vendor".to_string(), "testdata".to_string()]);
        assert_eq!(
            manifest.workspace.entry_points,
            vec!["lib.rs".to_string(), "main.rs".to_string(), "mod.rs".to_string()]
        );
    }

    /// A `[plugin.capabilities]` table that sets only one field still fills
    /// the rest from [`Capabilities::default`] rather than erroring on the
    /// missing ones or leaving them at some other implicit value - the
    /// container-level `#[serde(default)]` on [`Capabilities`] is what makes
    /// this work, and it is exactly the behavior a plugin author relies on
    /// when they only have something to say about one field.
    #[test]
    fn a_partially_specified_capabilities_table_defaults_the_rest() {
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]

[plugin.capabilities]
semantic_pass = true
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let manifest = read_manifest(&dir).unwrap();

        assert!(manifest.capabilities.semantic_pass);
        assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Unresolved);
        assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);
    }

    /// This task's other acceptance criterion: an unrecognized
    /// `receiver_calls` value is a hard failure naming the manifest path.
    /// Caught by TOML parsing itself (see [`ReceiverCallResolution`]'s
    /// `Deserialize` impl), so this shares its assertion shape with
    /// [`rejects_malformed_toml_naming_the_manifest_path`] rather than with
    /// the hand-written `bail!` checks below it - both are "TOML parsing
    /// failed", just for a different reason.
    #[test]
    fn rejects_an_invalid_receiver_calls_value_naming_the_manifest_path() {
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]

[plugin.capabilities]
receiver_calls = "maybe"
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let err = read_manifest(&dir).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
        assert!(message.contains("maybe"), "{message}");
    }

    /// This task's third acceptance criterion: a `[plugin.workspace]
    /// watch_files` entry that is not a valid glob is a hard failure naming
    /// the manifest path. Unlike the `receiver_calls` case above, TOML
    /// parsing cannot catch this by itself - any string is syntactically
    /// valid TOML - so [`read_manifest`]'s own glob-compilation step is what
    /// has to reject it, matching this module's `bail!`/`.with_context`
    /// error style rather than a `Deserialize` error.
    #[test]
    fn rejects_an_invalid_glob_in_watch_files_naming_the_manifest_path() {
        let body = format!(
            r#"
[plugin]
language = "python"
protocol_version = {version}
plugin_version = "0.1.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".py"]

[plugin.workspace]
watch_files = ["[unclosed"]
"#,
            version = CURRENT_PROTOCOL_VERSION,
        );
        let (_root, dir) = plugin_dir("python", &body);

        let err = read_manifest(&dir).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()), "{message}");
        assert!(message.contains("[unclosed"), "{message}");
    }

    #[test]
    fn a_bare_command_with_no_path_separator_is_left_for_path_lookup() {
        assert_eq!(resolve_path_entry("node", Path::new("/plugins/python")), PathBuf::from("node"));
    }

    #[test]
    fn a_command_with_a_path_separator_is_resolved_against_the_manifest_directory() {
        assert_eq!(
            resolve_path_entry("./g-mesh-plugin-python", Path::new("/plugins/python")),
            Path::new("/plugins/python").join("./g-mesh-plugin-python"),
        );
    }

    #[test]
    fn an_arg_with_no_path_separator_is_left_untouched() {
        assert_eq!(resolve_arg("--verbose", Path::new("/plugins/python")), "--verbose");
    }

    /// The bundled JS/TS plugin's own `plugin.toml` (`plugins/typescript/plugin.toml`,
    /// embedded at compile time so this test tracks the committed file, not a
    /// copy) must itself satisfy `read_manifest`. Its directory on disk used
    /// to be named `js-ts`, not `typescript` - a mismatch this module's own
    /// (now resolved) header used to flag - so this still copies the exact
    /// file contents into a tempdir rather than trusting any particular path,
    /// which is what lets it confirm the content itself is well-formed and
    /// matches the plugin's real handshake/version values independent of
    /// where the repo happens to keep the file.
    #[test]
    fn the_bundled_js_ts_plugin_manifest_parses_once_directory_named_correctly() {
        const BUNDLED_JS_TS_MANIFEST: &str = include_str!("../../../plugins/typescript/plugin.toml");

        let (_root, dir) = plugin_dir("typescript", BUNDLED_JS_TS_MANIFEST);

        let manifest = read_manifest(&dir).unwrap();

        assert_eq!(manifest.language, "typescript");
        assert_eq!(manifest.protocol_version, CURRENT_PROTOCOL_VERSION);
        assert_eq!(manifest.command, PathBuf::from("node"));
        assert_eq!(manifest.args, vec![dir.join("dist/src/index.js").to_string_lossy().into_owned()]);
        assert!(manifest.extensions.contains(&".ts".to_string()));
        assert!(manifest.extensions.contains(&".tsx".to_string()));
        assert!(manifest.extensions.contains(&".js".to_string()));

        // This task's acceptance criterion for the bundled manifest: it
        // carries the capabilities, not just the fields this test already
        // checked before this task.
        assert!(manifest.capabilities.semantic_pass);
        assert_eq!(manifest.capabilities.receiver_calls, ReceiverCallResolution::Unresolved);
        assert_eq!(manifest.capabilities.receiver_calls_structural, ReceiverCallResolution::Unresolved);
        assert_eq!(manifest.workspace.entry_points, vec!["index".to_string()]);
    }

    /// Task 155's actual acceptance criterion for the rename: discovery must
    /// find the bundled plugin at its *real* on-disk location, not just at a
    /// copy under a conveniently-named tempdir - `default_roots()`'s bundled
    /// entry (`CARGO_MANIFEST_DIR/../plugins`) really does contain a
    /// `typescript/plugin.toml` today, where before task 155 it contained
    /// `js-ts/plugin.toml` and could not satisfy `read_manifest`'s
    /// language-equals-directory-name rule at all.
    #[test]
    fn the_real_bundled_plugin_directory_satisfies_read_manifest_directly() {
        let bundled_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");

        let manifest = read_manifest(&bundled_root.join("typescript"))
            .expect("the real bundled plugin directory must satisfy read_manifest");

        assert_eq!(manifest.language, "typescript");
    }
}
