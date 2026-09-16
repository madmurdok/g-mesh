//! The Rust plugin's own acceptance test: run through the real `g-mesh
//! plugins check`, the same way `plugins/sdk`'s own toy plugin proves the
//! SDK and `plugins/typescript` proves itself in core's CI.
//!
//! # Why exactly one check skips
//!
//! `capabilities.semantic-engine-lazy` only applies when
//! `[plugin.capabilities] semantic_pass = true`. This plugin's manifest says
//! `false` - the rust-analyzer tier is GM-290, R4 in the design doc's
//! rollout - so its counterpart, `capabilities.semantic-pass-undeclared`, is
//! the one that runs instead. Everything else is asserted `Pass` by name,
//! one by one, so that a check which silently starts *skipping* (rather than
//! genuinely not applying) fails this test instead of passing it by
//! omission.
//!
//! Until GM-286 that list had a second entry:
//! `id-stability.declaration-edit-applies` needs a non-`File`,
//! non-placeholder node to edit and re-diff, and the project-model-only
//! plugin emitted none. It is a real `PASS` now, which is the narrow,
//! concrete sense in which this task made the plugin's diff path testable at
//! all.

use g_mesh_plugin_sdk::testing::{PluginCheck, Verdict};

/// Every check `g-mesh plugins check` reports, in no particular order -
/// asserted as a set so a check dropping out of the report (this crate's
/// own regression, not a plugin defect) fails loudly rather than shrinking
/// the loop below silently.
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

/// Everything but `capabilities.semantic-engine-lazy` - see this file's own
/// module doc.
const MUST_PASS: [&str; 14] = [
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
];

fn check() -> PluginCheck {
    PluginCheck::new(
        "rust",
        env!("CARGO_BIN_EXE_g-mesh-plugin-rust"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/project"),
    )
    .extensions(&[".rs"])
    .exclude_dirs(&["target"])
    .watch_files(&["Cargo.toml"])
    .entry_points(&["lib.rs", "main.rs", "mod.rs"])
    .semantic_pass(false)
}

#[test]
fn the_plugin_passes_every_check_that_applies_to_it() {
    let outcome = check().run().expect("the conformance kit could not be run");
    outcome.assert_conformant();

    let mut reported: Vec<&str> = outcome.outcomes.keys().map(String::as_str).collect();
    reported.sort_unstable();
    let mut expected = ALL_CHECKS.to_vec();
    expected.sort_unstable();
    assert_eq!(reported, expected, "the report must carry every check exactly once:\n{}", outcome.stdout);

    for id in MUST_PASS {
        assert_eq!(outcome.verdict(id), Some(Verdict::Pass), "{id} did not pass:\n{}", outcome.stdout);
    }

    assert_eq!(
        outcome.skipped(),
        vec!["capabilities.semantic-engine-lazy"],
        "exactly this one must skip, and nothing else:\n{}",
        outcome.stdout
    );
}

/// The acceptance criteria, as assertions against the linked index: see
/// `conformance/expect.toml`, which says what each one proves - GM-287's
/// full file, not GM-286's original five-entry floor.
#[test]
fn the_linked_index_answers_the_acceptance_criteria() {
    let outcome = check()
        .expect(concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/expect.toml"))
        .run()
        .expect("the conformance kit could not be run");
    outcome.assert_conformant();

    // `assert_conformant` fails on a FAIL and says nothing about a SKIP, and
    // the whole expectations section is skipped when the session did not
    // reach a state worth judging. So each entry is asserted to have been
    // *judged*: `expectations.file` (the file parsed at all) plus the eleven
    // entries `conformance/expect.toml` now carries, each reported under its
    // own `expectations.<kind>[<index>]` id.
    let judged: Vec<(&str, Verdict)> = outcome
        .outcomes
        .iter()
        .filter(|(id, _)| id.starts_with("expectations."))
        .map(|(id, verdict)| (id.as_str(), *verdict))
        .collect();
    assert_eq!(judged.len(), 12, "every expectation must be reported:\n{}", outcome.stdout);
    for (id, verdict) in judged {
        assert_eq!(verdict, Verdict::Pass, "{id} was not judged and passed:\n{}", outcome.stdout);
    }
}
