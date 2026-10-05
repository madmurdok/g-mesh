//! The TypeScript plugin's acceptance test: the Rust binary run through the
//! real `g-mesh plugins check`, against `conformance/project` and
//! `conformance/expect.toml`, the way `plugins/python/tests/conformance.rs`
//! proves the Python plugin.
//!
//! One configuration, the manifest this plugin ships: `semantic_pass = false`,
//! so core never sends a `semanticPass` and the structural tier answers alone.
//! The expectations run with `--skip-semantic-expectations`: every entry
//! tagged `tier = "semantic"` reports `Skip`, and every other entry passes.
//! The semantic arm returns with the language-server tier.
//!
//! `g-mesh` itself is found as the kit finds it: `G_MESH_BIN` if set, else the
//! binary beside this test's own target directory (`cargo build -p g-mesh`).

use g_mesh_plugin_sdk::testing::{CheckOutcome, PluginCheck, Verdict};
use g_mesh_plugin_typescript::extractor::grammar::EXTENSIONS;
use g_mesh_plugin_typescript::project::{EXCLUDE_DIRS, WATCH_FILES};

const EXPECT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/expect.toml");

/// Every check `g-mesh plugins check` reports outside `expectations.*`.
const ALL_CHECKS: [&str; 15] = [
    "session",
    "shape",
    "stream-order",
    "same-file-rule",
    "id-stability.bulk-repeat",
    "id-stability.whitespace-edit",
    "id-stability.deletes-known",
    "id-stability.incremental-matches-bulk",
    "id-stability.declaration-edit-applies",
    "ownership.defines-exports-from-file",
    "ownership.language",
    "ownership.no-container",
    "ownership.diff-stays-in-file",
    "capabilities.semantic-pass-undeclared",
    "capabilities.semantic-engine-lazy",
];

/// The capability check that does not apply to a manifest without a semantic
/// tier, and so must report `Skip`.
const NOT_APPLICABLE: &str = "capabilities.semantic-engine-lazy";

/// The shipped manifest's query tables (`non_symbol_queries`,
/// `symbol_query_prefixes`, `reexports`), re-serialized from `plugin.toml`
/// rather than copied, because the kit's generated manifest carries none of
/// them and `[[definition]] symbol = "@Component"` needs the `@` strip.
fn query_tables() -> String {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))
        .expect("plugins/typescript/plugin.toml is readable");
    let parsed: toml::Value = toml::from_str(&text).expect("plugins/typescript/plugin.toml parses");
    let mut plugin = toml::map::Map::new();
    for table in ["non_symbol_queries", "symbol_query_prefixes", "reexports"] {
        plugin.insert(table.to_string(), parsed["plugin"][table].clone());
    }
    let mut root = toml::map::Map::new();
    root.insert("plugin".to_string(), toml::Value::Table(plugin));
    toml::to_string(&toml::Value::Table(root)).expect("the tables re-serialize")
}

/// The shipped manifest's structural configuration, from the constants the
/// manifest is pinned to (tests/units.rs), so a run checks what users get.
fn structural() -> PluginCheck {
    PluginCheck::new(
        "typescript",
        env!("CARGO_BIN_EXE_g-mesh-plugin-typescript"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/project"),
    )
    .extensions(&EXTENSIONS)
    .exclude_dirs(&EXCLUDE_DIRS)
    .watch_files(&WATCH_FILES)
    .entry_points(&["index"])
    .semantic_pass(false)
    .receiver_calls("unresolved", "unresolved")
    .manifest_extra(query_tables())
}

/// `(entries, entries tagged tier = "semantic")` in [`EXPECT`], parsed rather
/// than grepped: the tag also appears in the file's comments.
fn count_expectations() -> (usize, usize) {
    let text = std::fs::read_to_string(EXPECT).expect("conformance/expect.toml is readable");
    let parsed: toml::Value = toml::from_str(&text).expect("conformance/expect.toml parses");
    let mut total = 0;
    let mut semantic = 0;
    for value in parsed.as_table().expect("expect.toml is a table").values() {
        let Some(entries) = value.as_array() else { continue };
        for entry in entries {
            total += 1;
            if entry.get("tier").and_then(toml::Value::as_str) == Some("semantic") {
                semantic += 1;
            }
        }
    }
    (total, semantic)
}

fn expectation_verdicts(outcome: &CheckOutcome) -> Vec<(&str, Verdict)> {
    outcome
        .outcomes
        .iter()
        .filter(|(id, _)| id.starts_with("expectations."))
        .map(|(id, verdict)| (id.as_str(), *verdict))
        .collect()
}

fn verdicts_of(judged: &[(&str, Verdict)], want: Verdict) -> usize {
    judged.iter().filter(|(_, verdict)| *verdict == want).count()
}

#[test]
fn the_structural_tier_passes_every_check_and_every_structural_expectation() {
    let outcome = structural()
        .expect(EXPECT)
        .skip_semantic_expectations(true)
        .run()
        .expect("the conformance kit could not be run");
    assert!(outcome.failures().is_empty(), "nothing may fail:\n{}", outcome.stdout);

    let mut reported: Vec<&str> = outcome.outcomes.keys().map(String::as_str).collect();
    reported.retain(|id| !id.starts_with("expectations."));
    reported.sort_unstable();
    let mut expected = ALL_CHECKS.to_vec();
    expected.sort_unstable();
    assert_eq!(reported, expected, "the report must carry every check exactly once:\n{}", outcome.stdout);
    for id in ALL_CHECKS.iter().filter(|id| **id != NOT_APPLICABLE) {
        assert_eq!(outcome.verdict(id), Some(Verdict::Pass), "{id} did not pass:\n{}", outcome.stdout);
    }
    assert_eq!(
        outcome.verdict(NOT_APPLICABLE),
        Some(Verdict::Skip),
        "{NOT_APPLICABLE} does not apply without a semantic tier:\n{}",
        outcome.stdout
    );

    // `expectations.file` plus one verdict per entry: a skipped section would
    // report none, and `assert_conformant` says nothing about a Skip.
    let (entries, semantic) = count_expectations();
    let judged = expectation_verdicts(&outcome);
    assert_eq!(judged.len(), entries + 1, "every expectation must be reported:\n{}", outcome.stdout);
    assert_eq!(
        verdicts_of(&judged, Verdict::Skip),
        semantic,
        "exactly the semantic entries skip:\n{}",
        outcome.stdout
    );
    assert_eq!(
        verdicts_of(&judged, Verdict::Pass),
        entries - semantic + 1,
        "the file plus every structural entry passes:\n{}",
        outcome.stdout
    );
}
