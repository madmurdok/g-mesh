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
//! # `${G_MESH_BIN_DIR}`
//!
//! A `command` that starts with [`BIN_DIR_PLACEHOLDER`] is resolved against
//! the directory of the running g-mesh executable ([`current_bin_dir`])
//! instead of the manifest's directory. `plugins/python/plugin.toml` and
//! `plugins/rust/plugin.toml` use it (GM-404): they name a cargo build output,
//! and before this they hard-coded `../../target/debug/...`, so a g-mesh run
//! from `target/release` still spawned the unoptimized debug plugins. The
//! placeholder makes the plugin's profile follow the core binary's. Only
//! `command` expands it, and only as a prefix; anywhere else it is a hard
//! error rather than a literal directory named `${G_MESH_BIN_DIR}`.
//!
//! `command` gets one more step a plain `args` entry does not: [`resolve_exe_suffix`]
//! falls back to the platform's suffixed spelling (`.exe` on Windows) when the
//! path as joined doesn't exist. `plugins/python/plugin.toml` and
//! `plugins/rust/plugin.toml` name their `command` as a cargo build output
//! (`${G_MESH_BIN_DIR}/g-mesh-plugin-<language>`), and cargo always emits
//! `<name>.exe` there on Windows - never the suffix-less name the manifest
//! (deliberately platform-neutral, per GM-335) actually writes - so without
//! this fallback those two plugins are unspawnable from any Windows checkout,
//! not just in CI. The Go plugin needs no such fallback (`plugins/go/plugin.toml`'s
//! own comment explains why: its build step names the binary explicitly, with
//! no suffix, on every platform) and the suffixed spelling is only ever used
//! once confirmed to exist on disk - see [`resolve_exe_suffix`]'s own doc
//! comment for why a spelling that resolves to nothing either way is left
//! alone rather than guessed at.
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

/// The `command` prefix that stands for [`current_bin_dir`] - see this
/// module's doc comment (GM-404).
pub const BIN_DIR_PLACEHOLDER: &str = "${G_MESH_BIN_DIR}";

/// The directory of the running g-mesh executable, which is what
/// [`BIN_DIR_PLACEHOLDER`] expands to: `target/release` for a release build,
/// `target/debug` for a debug one, and the install directory otherwise.
/// `None` only when this process cannot resolve its own path.
pub fn current_bin_dir() -> Option<PathBuf> {
    bin_dir_of(&std::env::current_exe().ok()?)
}

/// [`current_bin_dir`] for a given executable path. A cargo test binary
/// lives one level deeper, in `target/<profile>/deps/`, while the plugin
/// binaries it would spawn sit in `target/<profile>/`, so a parent named
/// `deps` is stepped over: an in-process test resolves the same profile
/// directory the real binary would.
fn bin_dir_of(exe: &Path) -> Option<PathBuf> {
    let parent = exe.parent()?;
    if parent.file_name().and_then(|name| name.to_str()) == Some("deps") {
        return parent.parent().map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

/// Where a spawned plugin is told to find the manifest core read about it.
///
/// Must match `g_mesh_plugin_sdk::MANIFEST_PATH_ENV`. Spelled out here rather
/// than imported because core deliberately does not depend on the plugin SDK;
/// a divergence shows up as an SDK plugin falling back to its in-code spec,
/// which is exactly the failure this variable exists to prevent.
///
/// # Why core sets it at all
///
/// An SDK plugin re-reads its own `plugin.toml` - for the walk's
/// `exclude_dirs`, and since GM-289 for `[plugin.semantic]`, the language
/// server behind its semantic tier. Left to itself it looks for the manifest
/// *beside its own executable*, which is true of an installed layout and
/// false of a cargo checkout, where the binary is in `target/debug/` and the
/// manifest is in `plugins/<language>/`.
///
/// That mismatch is silent and one-sided: core reads the manifest, sees
/// `capabilities.semantic_pass = true`, and sends a `semanticPass`; the
/// plugin finds no manifest, has no `[plugin.semantic]`, and degrades to
/// structural for the life of the process. Two readers of one file disagreed
/// about where the file was. Telling the plugin the path core actually used
/// removes the question - it is the same thing
/// `g_mesh_plugin_sdk::testing::PluginCheck` already does for the manifest it
/// writes, and for the same stated reason.
pub const MANIFEST_PATH_ENV: &str = "G_MESH_PLUGIN_MANIFEST";

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
///    checkout this points inside `target/<profile>/` (the workspace root's
///    build directory, `core/target/` before GM-284 made the repository a
///    cargo workspace), where nothing creates a `plugins/` directory, so it
///    contributes nothing.
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
    /// run - otherwise the structural tier alone).
    ///
    /// **`Resolved` means resolved against the receiver's declared or
    /// inferred type, never the type it holds at run time**, and GM-385
    /// measured that this is not one plugin's limitation but what the word
    /// can mean at all: a static analysis has no other type to resolve
    /// against. All three bundled plugins that declare `Resolved` behave
    /// identically - `go/types` attributes a call through a `Closer` value
    /// to `Closer.Close`, rust-analyzer attributes `&dyn Shape`/`<S: Shape>`
    /// to `Shape::area`, pyright attributes `obj.describe()` for `obj: Base`
    /// to `Base.describe` - so the override's own caller page loses those
    /// call sites and looks complete without them.
    ///
    /// That consequence is disclosed once per session rather than per
    /// answer: `mcp::instructions`' `P4_STATIC_RECEIVER` renders it, and that
    /// constant's doc comment has the measurements, the rejected per-row and
    /// per-response shapes, and why a count of what was missed must never be
    /// offered in place of a pointer to where it went.
    ///
    /// For the tool-surface consequence: `Resolved` means the MCP
    /// instructions do not list the receiver-call gap in its *open* form for
    /// this language; `Unresolved` (the default) means they do.
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
    // `path()` below is the file this was read from - see `MANIFEST_PATH_ENV`.
    /// Parsed `[plugin.capabilities]`, or [`Capabilities::default`] if the
    /// table is absent - see this module's doc comment.
    pub capabilities: Capabilities,
    /// Parsed `[plugin.workspace]`, or [`WorkspaceConfig::default`] if the
    /// table is absent - see this module's doc comment.
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

    let command =
        resolve_command(&plugin.spawn.command, dir, current_bin_dir().as_deref()).with_context(|| {
            format!("invalid [plugin.spawn] command in plugin manifest at {}", manifest_path.display())
        })?;
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

impl DiscoveredPlugins {
    /// Which language claims `file_path` (project-relative, forward-slash
    /// separated), by its extension alone; `None` if no discovered plugin
    /// does. The extension is lowercased first, since the routing table's
    /// keys are lowercase-with-leading-dot by manifest convention.
    pub fn language_for(&self, file_path: &str) -> Option<&str> {
        let extension = extension_of(file_path)?;
        self.routing.get(&extension).map(String::as_str)
    }

    /// The language whose plugin indexes `file_path`: the one that claims its
    /// extension ([`language_for`](Self::language_for)), unless the file sits
    /// under a directory that same language's `[plugin.workspace]
    /// exclude_dirs` names ([`under_excluded_dir`]). Each language's
    /// exclusions are its own - `dist/app.py` is Python's even though
    /// TypeScript excludes `dist` - so this is a per-language check, never a
    /// union of every manifest's list.
    ///
    /// The single core-side answer to "is this a file the index should hold?",
    /// shared by the watcher's routing (`daemon::registry::PluginRegistry::
    /// file_changed`) and `g-mesh status`'s coverage walk (`cli::status`), so
    /// the two cannot disagree about which files a project has.
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

/// Whether `file_path` sits under a directory literally named one of
/// `exclude_dirs`, checked against **every path segment except the file name
/// itself** - the architecture doc's `[plugin.workspace] exclude_dirs`
/// ("directory names... matched by exact name, not glob") and GM-272's
/// decision 5 ("match directory NAMES on any path segment, not prefixes"):
/// `vendor/pkg/build.alpha` is excluded by `exclude_dirs = ["vendor"]`
/// exactly as `pkg/vendor/build.alpha` is - the excluded name can sit at any
/// depth, not only as a leading path component - while
/// `vendored-tools/build.alpha` is **not**, because `"vendored-tools" !=
/// "vendor"` as a whole segment; a prefix/substring match would wrongly
/// exclude it.
///
/// Empty `exclude_dirs` (the common case - most manifests declare none, and
/// every fixture that predates GM-272) short-circuits without walking the
/// path at all.
pub(crate) fn under_excluded_dir(file_path: &str, exclude_dirs: &[String]) -> bool {
    if exclude_dirs.is_empty() {
        return false;
    }
    let mut segments = file_path.split('/');
    segments.next_back(); // the file name itself names no directory
    segments.any(|segment| exclude_dirs.iter().any(|excluded| excluded == segment))
}

/// Every language among `manifests` whose `capabilities.semantic_pass` is
/// `true`, sorted.
///
/// The set `daemon::semantic`'s per-language scheduler (GM-270) asks for a
/// whole-project pass, generalized from the single hardcoded
/// `plugin::BUNDLED_LANGUAGE` question it replaces - a manifest that never
/// mentions `[plugin.capabilities]`, or sets `semantic_pass = false`
/// explicitly, is excluded by [`Capabilities::default`]'s conservative
/// default, the same way it was already excluded from ever receiving a
/// `semanticPass` request at all.
///
/// Shared by [`crate::daemon::registry::PluginRegistry::semantic_pass_languages`]
/// (the live-registry view `daemon::semantic::run_with_registry` asks) and
/// `daemon::semantic::run_once` (which has a bare `&DiscoveredPlugins` and no
/// registry to ask), so the same filter and the same sort order back both.
///
/// Sorted for determinism, not just tidiness: `manifests` is a `HashMap`, and
/// `run_once` asks each capable language for its pass *sequentially* (see
/// that function's own doc comment for why concurrently is deliberately not
/// done) - an unsorted, hash-order-dependent sequence would make which
/// language runs first (and therefore which one a shared machine's memory
/// pressure hits) vary between two otherwise identical runs for no reason
/// anyone could explain from the outside.
pub fn semantic_pass_capable_languages(manifests: &HashMap<String, PluginManifest>) -> Vec<String> {
    let mut languages: Vec<String> = manifests
        .values()
        .filter(|manifest| manifest.capabilities.semantic_pass)
        .map(|manifest| manifest.language.clone())
        .collect();
    languages.sort();
    languages
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
/// like a bare command name, else joined against `dir` and passed through
/// [`resolve_exe_suffix`] for the platform-suffix fallback.
fn resolve_path_entry(value: &str, dir: &Path) -> PathBuf {
    if has_path_separator(value) {
        resolve_exe_suffix(dir.join(value), std::env::consts::EXE_SUFFIX)
    } else {
        PathBuf::from(value)
    }
}

/// Resolves `command`: a [`BIN_DIR_PLACEHOLDER`] prefix is replaced by
/// `bin_dir` (the running executable's directory, a parameter so a test can
/// name any directory), anything else goes through [`resolve_path_entry`].
/// Errors when the placeholder is used but `bin_dir` is unknown, or when the
/// placeholder appears anywhere but at the start.
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

/// Falls back to the platform's suffixed spelling (e.g. `.exe` on Windows)
/// for a resolved `command` path that does not exist as written - see this
/// module's doc comment for why cargo's two workspace-built plugins need
/// this and the Go plugin does not.
///
/// Only actually switches to the suffixed spelling once it is confirmed to
/// exist (`suffixed.is_file()`): a spelling that resolves to nothing on disk
/// either way is left exactly as the manifest wrote it, rather than guessed
/// at, so a genuinely-missing binary still names the path this function
/// actually resolved when [`crate::daemon::plugin::missing_workspace_binary_hint`]
/// separately decides what to report for it (that function re-derives the
/// suffixed spelling itself for its message, via the same [`exe_suffixed`]
/// this function uses, rather than trusting `command` to already carry it -
/// see its own doc comment).
///
/// `suffix` is a parameter, not read from [`std::env::consts::EXE_SUFFIX`]
/// internally, so a test can exercise the Windows arm (`".exe"`) from any
/// host - the constant itself is fixed at compile time to whatever platform
/// built the test binary.
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
/// extension or `suffix` is empty (the non-Windows case, where
/// [`std::env::consts::EXE_SUFFIX`] is `""` and this must be a no-op).
/// Pure and filesystem-independent - both [`resolve_exe_suffix`] (which adds
/// the "does the suffixed spelling actually exist" check on top) and
/// `daemon::plugin::missing_workspace_binary_hint` (which uses the suffixed
/// spelling to name what a spawn attempt was actually missing, whether or
/// not it exists) share this one decision of what the suffixed spelling
/// *is*, rather than each re-deriving it.
pub(crate) fn exe_suffixed(path: &Path, suffix: &str) -> Option<PathBuf> {
    if suffix.is_empty() || path.extension().is_some() {
        return None;
    }
    let mut name = path.file_name()?.to_os_string();
    name.push(suffix);
    Some(path.with_file_name(name))
}

/// Windows' extended-length prefix on a canonicalized path, written back the
/// ordinary way when that is possible: `\\?\C:\x` as `C:\x` and
/// `\\?\UNC\srv\share` as `\\srv\share`. `None` when no rewrite applies (any
/// path that is not extended-length, and an extended-length one naming
/// something with no ordinary spelling at all - `\\?\pipe\...`,
/// `\\?\Volume{...}`) or when the ordinary spelling would be too long to be
/// legal (see below). [`plain_spelling`] is the `Path` wrapper callers use.
///
/// This exists because of what the prefix *means*. `fs::canonicalize` returns
/// it on Windows, and under it Windows stops parsing the string: `/` is no
/// longer a separator and `.`/`..` are no longer resolved, so the path is
/// handed to the filesystem exactly as spelled. Everything this module
/// resolves is then joined onto that directory
/// ([`resolve_path_entry`]/[`resolve_arg`]), and every manifest in this tree
/// spells its own entry point with forward slashes and a leading `./` or
/// `../` - so `args = ["dist/src/index.js"]` under a canonicalized directory
/// becomes `\\?\D:\...\typescript\dist/src/index.js`, which is no longer a
/// path the OS will resolve but a string whatever runs it has to make sense
/// of. `node` does not: on CI run 35451298477 every conformance-kit test
/// whose plugin is spawned through node failed with the plugin exiting 1
/// before writing a byte, while the Go plugin - a native binary reached from
/// the same canonicalized directory - passed, and the *same* node plugin
/// spawned by the daemon from an un-canonicalized directory passed too
/// (`plugin_bridge`).
///
/// 259, not 260: `MAX_PATH` counts the terminating NUL, so that is the
/// longest ordinary path Windows accepts, and the prefix is the only way to
/// write a longer one. Measured in UTF-16 code units, which is what Windows
/// counts, rather than in bytes or `char`s.
///
/// A `&str`, not a `Path`, and no `#[cfg(windows)]`: `Path`'s prefix parsing
/// is the host's, so a `cfg`-gated version could only ever be tested on the
/// platform it is for. `fs::canonicalize` on Unix returns a path beginning
/// with `/`, so no Unix path reaches the branches below - which makes running
/// this unconditionally both harmless and the reason its Windows arm is
/// testable from any host, exactly as [`exe_suffixed`] takes its suffix as an
/// argument for that reason.
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

/// [`plain_win32_path`] over an owned path, left exactly as it came when no
/// rewrite applies - including any path that is not valid UTF-8, which cannot
/// be one of the spellings this rewrites.
pub(crate) fn plain_spelling(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(plain_win32_path) {
        Some(plain) => PathBuf::from(plain),
        None => path,
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
mod tests;
