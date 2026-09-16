//! What the plugin knows about itself: its language, the extensions it
//! claims, and the directories its walk must skip.
//!
//! # Why a plugin reads the manifest core already read
//!
//! `plugin.toml` is core's startup-time source of truth - it decides routing
//! and MCP instruction assembly before any plugin process exists
//! (`daemon::manifest`). Two of its fields are also facts the *plugin* needs
//! for its own walk: `[plugin.languages] extensions` and
//! `[plugin.workspace] exclude_dirs`. Core does not send them (nothing in the
//! protocol carries configuration), so the plugin either re-reads the file or
//! carries its own copy of the same list.
//!
//! Carrying a copy is what the TS plugin does, and it has the drift this
//! crate can avoid: `ignorePolicy.ts`'s `HARD_EXCLUDED_DIRS` and
//! `plugins/typescript/plugin.toml`'s `exclude_dirs` are the same list
//! written twice, with a comment explaining how they relate. So the SDK reads
//! the manifest when it can find one, and falls back to the [`PluginSpec`]
//! the plugin declared in code when it cannot.
//!
//! # Finding it
//!
//! 1. [`MANIFEST_PATH_ENV`], if set - an explicit path to the `plugin.toml`.
//! 2. `plugin.toml` beside the executable. This is the installed layout: a
//!    plugin directory holds its manifest and its binary, which is exactly
//!    what `[plugin.spawn] command = "./g-mesh-plugin-rust"` means.
//! 3. The [`PluginSpec`] alone.
//!
//! The env variable exists because (2) is false in precisely one situation
//! that matters: a binary built by cargo, which lives in `target/debug/` with
//! no manifest anywhere near it. That is how every `#[test]` runs a plugin,
//! so it has to work - and [`testing::PluginCheck`](crate::testing::PluginCheck),
//! which writes a manifest into a scratch directory, sets it.
//!
//! A manifest whose `language` is not the plugin's is a manifest for some
//! other plugin: it is reported and ignored, never merged. Everything else
//! that goes wrong reading it (unreadable, malformed, missing sections) is
//! also reported and ignored rather than fatal - a plugin that refuses to
//! start takes its language's whole index with it, and the spec is a complete
//! answer on its own.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// An explicit path to this plugin's `plugin.toml` - see this module's doc
/// for why it exists.
pub const MANIFEST_PATH_ENV: &str = "G_MESH_PLUGIN_MANIFEST";

/// What a plugin declares about itself in code, as the argument to
/// [`run`](crate::run).
///
/// Every field has a counterpart in `plugin.toml`, and the manifest wins when
/// one is found. Declaring them here as well is not redundancy for its own
/// sake: it is what makes a plugin binary runnable - and testable - outside
/// an installed layout.
#[derive(Debug, Clone)]
pub struct PluginSpec {
    language: &'static str,
    version: &'static str,
    extensions: &'static [&'static str],
    exclude_dirs: &'static [&'static str],
}

impl PluginSpec {
    /// `language` must equal the [`Extractor::LANGUAGE`](crate::Extractor::LANGUAGE)
    /// of the extractor it is passed with, and the manifest's `language`,
    /// which must equal the plugin directory's name. `extensions` are
    /// lowercase and dot-prefixed (`".rs"`).
    pub fn new(language: &'static str, version: &'static str, extensions: &'static [&'static str]) -> Self {
        Self { language, version, extensions, exclude_dirs: &[] }
    }

    /// Directory names the walk never descends into, on top of
    /// [`BASELINE_EXCLUDED_DIRS`](crate::walk_project). Mirror
    /// `[plugin.workspace] exclude_dirs`: core's watcher reads that list to
    /// decide what never to route to this plugin, and a walk that disagrees
    /// with it indexes files no edit will ever update.
    pub fn exclude_dirs(mut self, exclude_dirs: &'static [&'static str]) -> Self {
        self.exclude_dirs = exclude_dirs;
        self
    }
}

/// The spec after the manifest has had its say - what the walk and the
/// handshake actually use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSpec {
    /// The wire identifier, from the [`PluginSpec`] always: a manifest that
    /// disagrees is a manifest for another plugin and was discarded.
    pub language: String,
    /// The version announced in the handshake. The manifest's
    /// `plugin_version` when one was read, since that is the value core
    /// fingerprints the plugin by.
    pub version: String,
    /// Lowercase, dot-prefixed extensions this plugin claims.
    pub extensions: Vec<String>,
    /// Directory names the walk skips, beyond the baseline.
    pub exclude_dirs: Vec<String>,
    /// The manifest these came from, if any - reported in the plugin's
    /// startup log so "why did it walk that" has an answer.
    pub manifest_path: Option<PathBuf>,
}

impl ResolvedSpec {
    /// Resolves `spec` against whatever manifest can be found - see this
    /// module's doc for the search order. Never fails: a manifest that cannot
    /// be used is reported on stderr and the spec stands alone.
    pub fn resolve(spec: &PluginSpec) -> Self {
        Self::resolve_from(spec, manifest_path())
    }

    /// [`ResolvedSpec::resolve`] with the search already done.
    ///
    /// Split out so the resolution rules are testable without a
    /// process-global environment variable. Every test that went through
    /// `resolve` would have had to set [`MANIFEST_PATH_ENV`], which is shared
    /// by every thread in the process - so the tests would have needed a
    /// mutex between them, and would still have been one stray reader away
    /// from a flake that only appears under load.
    pub(crate) fn resolve_from(spec: &PluginSpec, path: Option<PathBuf>) -> Self {
        let mut resolved = Self {
            language: spec.language.to_string(),
            version: spec.version.to_string(),
            extensions: spec.extensions.iter().map(|e| e.to_lowercase()).collect(),
            exclude_dirs: spec.exclude_dirs.iter().map(|d| (*d).to_string()).collect(),
            manifest_path: None,
        };

        let Some(path) = path else { return resolved };
        let manifest = match read_manifest(&path) {
            Ok(manifest) => manifest,
            Err(err) => {
                eprintln!("[{}] ignoring {}: {err:#}", spec.language, path.display());
                return resolved;
            }
        };
        if manifest.plugin.language != spec.language {
            eprintln!(
                "[{}] ignoring {}: it declares language {:?}, not this plugin's",
                spec.language,
                path.display(),
                manifest.plugin.language
            );
            return resolved;
        }

        resolved.version = manifest.plugin.plugin_version;
        resolved.extensions = manifest.plugin.languages.extensions.iter().map(|e| e.to_lowercase()).collect();
        resolved.exclude_dirs = manifest.plugin.workspace.exclude_dirs;
        resolved.manifest_path = Some(path);
        resolved
    }
}

/// [`MANIFEST_PATH_ENV`], else `plugin.toml` beside the executable, else
/// nothing.
fn manifest_path() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(MANIFEST_PATH_ENV).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit));
    }
    let beside = std::env::current_exe().ok()?.parent()?.join("plugin.toml");
    beside.is_file().then_some(beside)
}

fn read_manifest(path: &Path) -> anyhow::Result<RawManifest> {
    let contents = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&contents)?)
}

/// Only the three fields a plugin needs back out of its own manifest. Core's
/// `daemon::manifest::read_manifest` is the validating reader; this one is
/// deliberately permissive about everything it does not use, so a manifest
/// gaining a field never stops a plugin from starting.
#[derive(Debug, Deserialize)]
struct RawManifest {
    plugin: RawPlugin,
}

#[derive(Debug, Deserialize)]
struct RawPlugin {
    language: String,
    plugin_version: String,
    languages: RawLanguages,
    #[serde(default)]
    workspace: RawWorkspace,
}

#[derive(Debug, Deserialize)]
struct RawLanguages {
    extensions: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawWorkspace {
    #[serde(default)]
    exclude_dirs: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: PluginSpec = PluginSpec {
        language: "toy",
        version: "9.9.9-from-code",
        extensions: &[".toy"],
        exclude_dirs: &["from-code"],
    };

    /// Writes `contents` to a uniquely named file and resolves against it.
    /// `None` resolves against a path that does not exist.
    fn resolve_against(tag: &str, contents: Option<&str>) -> ResolvedSpec {
        let dir = std::env::temp_dir().join(format!("g-mesh-sdk-manifest-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plugin.toml");
        if let Some(contents) = contents {
            std::fs::write(&path, contents).unwrap();
        }
        let resolved = ResolvedSpec::resolve_from(&SPEC, Some(path));
        let _ = std::fs::remove_dir_all(&dir);
        resolved
    }

    #[test]
    fn a_manifest_wins_over_the_spec() {
        let resolved = resolve_against(
            "wins",
            Some(
                "[plugin]\nlanguage = \"toy\"\nprotocol_version = 2\nplugin_version = \"1.2.3\"\n\n\
                 [plugin.spawn]\ncommand = \"./toy\"\n\n\
                 [plugin.languages]\nextensions = [\".toy\", \".TOYX\"]\n\n\
                 [plugin.workspace]\nexclude_dirs = [\"vendor\"]\n",
            ),
        );
        assert_eq!(resolved.version, "1.2.3");
        assert_eq!(resolved.extensions, vec![".toy", ".toyx"]);
        assert_eq!(resolved.exclude_dirs, vec!["vendor"]);
        assert!(resolved.manifest_path.is_some());
    }

    #[test]
    fn a_manifest_for_another_language_is_ignored_rather_than_merged() {
        let resolved = resolve_against(
            "other",
            Some(
                "[plugin]\nlanguage = \"other\"\nprotocol_version = 2\nplugin_version = \"1.2.3\"\n\n\
                 [plugin.spawn]\ncommand = \"./other\"\n\n\
                 [plugin.languages]\nextensions = [\".other\"]\n",
            ),
        );
        assert_eq!(resolved.language, "toy");
        assert_eq!(resolved.version, "9.9.9-from-code");
        assert_eq!(resolved.extensions, vec![".toy"]);
        assert_eq!(resolved.manifest_path, None);
    }

    #[test]
    fn a_malformed_or_missing_manifest_leaves_the_spec_standing() {
        for (tag, contents) in [
            ("missing", None),
            ("not-toml", Some("this is not toml {{{")),
            ("incomplete", Some("[plugin]\nlanguage = \"toy\"\n")),
        ] {
            let resolved = resolve_against(tag, contents);
            assert_eq!(resolved.extensions, vec![".toy"], "{tag}");
            assert_eq!(resolved.exclude_dirs, vec!["from-code"], "{tag}");
            assert_eq!(resolved.manifest_path, None, "{tag}");
        }
    }

    /// The spec alone, with nothing to resolve against - a plugin binary run
    /// straight out of `target/debug` with no manifest anywhere.
    #[test]
    fn with_no_manifest_at_all_the_spec_is_the_answer() {
        let resolved = ResolvedSpec::resolve_from(&SPEC, None);
        assert_eq!(resolved.language, "toy");
        assert_eq!(resolved.version, "9.9.9-from-code");
        assert_eq!(resolved.extensions, vec![".toy"]);
        assert_eq!(resolved.exclude_dirs, vec!["from-code"]);
        assert_eq!(resolved.manifest_path, None);
    }
}
