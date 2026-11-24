//! The coverage fixtures over the real bundled manifests, shared by this
//! module's tests and the daemon's tests that render a real walk's outcome.

use std::collections::HashMap;
use std::path::Path;

use super::{build, error_cause, present_languages, Coverage, PresentLanguage, ERROR_BYTES};
use crate::languages;
use crate::plugin_manifest::{discover, Capabilities, DiscoveredPlugins};
use crate::storage::language_outcome::LanguageOutcome;

pub(crate) use super::build as build_instructions;

/// The byte cap on a failed language's cause.
pub(crate) const MAX_CAUSE_BYTES: usize = ERROR_BYTES;

/// The cause a failed language's item shows for `error`.
pub(crate) fn cause_of(error: &str) -> String {
    error_cause(error)
}

/// The bundled plugins as `discover` finds them under `plugins/`, kept to
/// `keep`: the other bundled languages are then exactly as missing as on a
/// machine without their plugin.
pub(crate) fn real_plugins(keep: &[&str]) -> DiscoveredPlugins {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let mut found = discover(&[root]).expect("the bundled plugins must be discoverable");
    for language in keep {
        assert!(found.manifests.contains_key(*language), "no bundled manifest for {language}");
    }
    found.manifests.retain(|language, _| keep.contains(&language.as_str()));
    found.routing.retain(|_, language| keep.contains(&language.as_str()));
    found
}

/// Each discovered language's manifest capabilities, as
/// `PluginRegistry::receiver_call_capabilities` hands them to the server.
pub(crate) fn real_capabilities(found: &DiscoveredPlugins) -> HashMap<String, Capabilities> {
    found.manifests.iter().map(|(language, manifest)| (language.clone(), manifest.capabilities)).collect()
}

pub(crate) fn real_present(found: &DiscoveredPlugins, languages: &[&str]) -> Vec<PresentLanguage> {
    present_languages(languages.iter().map(|language| language.to_string()), &real_capabilities(found))
}

/// `languages::missing` over `found`, as `PluginRegistry::missing_languages`
/// returns it.
pub(crate) fn real_missing(found: &DiscoveredPlugins) -> Vec<String> {
    languages::missing(found).iter().map(|entry| entry.language.to_string()).collect()
}

/// `outcomes` sorted by language, as `schema::language_outcomes` returns them.
pub(crate) fn sorted(outcomes: Vec<(&str, LanguageOutcome)>) -> Vec<(String, LanguageOutcome)> {
    let mut outcomes: Vec<(String, LanguageOutcome)> =
        outcomes.into_iter().map(|(language, outcome)| (language.to_string(), outcome)).collect();
    outcomes.sort_by(|a, b| a.0.cmp(&b.0));
    outcomes
}

pub(crate) fn indexed() -> LanguageOutcome {
    LanguageOutcome::Indexed { files: 10 }
}

pub(crate) fn failed(error: &str) -> LanguageOutcome {
    LanguageOutcome::Failed { error: error.to_string() }
}

/// A warm coverage as `GMeshMcpServer::instructions` assembles it:
/// `indexed` languages paired with `found`'s manifests, the recorded
/// `outcomes` with no retry state (as for `Phase::Failed`), and `found`'s
/// missing catalogue languages.
pub(crate) fn warm_real(
    found: &DiscoveredPlugins,
    indexed: &[&str],
    outcomes: Vec<(&str, LanguageOutcome)>,
) -> Coverage {
    Coverage::from_outcomes(real_present(found, indexed), sorted(outcomes), None, real_missing(found))
}

/// The failed item `{lang} ({cause})` for `language` out of `rendered`.
pub(crate) fn failed_item<'a>(rendered: &'a str, language: &str) -> &'a str {
    let start = rendered
        .find(&format!("plugin failed: {language} ("))
        .unwrap_or_else(|| panic!("no failed item for {language}: {rendered}"));
    let item = &rendered[start + "plugin failed: ".len()..];
    let end = item.find(") - fix the plugin").unwrap_or_else(|| panic!("unterminated item: {rendered}"));
    &item[..=end]
}

/// `rendered` for the four bundled languages with `rust` failed with
/// `error` and the other three indexed.
pub(crate) fn rendered_with_rust_failed(error: &str) -> String {
    let found = real_plugins(&["go", "python", "rust", "typescript"]);
    build(&warm_real(
        &found,
        &["go", "python", "typescript"],
        vec![("go", indexed()), ("python", indexed()), ("rust", failed(error)), ("typescript", indexed())],
    ))
}
