//! The Rust plugin's own acceptance test: run through the real `g-mesh
//! plugins check`, the same way `plugins/sdk`'s own toy plugin proves the
//! SDK and `plugins/typescript` proves itself in core's CI.
//!
//! # Why two checks legitimately skip
//!
//! This crate is presently a **project-model-only** plugin (GM-285):
//! `extract` emits a file's own `File` node and nothing else - see
//! `src/extractor.rs`'s module doc for why, and GM-286 for the tree-sitter
//! extractor that replaces it. Two checks in the kit's fifteen depend on a
//! *declaration* existing to have something to say:
//!
//! - `id-stability.declaration-edit-applies` needs a non-`File`,
//!   non-placeholder node to edit and re-diff. A File-only plugin never
//!   emits one, so the kit reports "not reached", not a pass it did not earn.
//! - `capabilities.semantic-engine-lazy` only applies when
//!   `[plugin.capabilities] semantic_pass = true`; this plugin's manifest
//!   says `false` (no semantic tier yet), so its counterpart,
//!   `capabilities.semantic-pass-undeclared`, is the one that runs instead.
//!
//! Every other check - `shape`, `stream-order`, `same-file-rule`, every
//! `ownership.*` rule, and the three `id-stability.*` checks a File node
//! alone can satisfy (`bulk-repeat`, `whitespace-edit`,
//! `incremental-matches-bulk`, `deletes-known`) - is asserted `Pass`
//! explicitly, one by one, so a check silently starting to skip (rather than
//! genuinely not applying) fails this test instead of passing it by omission.

use g_mesh_plugin_sdk::testing::{PluginCheck, Verdict};

/// Every check `g-mesh plugins check` reports, in no particular order -
/// asserted as a set so a check dropping out of the report (this crate's
/// own regression, not a plugin defect) fails loudly rather than shrinking
/// the "everything but these two passed" loop below silently.
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

/// The checks a File-only plugin can, and must, pass outright.
const MUST_PASS: [&str; 13] = [
    "session",
    "shape",
    "stream-order",
    "same-file-rule",
    "id-stability.bulk-repeat",
    "id-stability.whitespace-edit",
    "id-stability.deletes-known",
    "id-stability.incremental-matches-bulk",
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
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/workspace"),
    )
    .extensions(&[".rs"])
    .exclude_dirs(&["target"])
    .watch_files(&["Cargo.toml"])
    .entry_points(&["lib.rs", "main.rs", "mod.rs"])
    .semantic_pass(false)
}

#[test]
fn the_file_only_stub_passes_every_check_that_applies_to_it() {
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

    // The two checks a File-only plugin genuinely cannot exercise - see this
    // file's own module doc.
    let mut skipped = outcome.skipped();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        vec!["capabilities.semantic-engine-lazy", "id-stability.declaration-edit-applies"],
        "exactly these two must skip, and nothing else:\n{}",
        outcome.stdout
    );
}
