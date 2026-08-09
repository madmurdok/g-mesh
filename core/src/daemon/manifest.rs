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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
/// With no override set, returns up to two roots:
/// 1. `~/.g-mesh/plugins/` - user-installed, global (not per-project),
///    resolved the same way `config::global_config_path` resolves everything
///    else under `~/.g-mesh/...`. Skipped (not an error - matches this
///    module's "a root that does not exist contributes nothing" philosophy
///    in [`discover`]) if the home directory cannot be resolved at all.
/// 2. The bundled root. In a repo checkout this resolves the same way
///    `daemon::plugin::plugin_entry_path`'s dev-checkout fallback does today:
///    relative to this crate's own source tree
///    (`CARGO_MANIFEST_DIR/../plugins/`), since `core/` and `plugins/` are
///    sibling directories with no distribution pipeline yet. TODO: a real
///    install has no `CARGO_MANIFEST_DIR` - that case needs resolving
///    relative to `std::env::current_exe()` instead, matching the "JS/TS
///    plugin ships in the same release archive" line in the v1 architecture
///    doc's Distribution section; not implemented yet because there is no
///    installed release to test it against.
///
/// With [`PLUGIN_ROOTS_OVERRIDE_ENV`] set, both standard roots are replaced
/// entirely by a single-element vec of just that one path.
pub fn default_roots() -> Vec<PathBuf> {
    if let Ok(over) = std::env::var(PLUGIN_ROOTS_OVERRIDE_ENV) {
        return vec![PathBuf::from(over)];
    }

    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".g-mesh").join("plugins"));
    }
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins"));
    roots
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
}

/// Reads and validates `<dir>/plugin.toml`.
///
/// Hard error on: malformed TOML, a missing required field, `language` not
/// equal to `dir`'s final path component, or an unrecognized
/// `protocol_version`. Every error names the manifest path; the two
/// semantic checks additionally name both the declared and expected value,
/// matching `protocol::handshake::verify`'s error style.
pub fn read_manifest(dir: &Path) -> Result<PluginManifest> {
    let manifest_path = dir.join(MANIFEST_FILE_NAME);

    let contents = fs::read_to_string(&manifest_path).with_context(|| {
        format!("failed to read plugin manifest at {}", manifest_path.display())
    })?;
    let raw: RawManifest = toml::from_str(&contents).with_context(|| {
        format!("failed to parse plugin manifest at {}", manifest_path.display())
    })?;
    let plugin = raw.plugin;

    let dir_name = dir.file_name().and_then(|name| name.to_str()).with_context(|| {
        format!(
            "plugin manifest directory {} has no valid (UTF-8) directory name",
            dir.display()
        )
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

    Ok(PluginManifest {
        language: plugin.language,
        protocol_version: plugin.protocol_version,
        plugin_version: plugin.plugin_version,
        command,
        args,
        extensions: plugin.languages.extensions,
        fingerprint_ignore: plugin.fingerprint.ignore,
        manifest_dir: dir.to_path_buf(),
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
            let entry = entry.with_context(|| {
                format!("failed to read plugin discovery root {}", root.display())
            })?;
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

    #[test]
    fn default_roots_bundled_entry_resolves_to_the_sibling_plugins_directory() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(PLUGIN_ROOTS_OVERRIDE_ENV);

        let roots = default_roots();
        let bundled = roots.last().expect("default_roots must return at least the bundled root");

        assert!(
            bundled.join("js-ts").join(MANIFEST_FILE_NAME).is_file(),
            "expected {} to contain js-ts/plugin.toml",
            bundled.display()
        );
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
        let extensions =
            extensions.iter().map(|ext| format!("\"{ext}\"")).collect::<Vec<_>>().join(", ");
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
        let (_root1, root1) =
            discovery_root(&[("python", &manifest_toml("python", "1.0.0", &[".py"]))]);
        let (_root2, root2) =
            discovery_root(&[("python", &manifest_toml("python", "2.0.0", &[".py"]))]);

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

        let err = discover(&[root.clone()]).unwrap_err();
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
        assert!(
            message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()),
            "{message}"
        );
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
        assert!(
            message.contains(&dir.join(MANIFEST_FILE_NAME).to_string_lossy().into_owned()),
            "{message}"
        );
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

    /// The bundled JS/TS plugin's own `plugin.toml` (`plugins/js-ts/plugin.toml`,
    /// embedded at compile time so this test tracks the committed file, not a
    /// copy) must itself satisfy `read_manifest`. Its directory on disk is
    /// named `js-ts`, not `typescript` - a known, tracked mismatch (see that
    /// file's own header comment and the architecture doc) still open for a
    /// later discovery-wiring task to resolve - so this test cannot point
    /// `read_manifest` at the real path directly; instead it copies the exact
    /// file contents into a tempdir named `typescript`, which is the name
    /// `read_manifest` requires today, and confirms the content itself is
    /// well-formed and matches the plugin's real handshake/version values.
    #[test]
    fn the_bundled_js_ts_plugin_manifest_parses_once_directory_named_correctly() {
        const BUNDLED_JS_TS_MANIFEST: &str = include_str!("../../../plugins/js-ts/plugin.toml");

        let (_root, dir) = plugin_dir("typescript", BUNDLED_JS_TS_MANIFEST);

        let manifest = read_manifest(&dir).unwrap();

        assert_eq!(manifest.language, "typescript");
        assert_eq!(manifest.protocol_version, CURRENT_PROTOCOL_VERSION);
        assert_eq!(manifest.command, PathBuf::from("node"));
        assert_eq!(manifest.args, vec![dir.join("dist/src/index.js").to_string_lossy().into_owned()]);
        assert!(manifest.extensions.contains(&".ts".to_string()));
        assert!(manifest.extensions.contains(&".tsx".to_string()));
        assert!(manifest.extensions.contains(&".js".to_string()));
    }
}
