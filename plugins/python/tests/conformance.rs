//! The Python plugin's own acceptance test: run through the real `g-mesh
//! plugins check`, the same way `plugins/rust`'s `tests/conformance.rs`
//! proves the Rust plugin and `plugins/sdk`'s own toy plugin proves the SDK.
//!
//! # Why two checks skip
//!
//! `id-stability.declaration-edit-applies` needs a non-`File`,
//! non-placeholder node to edit and re-diff, and this task's `File`-only
//! stub (`crate::extractor`'s module doc) emits none - the identical skip
//! `plugins/rust/tests/conformance.rs` documents for its own pre-GM-286
//! state. `capabilities.semantic-engine-lazy` only applies when
//! `[plugin.capabilities] semantic_pass = true`; this plugin's manifest says
//! `false` (no semantic tier exists yet), so its counterpart,
//! `capabilities.semantic-pass-undeclared`, is the one that runs instead.
//! Everything else is asserted `Pass` by name, so that a check which starts
//! *skipping* (rather than genuinely not applying) fails this test instead
//! of passing it by omission.

use g_mesh_plugin_sdk::testing::{PluginCheck, Verdict};

/// Every check `g-mesh plugins check` reports, in no particular order -
/// asserted as a set so a check dropping out of the report (this crate's own
/// regression, not a plugin defect) fails loudly rather than shrinking the
/// loop below silently.
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

/// Everything but the two documented skips above.
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
        "python",
        env!("CARGO_BIN_EXE_g-mesh-plugin-python"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/conformance/project"),
    )
    .extensions(&[".py", ".pyi"])
    .exclude_dirs(&[".venv", "venv", "__pycache__", ".tox", ".mypy_cache", "site-packages"])
    .watch_files(&["pyproject.toml", "setup.cfg", "setup.py"])
    .entry_points(&["__init__"])
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

    let mut skipped = outcome.skipped();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        vec!["capabilities.semantic-engine-lazy", "id-stability.declaration-edit-applies"],
        "exactly these two must skip, and nothing else:\n{}",
        outcome.stdout
    );
}
