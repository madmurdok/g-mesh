//! The shape of `core/tests/` itself (GM-623).
//!
//! Core's integration tests are two binaries, `it` and `scripts`, each a
//! directory whose `main.rs` declares one module per file. Cargo makes two
//! silent mistakes possible in that layout, and this guard turns both loud:
//!
//! 1. A new top-level `core/tests/foo.rs` is auto-discovered as a binary of its
//!    own again: it works, but undoes the merge one file at a time (and every
//!    extra binary is one more ~25 MB link of the whole crate).
//! 2. A new `core/tests/it/foo.rs` without `mod foo;` in `it/main.rs` (or
//!    `it/common/foo.rs` without `mod foo;` in `it/common/mod.rs`) is never
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

/// The entries of `dir`, sorted, with whether each is a directory. Dotfiles
/// (`.DS_Store`, an editor's `.#foo.rs` lock) are skipped: cargo builds none
/// of them. `file_type` does not follow symlinks, so a symlinked directory is
/// never descended into (a loop cannot hang the guard) and counts as a file.
fn entries(dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut out: Vec<(PathBuf, bool)> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()))
        .map(|entry| entry.expect("failed to read a directory entry"))
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|entry| {
            let is_dir = entry.file_type().expect("failed to read a file type").is_dir();
            (entry.path(), is_dir)
        })
        .collect();
    out.sort();
    out
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// The module names a `main.rs`/`mod.rs` declares: `mod x;` / `pub mod x;`
/// lines, with any `#[cfg]`/`#[path]` attribute on the lines above. An inline
/// `mod x { }` is not a file and is not counted.
fn declared_modules(root_file: &Path) -> BTreeSet<String> {
    let source = fs::read_to_string(root_file)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", root_file.display()));
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

/// Checks one module directory against its root file (`main.rs` for a
/// binary, `mod.rs` below it): every `*.rs` but the root file and every
/// subdirectory (which must hold a `mod.rs`) must be declared, and every
/// declaration must exist. Recurses into each subdirectory module, so an
/// undeclared `it/common/foo.rs` is caught like an undeclared `it/foo.rs`.
/// `rel` is `dir` relative to `core/tests`, for the messages.
fn check_module_dir(dir: &Path, rel: &str, root_file: &str, problems: &mut Vec<String>) {
    let declared = declared_modules(&dir.join(root_file));
    let mut on_disk = BTreeSet::new();
    for (path, is_dir) in entries(dir) {
        let name = file_name(&path);
        if is_dir {
            if path.join("mod.rs").is_file() {
                check_module_dir(&path, &format!("{rel}/{name}"), "mod.rs", problems);
                on_disk.insert(name);
            } else {
                problems.push(format!("{rel}/{name}/ has no mod.rs, so nothing under it is compiled"));
            }
        } else if let Some(stem) = name.strip_suffix(".rs") {
            if name != root_file {
                on_disk.insert(stem.to_string());
            }
        }
    }
    for missing in on_disk.difference(&declared) {
        problems.push(format!(
            "{rel}/{missing} has no `mod {missing};` in {rel}/{root_file}, so its tests never run"
        ));
    }
    for extra in declared.difference(&on_disk) {
        problems.push(format!("{rel}/{root_file} declares `mod {extra};` but no such file exists"));
    }
}

#[test]
fn core_tests_have_exactly_the_two_binaries() {
    let tests = tests_dir();
    let mut problems: Vec<String> = Vec::new();

    for (path, is_dir) in entries(&tests) {
        let name = file_name(&path);
        if is_dir && TOP_LEVEL_DIRS.contains(&name.as_str()) {
            // Cargo auto-discovers `tests/<dir>/main.rs`, one level down
            // only, as a binary: here, that would be a third one. A
            // `main.rs` deeper down (a fixture crate's `src/main.rs`) is
            // never built by cargo and is no concern of this guard.
            if !BINARIES.contains(&name.as_str()) && path.join("main.rs").is_file() {
                problems.push(format!("core/tests/{name}/main.rs would be a third test binary"));
            }
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
        check_module_dir(&dir, binary, "main.rs", &mut problems);
    }

    assert!(problems.is_empty(), "core/tests/ layout: {problems:#?}\nThe rule: {RULE}.");
}
