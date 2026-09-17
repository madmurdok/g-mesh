//! `g-mesh plugins list`: report the language plugins this install has,
//! globally rather than per-project - a plugin is a property of the g-mesh
//! install, not of any one indexed project.
//!
//! See `docs/architecture/plugin-modularity.md`'s Interfaces section for the
//! behavior this implements: every *effective* plugin - one entry per
//! language, exactly the set `daemon::manifest::discover` would hand the
//! daemon to spawn from these same roots - is reported, tagged with which
//! root it came from ([`PluginStatus::Bundled`] or [`PluginStatus::Installed`]);
//! a manifest that fails to parse is listed with an error string rather than
//! silently dropped or aborting the whole command - unlike daemon startup
//! (see [`crate::daemon::manifest::discover`], which hard-fails on the first
//! bad manifest), a listing tool's job is to surface what is there,
//! including what is broken.
//!
//! # Why this does not just call `daemon::manifest::discover`
//!
//! `discover()` returns only the merged, daemon-ready result: one manifest
//! per language (an earlier root's entry silently shadows a same-named later
//! one) and a hard error the instant any manifest fails to parse or two
//! languages claim the same extension. Two of those three properties do not
//! fit a listing command: this needs to know *which root* each manifest came
//! from (`discover()` throws that away once it merges into one `HashMap`),
//! and it must keep going after a bad manifest instead of aborting the whole
//! scan (extension-routing conflicts are the one property this command does
//! duplicate `discover()`'s stance on by *not* re-checking them - a daemon-
//! startup concern this command has no reason to raise on its own). So this
//! module walks each root itself, calling
//! [`crate::daemon::manifest::read_manifest`] per plugin directory found -
//! the same per-manifest primitive `discover()` uses - rather than reusing
//! `discover()`'s all-or-nothing merge.
//!
//! The one property this module *must* keep from `discover()`, on pain of
//! exactly the bug GM-306 fixed: **an earlier root's language shadows the
//! same language in a later one**, the same "earlier root wins" rule
//! `discover()` applies and this file's own [`default_roots`] already
//! documents as the precedence order. `list_from_roots` used to skip that
//! step - it walked every root and `extend`ed the results with no
//! deduplication at all, on the theory (recorded, and wrong, in a comment on
//! [`default_roots`] that has since been corrected) that the installed and
//! checkout bundled roots are never both real on the same machine. They are
//! both real on any machine that built the binary it is running, and on
//! every developer's machine besides - which is every machine anyone
//! reading this comment is likely to run `cargo build` on - so every plugin
//! this install ships was printed twice, invisibly on the one machine shape
//! (a user's, with no checkout) where nobody would notice. [`list_from_roots`]
//! now re-applies the same shadow rule `discover()` uses, so what this
//! command prints is never wider than what the daemon would actually spawn:
//! a checkout root's `plugin_version` disagreeing with an installed root's
//! (increasingly possible as of GM-303's `plugin_version` rule) shows only
//! the version that would actually run, not both.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::daemon::manifest::{self, Capabilities};

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
    Loaded {
        version: String,
        status: PluginStatus,
        /// What this plugin declares it can do - see
        /// `daemon::manifest::Capabilities`'s own doc comment. Carried
        /// through so `render` can show it: capabilities are read from the
        /// manifest specifically so they are knowable without spawning
        /// anything (see that type's doc comment), which is exactly the
        /// constraint a listing command that never spawns a plugin process
        /// already operates under.
        capabilities: Capabilities,
    },
    /// In place of a version/status/capabilities triple - see this module's
    /// doc comment.
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

/// Every plugin this install has - one entry per language, the same
/// language set `daemon::manifest::discover` would hand the daemon to spawn
/// from these same roots (see [`list_from_roots`] for the shadow rule that
/// guarantees this).
pub fn list() -> Result<Vec<PluginInfo>> {
    list_from_roots(&default_roots()?)
}

/// The roots `list()` scans, in the order they are reported: the user's own
/// `~/.g-mesh/plugins/` first, then the bundled roots - the same precedence
/// order `daemon::manifest::discover`'s doc comment documents
/// (`~/.g-mesh/plugins/` is scanned before the bundled root, so a
/// user-installed plugin can intentionally override a bundled one).
///
/// Delegating the bundled half to [`manifest::bundled_roots`] rather than
/// re-deriving it is what keeps this listing honest about what the daemon
/// would actually spawn: both the installed layout (a release archive's
/// `plugins/` beside the executable) and the checkout layout are reported.
/// They are *not* mutually exclusive - a checkout that has ever built its
/// own binary (`cargo build`, no install step involved) has both a
/// `target/<profile>/plugins/`-shaped installed root (once anything
/// populates it, e.g. a bundled release unpacked over a checkout, or a
/// developer copying one in) and its always-real `CARGO_MANIFEST_DIR/../plugins`
/// checkout root live at once; that is every developer's machine, and every
/// machine that produced the binary it is running. This function's job is
/// only to name both roots in precedence order and let [`list_from_roots`]
/// decide which one wins per language when both are real - not to assume
/// only one of them ever is.
fn default_roots() -> Result<Vec<(PathBuf, PluginStatus)>> {
    let home = dirs::home_dir().context("could not resolve home directory")?;
    let user_root = home.join(".g-mesh").join("plugins");
    let mut roots = vec![(user_root, PluginStatus::Installed)];
    roots.extend(manifest::bundled_roots().into_iter().map(|root| (root, PluginStatus::Bundled)));
    Ok(roots)
}

/// Scans each of `roots` in order, reporting one entry per language found
/// under them (see [`scan_root`]) - the testable core of [`list`], parameterized
/// over the roots so a test can point it at a fixture directory instead of
/// the real `~/.g-mesh/plugins/`.
///
/// # Shadowing: the earlier root wins, same as `discover()`
///
/// A language already reported from an earlier root is skipped when the
/// same language turns up again under a later one - `HashSet`-tracked as
/// entries accumulate, in the same root order `roots` was given in. This is
/// not a display-only policy invented here: it is `daemon::manifest::discover`'s
/// own "an earlier root's entry silently shadows a same-named later one"
/// rule (see that function's doc comment), reapplied so that what this
/// command prints never disagrees with what the daemon would actually spawn
/// from the same roots. The alternative - printing one line per root
/// regardless of language, as this function did before GM-306 - means every
/// plugin an install ships is listed twice on any machine where the
/// installed and checkout bundled roots are both real (see [`default_roots`]),
/// and silently picks whichever root's entry happens to render last as the
/// one a reader's eye lands on, with no indication that only the *other* one
/// is what actually gets spawned.
///
/// The key an entry shadows by is [`PluginInfo::language`] - the manifest's
/// own declared language for a parsed entry, or the containing directory
/// name for one that failed to parse (see [`scan_root`]) - not the manifest
/// content itself, so a later root's *different* `plugin_version` for the
/// same language is exactly the disagreement this is meant to resolve in
/// the daemon's favor, not surface twice.
fn list_from_roots(roots: &[(PathBuf, PluginStatus)]) -> Result<Vec<PluginInfo>> {
    let mut seen_languages = std::collections::HashSet::new();
    let mut infos = Vec::new();
    for (root, status) in roots {
        for info in scan_root(root, *status)? {
            if seen_languages.insert(info.language.clone()) {
                infos.push(info);
            }
            // else: an earlier, higher-precedence root already reported this
            // language - shadowed, same as `discover()`'s own rule.
        }
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
        let entry =
            entry.with_context(|| format!("failed to read plugin discovery root {}", root.display()))?;
        let dir = entry.path();
        if !dir.is_dir() || !dir.join(MANIFEST_FILE_NAME).is_file() {
            continue;
        }

        let dir_name = dir.file_name().and_then(|name| name.to_str()).unwrap_or("?").to_string();

        let outcome = match manifest::read_manifest(&dir) {
            Ok(manifest) => {
                infos.push(PluginInfo {
                    language: manifest.language,
                    outcome: PluginOutcome::Loaded {
                        version: manifest.plugin_version,
                        status,
                        capabilities: manifest.capabilities,
                    },
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
/// plugin naming its language, then either its version, status and
/// capabilities, or - for a manifest that failed to read - an error in their
/// place.
pub fn render(plugins: &[PluginInfo]) -> String {
    let mut out = String::new();
    for plugin in plugins {
        match &plugin.outcome {
            PluginOutcome::Loaded { version, status, capabilities } => {
                let _ = writeln!(
                    out,
                    "{}  {}  {}  {}",
                    plugin.language,
                    version,
                    status.label(),
                    render_capabilities(capabilities)
                );
            }
            PluginOutcome::Error(message) => {
                let _ = writeln!(out, "{}  error: {}", plugin.language, message);
            }
        }
    }
    out
}

/// One plugin's `[plugin.capabilities]`, as `render` shows them -
/// `semantic_pass` as `yes`/`no` (there is no third state to distinguish
/// from a boolean, unlike the two receiver-call fields) and the two
/// receiver-call fields via [`ReceiverCallResolution`]'s own `Display`
/// impl, so this has no second "resolved"/"unresolved" mapping to keep in
/// sync with the one on the type itself.
fn render_capabilities(capabilities: &Capabilities) -> String {
    format!(
        "semantic_pass={} receiver_calls={} receiver_calls_structural={}",
        if capabilities.semantic_pass { "yes" } else { "no" },
        capabilities.receiver_calls,
        capabilities.receiver_calls_structural,
    )
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

    /// The version the included manifest declares, read out of it rather
    /// than restated here.
    ///
    /// Three assertions below are about *which* version `list` reports, not
    /// about what that version happens to be, so spelling it out made a
    /// legitimate plugin bump fail three unrelated tests - and fail them
    /// confusingly, since `include_str!` bakes the manifest into the test
    /// binary and cargo only notices an edit by mtime.
    fn bundled_plugin_version() -> String {
        BUNDLED_JS_TS_MANIFEST.parse::<toml::Value>().expect("the bundled manifest must be valid TOML")
            ["plugin"]["plugin_version"]
            .as_str()
            .expect("plugin_version must be a string")
            .to_string()
    }

    /// The bundled manifest's `[plugin.capabilities]`, read through the real
    /// `read_manifest` parser rather than hand-extracted from the TOML -
    /// same "track what the plugin actually declares, not a restated copy"
    /// reasoning as [`bundled_plugin_version`], and it exercises the same
    /// parse path every assertion below is really checking.
    fn bundled_plugin_capabilities() -> Capabilities {
        let (_root, dir) = fixture_root(&[("typescript", BUNDLED_JS_TS_MANIFEST)]);
        manifest::read_manifest(&dir.join("typescript"))
            .expect("the bundled plugin's manifest must parse")
            .capabilities
    }

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
            PluginOutcome::Loaded {
                version: bundled_plugin_version(),
                status: PluginStatus::Bundled,
                capabilities: bundled_plugin_capabilities()
            }
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
            PluginOutcome::Loaded {
                version: bundled_plugin_version(),
                status: PluginStatus::Installed,
                capabilities: bundled_plugin_capabilities()
            }
        );
    }

    /// One good manifest and one malformed manifest in the same root: the
    /// good one lists normally, the bad one lists with a visible error
    /// indicator, and the scan as a whole still succeeds - it must not abort
    /// just because one entry is broken.
    #[test]
    fn a_malformed_manifest_is_listed_with_an_error_instead_of_aborting_the_scan() {
        let (_root, root) =
            fixture_root(&[("typescript", BUNDLED_JS_TS_MANIFEST), ("broken", "this is not [ valid toml")]);

        let plugins = list_from_roots(&[(root, PluginStatus::Bundled)])
            .expect("a bad manifest must not abort the scan");

        assert_eq!(plugins.len(), 2, "{plugins:?}");

        let good = plugins.iter().find(|p| p.language == "typescript").expect("good entry missing");
        assert_eq!(
            good.outcome,
            PluginOutcome::Loaded {
                version: bundled_plugin_version(),
                status: PluginStatus::Bundled,
                capabilities: bundled_plugin_capabilities()
            }
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

    /// A minimal well-formed `plugin.toml` body for a given language and
    /// version - the `list_from_roots` analog of `daemon::manifest`'s own
    /// `manifest_toml` test helper, needed here because this module's tests
    /// otherwise only ever exercise the *real* bundled JS/TS manifest
    /// ([`BUNDLED_JS_TS_MANIFEST`]), which cannot vary its own version to
    /// build the two-roots-disagree fixtures below.
    fn plugin_toml(language: &str, plugin_version: &str) -> String {
        format!(
            r#"
[plugin]
language = "{language}"
protocol_version = {version}
plugin_version = "{plugin_version}"

[plugin.spawn]
command = "node"

[plugin.languages]
extensions = [".{language}"]
"#,
            version = crate::protocol::types::CURRENT_PROTOCOL_VERSION,
        )
    }

    /// GM-306's regression test, stated directly: on a machine where two
    /// roots both offer the *same* language - exactly the installed-root-
    /// beside-the-executable-plus-checkout-root shape [`default_roots`]'s
    /// doc comment now explains is the common case, not the rare one - that
    /// language must be listed once, not once per root. Before GM-306,
    /// `list_from_roots` had no deduplication at all, so this reproduces
    /// "every bundled plugin printed twice" (the actual bug, observed on a
    /// real installed archive) at the smallest scale that still exercises
    /// the real code path: two real roots, one real language, `Bundled`
    /// status both times exactly as `bundled_roots()` tags them.
    #[test]
    fn a_language_offered_by_two_roots_is_listed_once_not_twice() {
        let (_root_a, root_a) = fixture_root(&[("typescript", &plugin_toml("typescript", "2.2.0"))]);
        let (_root_b, root_b) = fixture_root(&[("typescript", &plugin_toml("typescript", "2.2.0"))]);

        let plugins =
            list_from_roots(&[(root_a, PluginStatus::Bundled), (root_b, PluginStatus::Bundled)]).unwrap();

        assert_eq!(
            plugins.len(),
            1,
            "typescript is offered by both roots and must be listed once, not once per root: {plugins:?}"
        );
    }

    /// This task's other acceptance criterion: which root wins when two
    /// roots disagree - about a plugin's version, the case a checkout root
    /// and an installed root actually hit in practice (see the module doc
    /// comment) - is a decided, tested behavior, not an accident of
    /// iteration order. The decision: the *earlier*, higher-precedence root
    /// wins, matching `daemon::manifest::discover`'s own shadow rule (see
    /// [`list_from_roots`]'s doc comment) - so what a user reads from
    /// `plugins list` always names the version the daemon would actually
    /// spawn from these same roots, never the shadowed one.
    #[test]
    fn when_two_roots_disagree_about_a_plugin_version_the_earlier_root_wins() {
        let (_first, first_root) = fixture_root(&[("python", &plugin_toml("python", "1.0.0"))]);
        let (_second, second_root) = fixture_root(&[("python", &plugin_toml("python", "2.0.0"))]);

        let plugins =
            list_from_roots(&[(first_root, PluginStatus::Installed), (second_root, PluginStatus::Bundled)])
                .unwrap();

        assert_eq!(plugins.len(), 1, "{plugins:?}");
        assert_eq!(
            plugins[0].outcome,
            PluginOutcome::Loaded {
                version: "1.0.0".to_string(),
                status: PluginStatus::Installed,
                capabilities: Capabilities::default(),
            },
            "the earlier root (first_root, here tagged Installed) must win over the later \
             root's disagreeing version - the later root's 2.0.0 must not appear at all"
        );
    }

    #[test]
    fn render_reports_language_version_and_status_for_a_loaded_plugin() {
        let plugins = vec![PluginInfo {
            language: "typescript".to_string(),
            outcome: PluginOutcome::Loaded {
                version: "2.0.0".to_string(),
                status: PluginStatus::Bundled,
                capabilities: Capabilities::default(),
            },
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
            outcome: PluginOutcome::Loaded {
                version: "0.1.0".to_string(),
                status: PluginStatus::Installed,
                capabilities: Capabilities::default(),
            },
        }];

        let rendered = render(&plugins);

        assert!(rendered.contains("installed"), "{rendered}");
    }

    /// The acceptance criterion for this task's `g-mesh plugins list` change:
    /// capabilities are visible in the rendered output, not just carried on
    /// the struct. Uses a non-default `Capabilities` value on purpose - a
    /// render test built entirely from `Capabilities::default()` would still
    /// pass if `render_capabilities` silently ignored its argument and
    /// printed the defaults every time.
    #[test]
    fn render_shows_capabilities_for_a_loaded_plugin() {
        let plugins = vec![PluginInfo {
            language: "go".to_string(),
            outcome: PluginOutcome::Loaded {
                version: "0.1.0".to_string(),
                status: PluginStatus::Bundled,
                capabilities: Capabilities {
                    semantic_pass: true,
                    receiver_calls: manifest::ReceiverCallResolution::Resolved,
                    receiver_calls_structural: manifest::ReceiverCallResolution::Unresolved,
                },
            },
        }];

        let rendered = render(&plugins);

        assert!(rendered.contains("semantic_pass=yes"), "{rendered}");
        assert!(rendered.contains("receiver_calls=resolved"), "{rendered}");
        assert!(rendered.contains("receiver_calls_structural=unresolved"), "{rendered}");
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
