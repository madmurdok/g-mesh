//! `g-mesh plugins list`: report the language plugins this install has,
//! globally rather than per-project - a plugin is a property of the g-mesh
//! install, not of any one indexed project.
//!
//! See `docs/architecture/plugin-modularity.md`'s Interfaces section for the
//! behavior this implements: every manifest found under either discovery
//! root is reported, tagged with which root it came from
//! ([`PluginStatus::Bundled`] or [`PluginStatus::Installed`]); a manifest
//! that fails to parse is listed with an error string rather than silently
//! dropped or aborting the whole command - unlike daemon startup (see
//! [`crate::daemon::manifest::discover`], which hard-fails on the first bad
//! manifest), a listing tool's job is to surface what is there, including
//! what is broken.
//!
//! # Why this does not just call `daemon::manifest::discover`
//!
//! `discover()` returns only the merged, daemon-ready result: one manifest
//! per language (an earlier root's entry silently shadows a same-named later
//! one) and a hard error the instant any manifest fails to parse or two
//! languages claim the same extension. None of that fits a listing command:
//! this needs to know *which root* each manifest came from (`discover()`
//! throws that away once it merges into one `HashMap`), it must keep going
//! after a bad manifest instead of aborting, and extension-routing conflicts
//! are a daemon-startup concern this command has no reason to duplicate. So
//! this module walks each root itself, calling
//! [`crate::daemon::manifest::read_manifest`] per plugin directory found -
//! the same per-manifest primitive `discover()` uses - rather than reusing
//! `discover()`'s all-or-nothing merge.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::daemon::manifest::{self};

/// `<root>/<language-dir>/plugin.toml` - matches
/// `daemon::manifest`'s own (private) constant of the same name.
const MANIFEST_FILE_NAME: &str = "plugin.toml";

/// Which discovery root a plugin's manifest was found under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Ships inside this g-mesh install - there is no separate install step,
    /// and no version to manage independently of the core binary's own.
    Bundled,
    /// Found under `~/.g-mesh/plugins/` - a user-installed plugin, either
    /// filling in a language the bundled install does not ship, or
    /// deliberately overriding a bundled one of the same language (see
    /// `daemon::manifest::discover`'s doc comment on shadowing).
    Installed,
}

impl PluginStatus {
    fn label(self) -> &'static str {
        match self {
            PluginStatus::Bundled => "bundled",
            PluginStatus::Installed => "installed",
        }
    }
}

/// What discovering one plugin directory produced: a usable manifest, or -
/// unlike `daemon::manifest::discover`, which hard-fails here - the reason
/// it could not be read. Kept as a plain error string (not the root
/// `anyhow::Error`) so [`PluginInfo`] stays `Clone + PartialEq + Eq` like
/// the rest of this module's public types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginOutcome {
    Loaded { version: String, status: PluginStatus },
    /// In place of a version/status pair - see this module's doc comment.
    Error(String),
}

/// One plugin directory this install has, found under either discovery
/// root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// The manifest's `language` field for a plugin that parsed - or, when
    /// it did not, the directory it was found in (the best identifier left
    /// once the manifest itself cannot be trusted).
    pub language: String,
    pub outcome: PluginOutcome,
}

/// Runs `g-mesh plugins list`.
pub fn run() -> Result<()> {
    print!("{}", render(&list()?));
    Ok(())
}

/// Every plugin this install has, across both discovery roots.
pub fn list() -> Result<Vec<PluginInfo>> {
    list_from_roots(&default_roots()?)
}

/// The two roots `list()` scans, in the order they are reported: the user's
/// own `~/.g-mesh/plugins/` first, then the bundled root - the same
/// precedence order `daemon::manifest::discover`'s doc comment documents
/// (`~/.g-mesh/plugins/` is scanned before the bundled root, so a
/// user-installed plugin can intentionally override a bundled one).
///
/// The bundled root is resolved the same way
/// [`crate::daemon::plugin::plugin_entry_path`] resolves the bundled JS/TS
/// plugin's entry point today: relative to this crate's own source tree,
/// baked in at compile time - `core/` and `plugins/` are sibling directories
/// in this repo and there is no distribution pipeline yet.
fn default_roots() -> Result<Vec<(PathBuf, PluginStatus)>> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    let installed_root = home.join(".g-mesh").join("plugins");
    let bundled_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    Ok(vec![(installed_root, PluginStatus::Installed), (bundled_root, PluginStatus::Bundled)])
}

/// Scans each of `roots` in order, reporting every plugin directory found
/// under it (see [`scan_root`]) - the testable core of [`list`], parameterized
/// over the roots so a test can point it at a fixture directory instead of
/// the real `~/.g-mesh/plugins/`.
fn list_from_roots(roots: &[(PathBuf, PluginStatus)]) -> Result<Vec<PluginInfo>> {
    let mut infos = Vec::new();
    for (root, status) in roots {
        infos.extend(scan_root(root, *status)?);
    }
    Ok(infos)
}

/// Lists every plugin directory directly under `root` (one level, matching
/// `daemon::manifest::discover`'s own scan shape), tagging each with
/// `status`.
///
/// A root that does not exist, or is otherwise unreadable, contributes
/// nothing - not an error, same convention `discover()` uses for a missing
/// root. A directory with no `plugin.toml` in it is skipped, same as
/// `discover()`. Unlike `discover()`, a manifest that fails to read/parse is
/// kept as an [`PluginOutcome::Error`] entry rather than aborting the scan.
fn scan_root(root: &Path, status: PluginStatus) -> Result<Vec<PluginInfo>> {
    let mut infos = Vec::new();

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Ok(infos),
    };

    for entry in entries {
        let entry = entry
            .with_context(|| format!("failed to read plugin discovery root {}", root.display()))?;
        let dir = entry.path();
        if !dir.is_dir() || !dir.join(MANIFEST_FILE_NAME).is_file() {
            continue;
        }

        let dir_name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("?").to_string();

        let outcome = match manifest::read_manifest(&dir) {
            Ok(manifest) => {
                infos.push(PluginInfo {
                    language: manifest.language,
                    outcome: PluginOutcome::Loaded { version: manifest.plugin_version, status },
                });
                continue;
            }
            Err(err) => PluginOutcome::Error(format!("{err:#}")),
        };
        infos.push(PluginInfo { language: dir_name, outcome });
    }

    Ok(infos)
}

/// Renders the plugin list the way `plugins list` prints it: one line per
/// plugin naming its language, then either its version and status, or - for
/// a manifest that failed to read - an error in their place.
pub fn render(plugins: &[PluginInfo]) -> String {
    let mut out = String::new();
    for plugin in plugins {
        match &plugin.outcome {
            PluginOutcome::Loaded { version, status } => {
                let _ = writeln!(out, "{}  {}  {}", plugin.language, version, status.label());
            }
            PluginOutcome::Error(message) => {
                let _ = writeln!(out, "{}  error: {}", plugin.language, message);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled JS/TS plugin's own `plugin.toml`, embedded at compile
    /// time so this test tracks the committed file, not a copy - same
    /// approach `daemon::manifest`'s own
    /// `the_bundled_js_ts_plugin_manifest_parses_once_directory_named_correctly`
    /// test uses. Its containing directory on disk is named `typescript`
    /// (task 155 renamed it from `js-ts` precisely so it would satisfy
    /// `read_manifest`'s language-equals-directory-name rule directly), which
    /// is also the name a fixture root here has to use for the same reason.
    const BUNDLED_JS_TS_MANIFEST: &str = include_str!("../../../plugins/typescript/plugin.toml");

    /// Builds a fresh tempdir root containing one `<dir_name>/plugin.toml`
    /// per entry in `plugins`.
    fn fixture_root(plugins: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        for (dir_name, body) in plugins {
            let plugin_dir = root.path().join(dir_name);
            fs::create_dir_all(&plugin_dir).unwrap();
            fs::write(plugin_dir.join(MANIFEST_FILE_NAME), body).unwrap();
        }
        let path = root.path().to_path_buf();
        (root, path)
    }

    /// The acceptance criterion this command exists to satisfy: with only
    /// the bundled JS/TS plugin's manifest present, `list` reports exactly
    /// that one entry, marked bundled, with the language and version its
    /// real manifest declares.
    #[test]
    fn lists_exactly_the_one_bundled_js_ts_plugin() {
        let (_root, bundled_root) = fixture_root(&[("typescript", BUNDLED_JS_TS_MANIFEST)]);
        let (_empty, installed_root) = fixture_root(&[]);

        let plugins = list_from_roots(&[
            (installed_root, PluginStatus::Installed),
            (bundled_root, PluginStatus::Bundled),
        ])
        .expect("the bundled plugin's manifest must parse");

        assert_eq!(plugins.len(), 1, "{plugins:?}");
        assert_eq!(plugins[0].language, "typescript");
        assert_eq!(
            plugins[0].outcome,
            PluginOutcome::Loaded { version: "2.0.0".to_string(), status: PluginStatus::Bundled }
        );
    }

    /// A plugin found under `~/.g-mesh/plugins/` is reported `Installed`,
    /// not `Bundled` - status is derived from which root it came from, not
    /// assumed.
    #[test]
    fn a_plugin_found_under_the_installed_root_is_reported_installed() {
        let (_root, installed_root) = fixture_root(&[("typescript", BUNDLED_JS_TS_MANIFEST)]);
        let (_empty, bundled_root) = fixture_root(&[]);

        let plugins = list_from_roots(&[
            (installed_root, PluginStatus::Installed),
            (bundled_root, PluginStatus::Bundled),
        ])
        .unwrap();

        assert_eq!(plugins.len(), 1);
        assert_eq!(
            plugins[0].outcome,
            PluginOutcome::Loaded { version: "2.0.0".to_string(), status: PluginStatus::Installed }
        );
    }

    /// One good manifest and one malformed manifest in the same root: the
    /// good one lists normally, the bad one lists with a visible error
    /// indicator, and the scan as a whole still succeeds - it must not abort
    /// just because one entry is broken.
    #[test]
    fn a_malformed_manifest_is_listed_with_an_error_instead_of_aborting_the_scan() {
        let (_root, root) = fixture_root(&[
            ("typescript", BUNDLED_JS_TS_MANIFEST),
            ("broken", "this is not [ valid toml"),
        ]);

        let plugins =
            list_from_roots(&[(root, PluginStatus::Bundled)]).expect("a bad manifest must not abort the scan");

        assert_eq!(plugins.len(), 2, "{plugins:?}");

        let good = plugins.iter().find(|p| p.language == "typescript").expect("good entry missing");
        assert_eq!(
            good.outcome,
            PluginOutcome::Loaded { version: "2.0.0".to_string(), status: PluginStatus::Bundled }
        );

        let bad = plugins.iter().find(|p| p.language == "broken").expect("bad entry missing");
        assert!(matches!(&bad.outcome, PluginOutcome::Error(_)), "{bad:?}");
    }

    /// A root that does not exist contributes nothing and is not an error -
    /// same convention `daemon::manifest::discover` uses.
    #[test]
    fn a_root_that_does_not_exist_contributes_nothing() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("does-not-exist");

        let plugins = list_from_roots(&[(missing, PluginStatus::Bundled)]).unwrap();

        assert!(plugins.is_empty());
    }

    #[test]
    fn render_reports_language_version_and_status_for_a_loaded_plugin() {
        let plugins = vec![PluginInfo {
            language: "typescript".to_string(),
            outcome: PluginOutcome::Loaded { version: "2.0.0".to_string(), status: PluginStatus::Bundled },
        }];

        let rendered = render(&plugins);

        assert!(rendered.contains("typescript"), "{rendered}");
        assert!(rendered.contains("2.0.0"), "{rendered}");
        assert!(rendered.contains("bundled"), "{rendered}");
    }

    #[test]
    fn render_reports_installed_status() {
        let plugins = vec![PluginInfo {
            language: "python".to_string(),
            outcome: PluginOutcome::Loaded { version: "0.1.0".to_string(), status: PluginStatus::Installed },
        }];

        let rendered = render(&plugins);

        assert!(rendered.contains("installed"), "{rendered}");
    }

    #[test]
    fn render_shows_an_error_indicator_for_a_broken_manifest() {
        let plugins = vec![PluginInfo {
            language: "broken".to_string(),
            outcome: PluginOutcome::Error("failed to parse plugin manifest".to_string()),
        }];

        let rendered = render(&plugins);

        assert!(rendered.contains("broken"), "{rendered}");
        assert!(rendered.contains("error"), "{rendered}");
        assert!(rendered.contains("failed to parse plugin manifest"), "{rendered}");
    }

    #[test]
    fn render_of_no_plugins_is_empty() {
        assert_eq!(render(&[]), "");
    }
}
