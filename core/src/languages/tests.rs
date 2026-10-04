use super::*;
use crate::daemon::manifest::discover;
use crate::daemon::manifest::tests::{discovery_root, manifest_toml};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Discovery over one fresh root holding `plugins` (`(language, extensions)`
/// pairs), through the real `discover()`, so routing and manifests are built
/// exactly as the daemon builds them.
fn discovered(plugins: &[(&str, &[&str])]) -> DiscoveredPlugins {
    let bodies: Vec<(String, String)> = plugins
        .iter()
        .map(|(language, extensions)| (language.to_string(), manifest_toml(language, "0.0.1", extensions)))
        .collect();
    let pairs: Vec<(&str, &str)> = bodies.iter().map(|(dir, body)| (dir.as_str(), body.as_str())).collect();
    let (_guard, root) = discovery_root(&pairs);
    discover(&[root]).expect("fixture plugins must be discoverable")
}

fn languages(entries: &[&CatalogueEntry]) -> Vec<&'static str> {
    entries.iter().map(|entry| entry.language).collect()
}

/// language -> sorted extensions, read from every real
/// `plugins/<dir>/plugin.toml` in the checkout.
fn real_manifest_extensions() -> BTreeMap<String, Vec<String>> {
    let plugins_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let mut found = BTreeMap::new();
    for dir in std::fs::read_dir(&plugins_root).unwrap() {
        let manifest_path = dir.unwrap().path().join("plugin.toml");
        if !manifest_path.is_file() {
            continue;
        }
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let plugin = &value["plugin"];
        let language = plugin["language"].as_str().unwrap().to_string();
        let mut extensions: Vec<String> = plugin["languages"]["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|ext| ext.as_str().unwrap().to_string())
            .collect();
        extensions.sort();
        found.insert(language, extensions);
    }
    found
}

#[test]
fn the_catalogue_names_exactly_the_four_bundled_languages_in_order() {
    let ids: Vec<&str> = CATALOGUE.iter().map(|entry| entry.language).collect();
    assert_eq!(ids, ["typescript", "python", "rust", "go"]);
}

/// The catalogue's extensions are copied from the plugins' own manifests; this
/// reads those manifests rather than a second hand-written list, so a plugin
/// that gains (or a new plugin that ships with) an extension the catalogue
/// lacks fails here.
#[test]
fn every_catalogue_entry_matches_its_real_plugin_manifest_extensions() {
    let catalogue: BTreeMap<String, Vec<String>> = CATALOGUE
        .iter()
        .map(|entry| {
            let mut extensions: Vec<String> = entry.extensions.iter().map(|ext| ext.to_string()).collect();
            extensions.sort();
            (entry.language.to_string(), extensions)
        })
        .collect();
    assert_eq!(catalogue, real_manifest_extensions());
}

#[test]
fn install_command_is_the_exact_plugins_install_command_for_each_language() {
    let commands: Vec<String> = CATALOGUE.iter().map(CatalogueEntry::install_command).collect();
    assert_eq!(
        commands,
        [
            "g-mesh plugins install typescript",
            "g-mesh plugins install python",
            "g-mesh plugins install rust",
            "g-mesh plugins install go",
        ]
    );
}

/// The catalogue carries no capability information, and this destructuring
/// is exhaustive so that adding any field to `CatalogueEntry` stops it
/// compiling. That is deliberate: capabilities belong to the plugin manifest
/// (`daemon::manifest::Capabilities`) alone. A capability here would be read
/// instead of the manifest's and drift from it, giving core two sources of
/// truth about a live plugin. If you are adding a field, the catalogue is the
/// wrong place for it unless it describes an *absent* plugin only.
#[test]
fn a_catalogue_entry_holds_only_a_language_and_its_extensions() {
    for entry in CATALOGUE {
        let CatalogueEntry { language, extensions } = *entry;
        assert!(!language.is_empty());
        assert!(!extensions.is_empty());
    }
}

#[test]
fn entry_finds_a_catalogued_language_by_id() {
    assert_eq!(entry("rust").map(|e| e.language), Some("rust"));
    assert_eq!(entry("go").map(|e| e.extensions), Some(&[".go"][..]));
}

#[test]
fn entry_is_none_for_an_uncatalogued_language() {
    assert_eq!(entry("kotlin"), None);
}

#[test]
fn entry_for_path_matches_the_extension_ignoring_case() {
    assert_eq!(entry_for_path("src/main.rs").map(|e| e.language), Some("rust"));
    assert_eq!(entry_for_path("src/App.TSX").map(|e| e.language), Some("typescript"));
    assert_eq!(entry_for_path("pkg/Stubs.PyI").map(|e| e.language), Some("python"));
}

#[test]
fn entry_for_path_is_none_with_no_extension_or_an_unknown_one() {
    assert_eq!(entry_for_path("Makefile"), None);
    assert_eq!(entry_for_path("src/lib.zig"), None);
}

/// Precedence, whole-set: a discovered manifest wins, so a language with one
/// is never reported missing - even when that manifest claims none of the
/// catalogue's extensions for it.
#[test]
fn missing_returns_only_languages_with_no_manifest_in_catalogue_order() {
    let found = discovered(&[("python", &[".py"]), ("rust", &[".something-else"])]);
    assert_eq!(languages(&missing(&found)), ["typescript", "go"]);
}

#[test]
fn missing_returns_the_whole_catalogue_when_nothing_is_discovered() {
    let found = discovered(&[]);
    assert_eq!(languages(&missing(&found)), ["typescript", "python", "rust", "go"]);
}

/// A non-catalogue plugin claiming a catalogued extension routes that file;
/// the catalogue must not then say python's plugin is what is missing.
#[test]
fn absent_for_path_is_none_when_any_manifest_claims_the_extension() {
    let found = discovered(&[("snake", &[".py"])]);
    assert_eq!(absent_for_path(&found, "app/main.py"), None);
}

/// The plugin is present but its manifest dropped `.pyi`: the manifest wins,
/// so the file is simply not routed, and the catalogue does not claim the
/// (present) python plugin is absent.
#[test]
fn absent_for_path_is_none_when_the_catalogue_language_has_a_manifest_that_no_longer_claims_it() {
    let found = discovered(&[("python", &[".py"])]);
    assert_eq!(absent_for_path(&found, "app/stubs.pyi"), None);
}

#[test]
fn absent_for_path_names_the_catalogue_entry_when_no_manifest_answers() {
    let found = discovered(&[("python", &[".py", ".pyi"])]);
    assert_eq!(absent_for_path(&found, "cmd/Main.GO").map(|e| e.language), Some("go"));
    assert_eq!(
        absent_for_path(&found, "cmd/main.go").map(CatalogueEntry::install_command),
        Some("g-mesh plugins install go".to_string())
    );
}

#[test]
fn absent_for_path_is_none_for_a_path_no_catalogue_entry_claims() {
    let found = discovered(&[]);
    assert_eq!(absent_for_path(&found, "Makefile"), None);
    assert_eq!(absent_for_path(&found, "src/lib.zig"), None);
}

/// AC4: adding a language is one entry. The lookups are the same
/// table-driven functions the public API delegates to; a fifth entry appended
/// to the real catalogue works through every one of them, with nothing else
/// written for it.
#[test]
fn a_fifth_language_works_through_every_lookup_as_one_more_entry() {
    let table: Vec<CatalogueEntry> = CATALOGUE
        .iter()
        .copied()
        .chain([CatalogueEntry { language: "zig", extensions: &[".zig"] }])
        .collect();

    assert_eq!(entry_in(&table, "zig").map(|e| e.language), Some("zig"));
    assert_eq!(entry_for_path_in(&table, "src/Main.ZIG").map(|e| e.language), Some("zig"));
    assert_eq!(table[4].install_command(), "g-mesh plugins install zig");

    let found = discovered(&[("python", &[".py", ".pyi"])]);
    assert_eq!(languages(&missing_in(&table, &found)), ["typescript", "rust", "go", "zig"]);
    assert_eq!(absent_for_path_in(&table, &found, "src/main.zig").map(|e| e.language), Some("zig"));

    // Once zig's plugin is discovered, the manifest wins for it too.
    let found = discovered(&[("zig", &[".zig"])]);
    assert!(!languages(&missing_in(&table, &found)).contains(&"zig"));
    assert_eq!(absent_for_path_in(&table, &found, "src/main.zig"), None);
}
