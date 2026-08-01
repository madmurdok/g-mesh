//! `g-mesh plugins list`: report the language plugins this install has,
//! globally rather than per-project - a plugin is a property of the g-mesh
//! install, not of any one indexed project.
//!
//! There is exactly one plugin to report today, and no discovery step: see
//! [`crate::daemon::plugin`]'s module doc for why the general
//! `~/.g-mesh/plugins/<language>/` discovery/manifest scheme from the v1
//! architecture doc is deliberately not built for this MVP release. Scanning
//! that directory now would just always come back empty, which is a worse
//! answer than the honest one - this command reports the one plugin that is
//! actually there, the same bundled JS/TS plugin
//! [`crate::daemon::plugin::plugin_entry_path`] spawns.

use std::fmt::Write as _;

use anyhow::{Context, Result};

/// Where a plugin's code comes from, as `plugins list` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginStatus {
    /// Ships inside this g-mesh install - there is no separate install step,
    /// and no version to manage independently of the core binary's own.
    Bundled,
}

impl PluginStatus {
    fn label(self) -> &'static str {
        match self {
            PluginStatus::Bundled => "bundled",
        }
    }
}

/// One installed language plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    pub language: String,
    pub version: String,
    pub status: PluginStatus,
}

/// The bundled JS/TS plugin's own `package.json`, embedded at compile time.
///
/// Same "resolve relative to this crate's own source tree" reasoning as
/// [`crate::daemon::plugin::plugin_entry_path`]: `core/` and `plugins/js-ts/`
/// are sibling directories in this repo and there is no distribution
/// pipeline yet, so a path baked in at compile time is the pragmatic MVP
/// answer. Reading the real `package.json` rather than hard-coding its
/// version string keeps this command from silently drifting out of date the
/// next time that plugin is bumped.
const JS_TS_PACKAGE_JSON: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../plugins/js-ts/package.json"));

/// Runs `g-mesh plugins list`.
pub fn run() -> Result<()> {
    print!("{}", render(&list()?));
    Ok(())
}

/// Every plugin this install has.
///
/// Just the one bundled JS/TS plugin today - see this module's doc comment
/// for why there is nothing to discover beyond it yet.
pub fn list() -> Result<Vec<PluginInfo>> {
    let package: serde_json::Value = serde_json::from_str(JS_TS_PACKAGE_JSON)
        .context("failed to parse the bundled JS/TS plugin's package.json")?;
    let version = package
        .get("version")
        .and_then(|value| value.as_str())
        .context("the bundled JS/TS plugin's package.json has no \"version\" field")?;

    Ok(vec![PluginInfo {
        language: "javascript/typescript".to_string(),
        version: version.to_string(),
        status: PluginStatus::Bundled,
    }])
}

/// Renders the plugin list the way `plugins list` prints it: one line per
/// plugin naming its language, version, and status.
pub fn render(plugins: &[PluginInfo]) -> String {
    let mut out = String::new();
    for plugin in plugins {
        let _ = writeln!(out, "{}  {}  {}", plugin.language, plugin.version, plugin.status.label());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The acceptance criterion this command exists to satisfy: with only
    /// the bundled JS/TS plugin present, `list` reports exactly that one
    /// entry, marked bundled.
    #[test]
    fn lists_exactly_the_one_bundled_js_ts_plugin() {
        let plugins = list().expect("the bundled plugin's package.json must parse");

        assert_eq!(plugins.len(), 1, "{plugins:?}");
        assert_eq!(plugins[0].language, "javascript/typescript");
        assert_eq!(plugins[0].status, PluginStatus::Bundled);
        assert!(!plugins[0].version.is_empty());
    }

    #[test]
    fn render_reports_language_version_and_bundled_status() {
        let plugins = vec![PluginInfo {
            language: "javascript/typescript".to_string(),
            version: "0.1.0".to_string(),
            status: PluginStatus::Bundled,
        }];

        let rendered = render(&plugins);

        assert!(rendered.contains("javascript/typescript"), "{rendered}");
        assert!(rendered.contains("0.1.0"), "{rendered}");
        assert!(rendered.contains("bundled"), "{rendered}");
    }

    #[test]
    fn render_of_no_plugins_is_empty() {
        assert_eq!(render(&[]), "");
    }
}
