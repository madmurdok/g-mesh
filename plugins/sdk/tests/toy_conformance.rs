//! The SDK's acceptance test: a plugin built on it, run through the real
//! `g-mesh plugins check`.
//!
//! # Why this is the test that matters
//!
//! Every other test in this crate checks a piece of the SDK against what the
//! SDK's author believed the contract to be. This one checks a whole plugin
//! against what core actually enforces - the same binary, the same fifteen
//! checks, the same fixture-copy-and-edit session a bundled plugin goes
//! through in CI. A belief about `fileChanged` that is wrong shows up here and
//! nowhere else.
//!
//! It also proves the other half of `testing::PluginCheck`: the helper is
//! used here exactly the way a real plugin crate's own test will use it, so
//! "a plugin crate can run the kit from a `#[test]`" is demonstrated rather
//! than documented.
//!
//! # What the fixture contains, and why each piece
//!
//! `tests/fixtures/toy-project/`:
//!
//! - `a.toy` - the file the kit picks to edit (it has the most nodes): a
//!   declaration at the top for the declaration-edit step to move, a call
//!   onto a declaration further down the file (a same-file resolved edge), a
//!   cross-file `use` (a placeholder, and an unresolved edge onto it), and an
//!   open site for the semantic tier.
//! - `b.toy` - what `a.toy`'s `use` and its open site are waiting for, so
//!   both the linker and the semantic pass have something real to find.
//! - `sub/c.toy` - a nested directory, so the walk is a walk.
//! - `vendor/skipped.toy` and `ignored.toy` + `.gitignore` - the two
//!   exclusion mechanisms, present so a walk that lost one is visible in the
//!   report's node counts rather than invisible. The fixture emits ten nodes;
//!   a walk that ignored `exclude_dirs` would emit twelve, and one that
//!   ignored `.gitignore` twelve as well.
//!
//! `ignored.toy` is committed with `git add -f`, because the fixture's own
//! `.gitignore` is a real `.gitignore` and git obeys it too. Without the
//! force-add the file would simply not exist in a fresh checkout, and the
//! gitignore half of the walk's policy would be "tested" against a file that
//! was not there - a check that cannot fail.

use g_mesh_plugin_sdk::testing::{PluginCheck, Verdict};

/// Every check `g-mesh plugins check` reports, in report order. Asserted in
/// full so that a check silently dropping out of the report fails this test
/// rather than passing every "and this one passed" assertion vacuously - the
/// same guard core's own `tests/plugin_check.rs` keeps.
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

fn check() -> PluginCheck {
    PluginCheck::new(
        "toy",
        env!("CARGO_BIN_EXE_g-mesh-plugin-toy"),
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/toy-project"),
    )
    .extensions(&[".toy"])
    .exclude_dirs(&["vendor"])
    .semantic_pass(true)
}

#[test]
fn the_toy_plugin_passes_every_conformance_check() {
    let outcome = check().run().expect("the conformance kit could not be run");
    outcome.assert_conformant();

    let mut reported: Vec<&str> = outcome.outcomes.keys().map(String::as_str).collect();
    reported.sort_unstable();
    let mut expected = ALL_CHECKS.to_vec();
    expected.sort_unstable();
    assert_eq!(reported, expected, "the report must carry every check exactly once:\n{}", outcome.stdout);

    // The checks that are the SDK's own guarantees, named one by one: a
    // blanket "nothing failed" would be satisfied by a run in which they all
    // skipped.
    for id in [
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
    ] {
        assert_eq!(outcome.verdict(id), Some(Verdict::Pass), "{id} did not pass:\n{}", outcome.stdout);
    }

    // The lazy-engine check is the one the SDK exists to make *provable*: a
    // plugin that never wrote the marker would be reported "not
    // instrumented" and skipped, which is not a pass. Every SDK plugin writes
    // it, so this is a real verdict.
    assert_eq!(
        outcome.verdict("capabilities.semantic-engine-lazy"),
        Some(Verdict::Pass),
        "the semantic engine must have started, and only on the first semanticPass:\n{}",
        outcome.stdout
    );
    // ...and its counterpart does not apply to a plugin that declares the
    // capability, which is the only check expected to skip here.
    assert_eq!(
        outcome.skipped(),
        vec!["capabilities.semantic-pass-undeclared"],
        "nothing else may skip:\n{}",
        outcome.stdout
    );
}

/// The same plugin with `semantic_pass = false` in its manifest: core must
/// never send it a `semanticPass`, and it must never start an engine on its
/// own. It is the discriminating half of the check above - if the SDK started
/// the engine for structural work, this run would fail
/// `capabilities.semantic-pass-undeclared` on the marker alone.
#[test]
fn a_plugin_that_does_not_declare_a_semantic_pass_never_starts_its_engine() {
    let outcome = check().semantic_pass(false).run().expect("the conformance kit could not be run");
    outcome.assert_conformant();
    assert_eq!(
        outcome.verdict("capabilities.semantic-pass-undeclared"),
        Some(Verdict::Pass),
        "no semanticPass may be sent, and no marker written:\n{}",
        outcome.stdout
    );
    assert_eq!(outcome.verdict("capabilities.semantic-engine-lazy"), Some(Verdict::Skip));
}
