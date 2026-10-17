//! The shape of `core/tests/` itself (GM-623).
//!
//! Core's integration tests are two binaries, `it` and `scripts`, each a
//! directory whose `main.rs` declares one module per file. Cargo makes two
//! silent mistakes possible in that layout, and this guard turns both loud:
//!
//! 1. A new top-level `core/tests/foo.rs` is auto-discovered as a binary of its
//!    own again: it works, but undoes the merge one file at a time (and every
//!    extra binary is one more ~25 MB link of the whole crate).
//! 2. A new `core/tests/it/foo.rs` without `mod foo;` in `it/main.rs` is never
//!    compiled at all, so its tests silently never run.
//!
//! Plain file reading, no build and no process: it runs in milliseconds.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The placement rule, quoted in every failure so the fix is in the message.
const RULE: &str = "a core integration test whose subject is a file under `scripts/` goes to \
                    `core/tests/scripts/`; every other one goes to `core/tests/it/`; either way \
                    it needs a `mod <file>;` line in that directory's `main.rs`";

/// The only entries `core/tests/` may hold at its top level.
const TOP_LEVEL_DIRS: [&str; 4] = ["fixtures", "it", "log_line", "scripts"];

/// The two binaries: directories with a `main.rs` that cargo builds as tests.
const BINARIES: [&str; 2] = ["it", "scripts"];

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .map(|entry| entry.expect("failed to read a directory entry").path())
        .collect();
    out.sort();
    out
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// The module names a `main.rs` declares: `mod x;` / `pub mod x;` lines, with
/// any `#[cfg]`/`#[path]` attribute on the lines above. An inline `mod x { }`
/// is not a file and is not counted.
fn declared_modules(main_rs: &Path) -> BTreeSet<String> {
    let source =
        fs::read_to_string(main_rs).unwrap_or_else(|e| panic!("failed to read {}: {e}", main_rs.display()));
    source
        .lines()
        .filter_map(|line| {
            let code = line.split("//").next().unwrap_or("").trim();
            let code = code.strip_prefix("pub ").unwrap_or(code);
            let name = code.strip_prefix("mod ")?.strip_suffix(';')?.trim();
            let valid = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            valid.then(|| name.to_string())
        })
        .collect()
}

/// The modules a binary directory holds on disk: every `*.rs` but `main.rs`,
/// and every subdirectory (which must be a `mod.rs` module). Problems found
/// on the way are pushed onto `problems`.
fn modules_on_disk(dir: &Path, problems: &mut Vec<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for path in entries(dir) {
        let name = file_name(&path);
        if path.is_dir() {
            if path.join("mod.rs").is_file() {
                out.insert(name);
            } else {
                problems.push(format!(
                    "{}/{name}/ has no mod.rs, so nothing under it is compiled",
                    file_name(dir)
                ));
            }
        } else if let Some(stem) = name.strip_suffix(".rs") {
            if stem != "main" {
                out.insert(stem.to_string());
            }
        }
    }
    out
}

#[test]
fn core_tests_have_exactly_the_two_binaries() {
    let tests = tests_dir();
    let mut problems: Vec<String> = Vec::new();

    for path in entries(&tests) {
        let name = file_name(&path);
        if path.is_dir() && TOP_LEVEL_DIRS.contains(&name.as_str()) {
            continue;
        }
        if name.ends_with(".rs") {
            problems.push(format!("core/tests/{name} would be a test binary of its own"));
        } else {
            problems.push(format!("core/tests/{name} is not one of {TOP_LEVEL_DIRS:?}"));
        }
    }

    for binary in BINARIES {
        let dir = tests.join(binary);
        let main_rs = dir.join("main.rs");
        assert!(main_rs.is_file(), "{} is missing: {RULE}", main_rs.display());
        let declared = declared_modules(&main_rs);
        let on_disk = modules_on_disk(&dir, &mut problems);
        for missing in on_disk.difference(&declared) {
            problems.push(format!(
                "{binary}/{missing} has no `mod {missing};` in {binary}/main.rs, so its tests never run"
            ));
        }
        for extra in declared.difference(&on_disk) {
            problems.push(format!("{binary}/main.rs declares `mod {extra};` but no such file exists"));
        }
    }

    // A `main.rs` anywhere else under core/tests is a binary cargo does not
    // build (only `tests/<dir>/main.rs` one level down is auto-discovered) or,
    // one level down, a third binary.
    let mut stack = vec![tests.clone()];
    while let Some(dir) = stack.pop() {
        for path in entries(&dir) {
            if path.is_dir() {
                stack.push(path);
            } else if file_name(&path) == "main.rs" {
                let rel = path.strip_prefix(&tests).unwrap_or(&path);
                let parent = rel.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
                if !BINARIES.contains(&parent.as_str()) {
                    problems
                        .push(format!("core/tests/{} is a main.rs outside it/ and scripts/", rel.display()));
                }
            }
        }
    }

    assert!(problems.is_empty(), "core/tests/ layout: {problems:#?}\nThe rule: {RULE}.");
}
