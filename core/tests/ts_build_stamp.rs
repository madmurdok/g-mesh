//! `core/build.rs` skips `npm run build` when the JS/TS plugin's inputs are
//! unchanged since its last successful build.
//!
//! The decision lives in `core/ts_build_stamp.rs`, included here exactly as
//! the build script includes it, and is exercised against a scratch plugin
//! directory shaped like `plugins/typescript`: the inputs the build script
//! declares, a built entry point, and a `node_modules/` for the stamp. No
//! `npm` runs: what is under test is whether one would.

use std::fs;
use std::path::Path;

include!("../ts_build_stamp.rs");

/// A plugin directory that has just been built: inputs, `dist/`, and the
/// stamp `core/build.rs` records after a successful `npm run build`.
fn built_plugin() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create a scratch plugin directory");
    let root = dir.path();
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(root.join("src/index.ts"), "export const a = 1;\n").unwrap();
    fs::write(root.join("src/nested/extract.ts"), "export const b = 2;\n").unwrap();
    fs::write(root.join("package.json"), "{\"version\": \"1.0.0\"}\n").unwrap();
    fs::write(root.join("tsconfig.json"), "{}\n").unwrap();
    fs::create_dir_all(root.join("dist/src")).unwrap();
    fs::write(root.join(TS_BUILD_ENTRY), "exports.a = 1;\n").unwrap();
    fs::create_dir_all(root.join("node_modules")).unwrap();
    assert!(ts_build_needed(root), "a directory with no stamp yet must build");
    ts_record_build(root).expect("the stamp can be written");
    dir
}

fn stamp(root: &Path) -> String {
    fs::read_to_string(root.join(TS_BUILD_STAMP)).expect("a successful build leaves a stamp")
}

/// Control: make `ts_build_needed` return `true` unconditionally and this
/// fails - the skip never happens.
#[test]
fn unchanged_inputs_are_not_rebuilt() {
    let plugin = built_plugin();
    assert!(!ts_build_needed(plugin.path()), "nothing changed since the recorded build");
}

/// Content, not mtimes: rewriting every input with the same bytes (what a
/// checkout or an archive extraction does) is still "unchanged".
///
/// Control: digest mtimes (or anything but the bytes) and this fails.
#[test]
fn rewriting_an_input_with_the_same_bytes_is_not_a_change() {
    let plugin = built_plugin();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(plugin.path().join("src/index.ts"), "export const a = 1;\n").unwrap();
    fs::write(plugin.path().join("tsconfig.json"), "{}\n").unwrap();
    assert!(!ts_build_needed(plugin.path()));
}

/// A content change under `src/` rebuilds, and the build records a new
/// stamp, which then skips again.
///
/// Control: drop file bytes from `ts_inputs_digest` (hash paths only), or
/// leave `src` out of `TS_BUILD_INPUTS`, and the first assertion fails.
#[test]
fn a_content_change_in_src_rebuilds_and_changes_the_stamp() {
    let plugin = built_plugin();
    let before = stamp(plugin.path());

    fs::write(plugin.path().join("src/nested/extract.ts"), "export const b = 3;\n").unwrap();
    assert!(ts_build_needed(plugin.path()), "a changed source must rebuild");

    ts_forget_build(plugin.path());
    ts_record_build(plugin.path()).unwrap();
    let after = stamp(plugin.path());
    assert_ne!(before, after, "the stamp must record the new inputs");
    assert!(!ts_build_needed(plugin.path()), "and the build it records is current");
}

/// Each declared top-level input counts, and so does a file added or
/// removed under `src/`.
///
/// Control: remove an entry from `TS_BUILD_INPUTS` and its case fails.
#[test]
fn every_declared_input_and_every_added_or_removed_file_counts() {
    for (what, change) in [
        (
            "package.json",
            &(|root: &Path| fs::write(root.join("package.json"), "{\"version\": \"1.0.1\"}\n").unwrap())
                as &dyn Fn(&Path),
        ),
        ("tsconfig.json", &|root: &Path| {
            fs::write(root.join("tsconfig.json"), "{\"strict\": true}\n").unwrap()
        }),
        ("a new file", &|root: &Path| fs::write(root.join("src/added.ts"), "").unwrap()),
        ("a removed file", &|root: &Path| fs::remove_file(root.join("src/nested/extract.ts")).unwrap()),
        ("a removed input", &|root: &Path| fs::remove_file(root.join("tsconfig.json")).unwrap()),
    ] {
        let plugin = built_plugin();
        change(plugin.path());
        assert!(ts_build_needed(plugin.path()), "{what} must rebuild");
    }
}

/// A missing `dist/` rebuilds whatever the stamp says.
///
/// Control: drop the `TS_BUILD_ENTRY` check from `ts_build_needed` and this
/// fails.
#[test]
fn a_missing_dist_rebuilds() {
    let plugin = built_plugin();
    fs::remove_dir_all(plugin.path().join("dist")).unwrap();
    assert!(ts_build_needed(plugin.path()));
}

/// A build that is about to run forgets the old stamp first, so one that
/// fails part-way cannot leave a stamp vouching for what it half-emitted.
///
/// Control: make `ts_forget_build` a no-op and this fails.
#[test]
fn a_build_that_starts_forgets_the_previous_stamp() {
    let plugin = built_plugin();
    ts_forget_build(plugin.path());
    assert!(ts_build_needed(plugin.path()));
}

/// The digest names files relative to the plugin directory with `/`, so two
/// checkouts at different absolute paths agree.
///
/// Control: digest absolute paths and this fails.
#[test]
fn the_digest_does_not_depend_on_where_the_checkout_is() {
    let first = built_plugin();
    let second = built_plugin();
    assert_eq!(ts_inputs_digest(first.path()).unwrap(), ts_inputs_digest(second.path()).unwrap());
}

/// The one list `core/build.rs` both watches and digests is the one the
/// real plugin builds from.
#[test]
fn the_declared_inputs_exist_in_the_real_plugin() {
    let plugin = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../plugins/typescript"));
    for input in TS_BUILD_INPUTS {
        assert!(plugin.join(input).exists(), "{input} is declared but not in plugins/typescript");
    }
}
