//! A change runs only the test sections it can affect (`scripts/test-select.sh`,
//! GM-539), and a release is cut only from a commit that passed the whole
//! suite (`scripts/cut-release.sh`'s `wait_for_green_full_ci`).
//!
//! The selector runs for real on path lists (`--paths-from`) and on a scratch
//! git repository (`--base`) holding copies of the two section scripts. The
//! release gate is sourced and called against a scratch repository with a
//! stand-in `gh` that answers from fixture JSON through the real `jq`, so the
//! script's own `--jq` filters decide. Bash, git and jq only, so Unix only,
//! like `test_sections_scripts.rs`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;

const CORE: [&str; 7] =
    ["core-it", "core-scripts", "core-mcp", "core-daemon", "core-cli", "core-graph", "core-rest"];

/// Every core section but `core-it` and `core-scripts`: the lib sections.
const CORE_LIB: [&str; 5] = ["core-mcp", "core-daemon", "core-cli", "core-graph", "core-rest"];

fn repo_root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).to_path_buf()
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn write_executable(path: &Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

// ---------------------------------------------------------------------------
// scripts/test-select.sh --paths-from

fn select_script(script_root: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script_root.join("scripts/test-select.sh"))
        .args(args)
        .output()
        .expect("failed to run bash")
}

/// Runs the selector on `contents` as a `--paths-from` file and returns its
/// one output line; any failure panics with the script's output.
fn select_raw(contents: &str) -> String {
    select_raw_with(&[], contents)
}

/// `select_raw` with `flags` (such as `--narrow`) before `--paths-from`.
fn select_raw_with(flags: &[&str], contents: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("paths.txt");
    fs::write(&list, contents).unwrap();
    let mut args = flags.to_vec();
    args.extend(["--paths-from", list.to_str().unwrap()]);
    let output = select_script(&repo_root(), &args);
    assert!(output.status.success(), "{}", describe(&output));
    let stdout = stdout_of(&output);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "expected exactly one output line for {contents:?}\n{}", describe(&output));
    lines[0].to_owned()
}

fn select(paths: &[&str]) -> String {
    select_raw(&paths.iter().map(|p| format!("{p}\n")).collect::<String>())
}

/// What `--narrow` selects for `paths`.
fn narrowed(paths: &[&str]) -> String {
    select_raw_with(&["--narrow"], &paths.iter().map(|p| format!("{p}\n")).collect::<String>())
}

/// `core-it` and the five lib sections, space-separated, then `extra`.
fn core_it_and_lib(extra: &[&str]) -> String {
    ["core-it"].iter().chain(CORE_LIB.iter()).chain(extra).copied().collect::<Vec<_>>().join(" ")
}

/// The seven core sections followed by `extra`, space-separated.
fn core_plus(extra: &[&str]) -> String {
    CORE.iter().chain(extra).copied().collect::<Vec<_>>().join(" ")
}

/// Asserts each path, alone, selects `expected`.
fn assert_each(paths: &[&str], expected: &str) {
    for path in paths {
        assert_eq!(select(&[path]), expected, "path {path}");
    }
}

#[test]
fn wire_and_sdk_changes_run_every_section() {
    assert_each(
        &["wire/src/lib.rs", "wire/Cargo.toml", "plugins/sdk/src/lib.rs", "plugins/sdk/tests/toy.rs"],
        "full",
    );
}

#[test]
fn cargo_and_nextest_configuration_runs_every_section() {
    assert_each(
        &["Cargo.toml", "Cargo.lock", ".cargo/config.toml", ".config/nextest.toml", "rust-toolchain.toml"],
        "full",
    );
}

#[test]
fn ci_and_the_selection_machinery_run_every_section() {
    assert_each(
        &[
            ".github/workflows/ci.yml",
            "scripts/test-sections.sh",
            "scripts/test-sections-check.py",
            "scripts/test-select.sh",
            ".gitattributes",
        ],
        "full",
    );
}

#[test]
fn a_core_change_runs_the_core_sections_only() {
    assert_each(&["core/src/main.rs", "core/tests/foo.rs", "core/Cargo.toml"], &core_plus(&[]));
}

/// CI's call (no `--narrow`) keeps every core section for a leaf-module
/// change; only `scripts/test-local.sh`'s `--narrow` call narrows it. A cli
/// path adds `core-rest` because a storage test calls `cli::status::index_status`.
#[test]
fn a_cli_or_mcp_change_runs_every_core_section_unless_narrowed() {
    let leaves =
        ["core/src/cli/status.rs", "core/src/cli/x/y.rs", "core/src/mcp/mod.rs", "core/src/mcp/x.rs"];
    assert_each(&leaves, &core_plus(&[]));

    assert_eq!(narrowed(&["core/src/cli/status.rs"]), "core-it core-cli core-rest");
    assert_eq!(narrowed(&["core/src/cli/x/y.rs"]), "core-it core-cli core-rest");
    assert_eq!(narrowed(&["core/src/mcp/mod.rs"]), "core-it core-mcp");
    assert_eq!(narrowed(&["core/src/cli/a.rs", "core/src/mcp/b.rs"]), "core-it core-mcp core-cli core-rest");
    // Any other core path selects more, narrowed or not.
    assert_eq!(narrowed(&["core/src/graph/x.rs"]), core_it_and_lib(&[]));
    assert_eq!(narrowed(&["core/src/cli/a.rs", "core/src/graph/x.rs"]), core_it_and_lib(&[]));
    for other in ["core/src/lib.rs", "core/tests/foo.rs", "core/Cargo.toml"] {
        assert_eq!(narrowed(&[other]), core_plus(&[]), "path {other}");
        assert_eq!(narrowed(&["core/src/cli/a.rs", other]), core_plus(&[]), "path {other} with a cli path");
    }
    // The flag's position does not matter.
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("paths.txt");
    fs::write(&list, "core/src/cli/a.rs\n").unwrap();
    let output = select_script(&repo_root(), &["--paths-from", list.to_str().unwrap(), "--narrow"]);
    assert_eq!(stdout_of(&output), "core-it core-cli core-rest\n", "{}", describe(&output));
}

#[test]
fn the_local_run_scripts_run_every_section_narrowed_or_not() {
    for path in ["scripts/test-local.sh", "scripts/test-heavy-report.py"] {
        assert_eq!(select(&[path]), "full", "path {path}");
        assert_eq!(select_raw_with(&["--narrow"], &format!("{path}\n")), "full", "path {path} --narrow");
    }
}

#[test]
fn a_plugin_change_runs_that_plugins_section_and_every_core_section_but_no_other_plugin() {
    for plugin in ["typescript", "rust", "python"] {
        let expected = core_plus(&[&format!("plugin-{plugin}")]);
        assert_each(
            &[&format!("plugins/{plugin}/src/x.rs"), &format!("plugins/{plugin}/plugin.toml")],
            &expected,
        );
    }
}

#[test]
fn two_plugin_changes_run_both_plugin_sections_in_list_order() {
    let expected = core_plus(&["plugin-typescript", "plugin-python"]);
    assert_eq!(select(&["plugins/python/src/a.rs", "plugins/typescript/src/b.rs"]), expected);
}

#[test]
fn a_go_plugin_change_runs_the_core_sections_without_a_plugin_section() {
    assert_each(&["plugins/go/main.go", "plugins/go/plugin.toml"], &core_plus(&[]));
}

#[test]
fn readme_scripts_and_eval_changes_run_their_core_sections() {
    assert_eq!(select(&["README.md"]), "core-cli");
    assert_each(&["scripts/install.sh", "scripts/cut-release.sh"], "core-it core-scripts core-cli");
    assert_eq!(select(&["eval/run.py"]), "core-cli");
}

#[test]
fn a_docs_only_change_runs_no_section() {
    assert_each(
        &[
            "docs/adr/0030-test-sections.md",
            "docs/images/x.png",
            "CHANGELOG.md",
            "LICENSE",
            "LICENSE-MIT",
            ".gitignore",
            ".git-blame-ignore-revs",
            "clippy.toml",
            "rustfmt.toml",
        ],
        "none",
    );
    assert_eq!(select(&["docs/a.md", "CHANGELOG.md"]), "none");
    assert_eq!(select_raw(""), "none");
}

#[test]
fn a_markdown_file_under_core_or_a_plugin_matches_its_directory_row_first() {
    assert_eq!(select(&["core/README.md"]), core_plus(&[]));
    assert_eq!(select(&["plugins/rust/README.md"]), core_plus(&["plugin-rust"]));
}

#[test]
fn an_unknown_path_runs_every_section() {
    assert_each(&["somewhere/new.txt", "plugins/unknown/x.rs", "Makefile"], "full");
    assert_eq!(select(&["docs/a.md", "somewhere/new.txt"]), "full");
    assert_eq!(select(&["core/src/lib.rs", "somewhere/new.txt", "README.md"]), "full");
}

#[test]
fn a_mixed_change_runs_the_union_in_list_order_without_duplicates() {
    assert_eq!(select(&["README.md", "core/src/lib.rs"]), core_plus(&[]));
    assert_eq!(
        select(&["plugins/rust/src/a.rs", "eval/x.py", "docs/a.md", "scripts/install.sh"]),
        core_plus(&["plugin-rust"])
    );
    assert_eq!(select(&["README.md", "scripts/install.sh"]), "core-it core-scripts core-cli");
    assert_eq!(select(&["core/src/lib.rs", "wire/src/lib.rs"]), "full");
}

#[test]
fn crlf_lines_and_blank_lines_are_tolerated() {
    assert_eq!(select_raw("\r\nREADME.md\r\n\r\n\nscripts/install.sh\r\n"), "core-it core-scripts core-cli");
    assert_eq!(select_raw("docs/a.md\r\n"), "none");
}

/// A scratch checkout holding the real selector and section scripts and
/// `map` as `scripts/test-plugin-spawns.txt`.
fn scratch_root_with_map(map: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for script in ["scripts/test-select.sh", "scripts/test-sections.sh"] {
        write_executable(&dir.path().join(script), &fs::read_to_string(repo_root().join(script)).unwrap());
    }
    write_file(dir.path(), "scripts/test-plugin-spawns.txt", map);
    dir
}

/// `--narrow --paths-from` run from `root`'s copy of the scripts.
fn narrowed_in(root: &Path, paths: &[&str]) -> String {
    let list = root.join("paths.txt");
    fs::write(&list, paths.iter().map(|p| format!("{p}\n")).collect::<String>()).unwrap();
    let output = select_script(root, &["--narrow", "--paths-from", list.to_str().unwrap()]);
    assert!(output.status.success(), "{}", describe(&output));
    stdout_of(&output).trim_end().to_owned()
}

fn go_query_narrowed(paths: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("paths.txt");
    fs::write(&list, paths.iter().map(|p| format!("{p}\n")).collect::<String>()).unwrap();
    let output = select_script(&repo_root(), &["--narrow", "--go", "--paths-from", list.to_str().unwrap()]);
    assert!(output.status.success(), "{}", describe(&output));
    stdout_of(&output).trim_end().to_owned()
}

/// A narrowed plugin source change runs the plugin's own section and only the
/// core tests that start it (`core-spawns-<plugin>`), not every core section.
#[test]
fn a_narrowed_plugin_source_change_runs_that_plugin_and_the_core_tests_that_start_it() {
    for plugin in ["typescript", "rust", "python"] {
        let expected = format!("plugin-{plugin} core-spawns-{plugin}");
        for path in [format!("plugins/{plugin}/src/x.rs"), format!("plugins/{plugin}/README.md")] {
            assert_eq!(narrowed(&[&path]), expected, "path {path}");
        }
    }
    assert_eq!(narrowed(&["plugins/typescript/package.json"]), "plugin-typescript core-spawns-typescript");
    // Go has no plugin section: its own tests run through `--go`.
    assert_eq!(narrowed(&["plugins/go/main.go"]), "core-spawns-go");
    assert_eq!(go_query_narrowed(&["plugins/go/main.go"]), "1");
}

/// The manifest and `Cargo.toml` are read by core tests that do not start the
/// plugin, and CI never narrows: those keep today's answer.
#[test]
fn a_narrowed_manifest_change_and_every_unnarrowed_plugin_change_stay_wide() {
    for plugin in ["typescript", "rust", "python"] {
        let expected = core_plus(&[&format!("plugin-{plugin}")]);
        for file in ["plugin.toml", "Cargo.toml", "src/x.rs"] {
            let path = format!("plugins/{plugin}/{file}");
            assert_eq!(select(&[&path]), expected, "path {path}");
            if file != "src/x.rs" {
                assert_eq!(narrowed(&[&path]), expected, "path {path} --narrow");
            }
        }
    }
    assert_eq!(narrowed(&["plugins/go/plugin.toml"]), core_plus(&[]));
    assert_eq!(select(&["plugins/go/main.go"]), core_plus(&[]));
    assert_eq!(go_query_narrowed(&["plugins/go/plugin.toml"]), "1");
}

#[test]
fn the_spawn_map_is_part_of_the_selection_machinery() {
    assert_eq!(select(&["scripts/test-plugin-spawns.txt"]), "full");
    assert_eq!(narrowed(&["scripts/test-plugin-spawns.txt"]), "full");
}

/// The `core-spawns-*` selectors come after the sections, in plugin order, and
/// only while some core section that holds a mapped binary is missing.
#[test]
fn core_spawns_selectors_follow_the_sections_and_are_dropped_when_core_it_and_the_lib_sections_run() {
    assert_eq!(
        narrowed(&["plugins/typescript/src/x.rs", "core/tests/it/x.rs"]),
        "core-it plugin-typescript core-spawns-typescript"
    );
    assert_eq!(
        narrowed(&["plugins/python/src/a.rs", "plugins/typescript/src/b.rs", "plugins/rust/src/c.rs"]),
        "plugin-typescript plugin-rust plugin-python core-spawns-typescript core-spawns-rust core-spawns-python"
    );
    assert_eq!(
        narrowed(&["plugins/go/main.go", "plugins/typescript/src/b.rs"]),
        "plugin-typescript core-spawns-typescript core-spawns-go"
    );
    // `core-it` and the five lib sections already hold every mapped test.
    assert_eq!(
        narrowed(&["plugins/typescript/src/x.rs", "core/src/config/mod.rs"]),
        core_it_and_lib(&["plugin-typescript"])
    );
    // Without `--narrow` the selectors are never printed.
    assert_eq!(select(&["plugins/typescript/src/x.rs"]), core_plus(&["plugin-typescript"]));
}

/// With a map row for the `scripts` binary, `core-scripts` also holds mapped
/// tests, so the selectors stay until it runs too.
#[test]
fn a_scripts_row_in_the_map_keeps_the_selectors_until_core_scripts_runs() {
    let root = scratch_root_with_map("scripts:probe typescript\n");
    assert_eq!(
        narrowed_in(root.path(), &["plugins/typescript/src/x.rs", "core/src/config/mod.rs"]),
        core_it_and_lib(&["plugin-typescript", "core-spawns-typescript"])
    );
    assert_eq!(
        narrowed_in(
            root.path(),
            &["plugins/typescript/src/x.rs", "core/src/config/mod.rs", "core/tests/scripts/x.rs"]
        ),
        core_plus(&["plugin-typescript"])
    );
}

const SCRIPTS_CLOSURE_PATHS: [&str; 10] = [
    "core/src/lib.rs",
    "core/src/log.rs",
    "core/src/languages.rs",
    "core/src/languages/tests.rs",
    "core/src/daemon/manifest.rs",
    "core/src/daemon/manifest/tests.rs",
    "core/src/graph/symbol_links.rs",
    "core/src/graph/symbol_links/x.rs",
    "core/src/protocol/types.rs",
    "core/src/protocol/mod.rs",
];

#[test]
fn a_narrowed_core_source_change_outside_the_scripts_closure_skips_core_scripts() {
    for path in ["core/src/config/mod.rs", "core/src/graph/x.rs", "core/src/storage/index_store.rs"] {
        assert_eq!(narrowed(&[path]), core_it_and_lib(&[]), "path {path}");
    }
    for path in SCRIPTS_CLOSURE_PATHS {
        assert_eq!(narrowed(&[path]), core_plus(&[]), "closure path {path}");
    }
}

#[test]
fn a_narrowed_core_test_change_runs_only_the_binary_it_belongs_to() {
    assert_eq!(narrowed(&["core/tests/it/x.rs"]), "core-it");
    assert_eq!(narrowed(&["core/tests/scripts/x.rs"]), "core-it core-scripts");
    assert_eq!(narrowed(&["core/tests/scripts/main.rs"]), "core-it core-scripts");
    for path in ["core/Cargo.toml", "core/build.rs", "core/tests/fixtures/valid.ndjson", "core/tests/foo.rs"]
    {
        assert_eq!(narrowed(&[path]), core_plus(&[]), "path {path}");
    }
}

#[test]
fn without_narrow_every_core_path_runs_every_core_section() {
    for path in SCRIPTS_CLOSURE_PATHS.iter().chain(&[
        "core/src/config/mod.rs",
        "core/tests/it/x.rs",
        "core/tests/scripts/x.rs",
        "core/build.rs",
    ]) {
        assert_eq!(select(&[path]), core_plus(&[]), "path {path}");
    }
}

#[test]
fn a_log_line_test_change_adds_the_sdk_section_narrowed_or_not() {
    let expected = core_plus(&["sdk"]);
    assert_eq!(select(&["core/tests/log_line/x.rs"]), expected);
    assert_eq!(narrowed(&["core/tests/log_line/x.rs"]), expected);
    assert_eq!(narrowed(&["core/tests/log_line/x.rs", "core/tests/it/y.rs"]), expected);
}

/// The scripts tests read `scripts/`, so a change there keeps them running.
#[test]
fn a_scripts_change_runs_core_scripts_narrowed_or_not() {
    for path in ["scripts/install.sh", "scripts/new-task-worktree.sh", "scripts/test-plugin-spawns.sh"] {
        assert_eq!(select(&[path]), "core-it core-scripts core-cli", "path {path}");
        assert_eq!(narrowed(&[path]), "core-it core-scripts core-cli", "path {path} --narrow");
    }
}

/// Every core module the scripts tests use directly is a path the selector
/// maps to `core-scripts`, so changing it cannot skip the tests that use it.
/// Only direct uses are read; what those modules call is the selector's own
/// hand-kept closure.
#[test]
fn every_core_module_the_scripts_tests_use_selects_core_scripts() {
    let src = repo_root().join("core/src");
    let mut uses: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for entry in fs::read_dir(repo_root().join("core/tests/scripts")).unwrap() {
        let path = entry.unwrap().path();
        // This file names the pattern it searches for.
        if path.extension().is_none_or(|e| e != "rs") || path.ends_with("test_select_scripts.rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for (at, _) in text.match_indices("g_mesh::") {
            let boundary = text[..at].chars().next_back().is_none_or(|c| !(c.is_alphanumeric() || c == '_'));
            if !boundary {
                continue;
            }
            let rest = &text[at + "g_mesh::".len()..];
            assert!(
                !rest.starts_with('{'),
                "{file}: `g_mesh::{{..}}` groups are not read by this guard; write each path in full"
            );
            let mut segments = rest
                .split("::")
                .map(|s| s.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect::<String>());
            let a = segments.next().unwrap();
            if a.is_empty() {
                continue;
            }
            let b = segments.next().unwrap_or_default();
            let candidates = [
                (!b.is_empty()).then(|| format!("core/src/{a}/{b}.rs")),
                (!b.is_empty()).then(|| format!("core/src/{a}/{b}/mod.rs")),
                Some(format!("core/src/{a}.rs")),
                Some(format!("core/src/{a}/mod.rs")),
            ];
            let found =
                candidates.into_iter().flatten().find(|c| repo_root().join(c).is_file()).unwrap_or_else(
                    || panic!("{file}: g_mesh::{a}::{b} is not a file under {}", src.display()),
                );
            uses.entry(found).or_insert(file.clone());
        }
    }
    assert!(!uses.is_empty(), "no `g_mesh::` use found in core/tests/scripts; the guard reads nothing");
    let missed: Vec<String> = uses
        .iter()
        .filter(|(path, _)| !narrowed(&[path.as_str()]).split(' ').any(|s| s == "core-scripts"))
        .map(|(path, file)| format!("{path} (used by core/tests/scripts/{file})"))
        .collect();
    assert!(
        missed.is_empty(),
        "a narrowed change to these selects no `core-scripts`; add them to SCRIPTS_CLOSURE in \
         scripts/test-select.sh: {missed:?}"
    );
}

#[test]
fn bad_arguments_exit_2_without_a_selection() {
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("paths.txt");
    fs::write(&list, "README.md\n").unwrap();
    let list = list.to_str().unwrap();
    let missing = dir.path().join("missing.txt");
    let cases: Vec<Vec<&str>> = vec![
        vec!["--paths-from", missing.to_str().unwrap()],
        vec!["--paths-from"],
        vec!["--base"],
        vec!["--base", "HEAD", "--paths-from", list],
        vec!["--bogus"],
        vec!["README.md"],
    ];
    for args in cases {
        let output = select_script(&repo_root(), &args);
        assert_eq!(output.status.code(), Some(2), "args {args:?}\n{}", describe(&output));
        assert_eq!(stdout_of(&output), "", "args {args:?} printed a selection");
    }
}

// ---------------------------------------------------------------------------
// scripts/test-select.sh with git

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").args(args).current_dir(dir).output().expect("git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("failed to run git");
    assert!(output.status.success(), "git {args:?}\n{}", describe(&output));
}

fn write_file(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

/// A scratch repository holding copies of the two section scripts, a doc,
/// a core file and a wire file, committed on `main` and branched as
/// `release-9.9.9`, with HEAD on `main`.
fn scratch_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    for script in ["scripts/test-select.sh", "scripts/test-sections.sh"] {
        let body = fs::read_to_string(repo_root().join(script)).unwrap();
        write_executable(&root.join(script), &body);
    }
    write_file(root, "docs/a.md", "a\n");
    write_file(root, "core/src/lib.rs", "// core\n");
    write_file(root, "wire/src/lib.rs", "// wire\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["branch", "release-9.9.9"]);
    dir
}

fn select_in(repo: &Path, args: &[&str]) -> Output {
    select_script(repo, args)
}

fn selection_in(repo: &Path, args: &[&str]) -> String {
    let output = select_in(repo, args);
    assert!(output.status.success(), "{}", describe(&output));
    stdout_of(&output).trim_end().to_owned()
}

#[test]
fn base_mode_counts_committed_staged_unstaged_and_untracked_changes() {
    let repo = scratch_repo();
    let root = repo.path();
    assert_eq!(selection_in(root, &["--base", "release-9.9.9"]), "none");

    write_file(root, "docs/b.md", "b\n");
    git(root, &["add", "docs/b.md"]);
    git(root, &["commit", "-q", "-m", "docs"]);
    assert_eq!(selection_in(root, &["--base", "release-9.9.9"]), "none", "committed docs");

    write_file(root, "README.md", "readme\n");
    assert_eq!(selection_in(root, &["--base", "release-9.9.9"]), "core-cli", "untracked README");

    write_file(root, "core/src/lib.rs", "// core changed\n");
    assert_eq!(selection_in(root, &["--base", "release-9.9.9"]), core_plus(&[]), "unstaged core");

    write_file(root, "wire/src/lib.rs", "// wire changed\n");
    git(root, &["add", "wire/src/lib.rs"]);
    assert_eq!(selection_in(root, &["--base", "release-9.9.9"]), "full", "staged wire");
}

/// `--go` (GM-573): does the diff need the Go plugin's own tests?
fn go_query(paths: &[&str]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("paths.txt");
    fs::write(&list, paths.iter().map(|p| format!("{p}\n")).collect::<String>()).unwrap();
    let output = select_script(&repo_root(), &["--go", "--paths-from", list.to_str().unwrap()]);
    assert!(output.status.success(), "{}", describe(&output));
    stdout_of(&output).trim_end().to_owned()
}

#[test]
fn go_query_is_1_for_a_path_under_plugins_go_and_0_for_other_changes() {
    assert_eq!(go_query(&["plugins/go/main.go"]), "1");
    assert_eq!(go_query(&["docs/a.md", "plugins/go/internal/x.go"]), "1");
    assert_eq!(go_query(&["docs/a.md"]), "0");
    assert_eq!(
        go_query(&["core/src/cli/status.rs", "plugins/rust/src/lib.rs", "plugins/typescript/a.ts"]),
        "0"
    );
}

#[test]
fn go_query_is_1_when_the_selection_is_full() {
    assert_eq!(go_query(&["wire/src/lib.rs"]), "1");
    assert_eq!(go_query(&["Cargo.toml"]), "1");
}

#[test]
fn go_query_in_base_mode_sees_an_untracked_go_file() {
    let repo = scratch_repo();
    let root = repo.path();
    assert_eq!(selection_in(root, &["--go", "--base", "release-9.9.9"]), "0");
    write_file(root, "plugins/go/main.go", "package main\n");
    assert_eq!(selection_in(root, &["--go", "--base", "release-9.9.9"]), "1");
}

#[test]
fn base_mode_sees_a_file_moved_out_of_wire() {
    let repo = scratch_repo();
    let root = repo.path();
    git(root, &["mv", "wire/src/lib.rs", "docs/lib.rs.md"]);
    git(root, &["commit", "-q", "-m", "move"]);
    assert_eq!(selection_in(root, &["--base", "release-9.9.9"]), "full");
}

#[test]
fn base_mode_refuses_an_unknown_base_instead_of_selecting_none() {
    let repo = scratch_repo();
    let output = select_in(repo.path(), &["--base", "no-such-ref"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert_eq!(stdout_of(&output), "");
}

#[test]
fn without_a_base_the_nearest_release_branch_is_used() {
    let repo = scratch_repo();
    let root = repo.path();
    write_file(root, "core/src/lib.rs", "// core changed\n");
    git(root, &["commit", "-q", "-am", "core"]);
    // release-9.9.9 is further behind HEAD and must not be chosen: from it
    // the core change above would count too.
    git(root, &["branch", "release-9.9.10"]);
    write_file(root, "README.md", "readme\n");
    git(root, &["add", "README.md"]);
    git(root, &["commit", "-q", "-m", "readme"]);
    let output = select_in(root, &[]);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(stdout_of(&output).trim_end(), "core-cli");
    assert!(stderr_of(&output).contains("base release-9.9.10"), "{}", describe(&output));
}

#[test]
fn without_a_base_or_a_release_branch_the_selector_asks_for_one() {
    let repo = scratch_repo();
    git(repo.path(), &["branch", "-D", "release-9.9.9"]);
    let output = select_in(repo.path(), &[]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert_eq!(stdout_of(&output), "");
    assert!(stderr_of(&output).contains("pass --base"), "{}", describe(&output));
}

// ---------------------------------------------------------------------------
// scripts/test-sections.sh run none|full

fn recording_run(args: &[&str]) -> (Output, String) {
    let dir = tempfile::tempdir().unwrap();
    let called = dir.path().join("cargo-called");
    write_executable(
        &dir.path().join("bin/cargo"),
        &format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", called.display()),
    );
    let output = Command::new("bash")
        .arg(repo_root().join("scripts/test-sections.sh"))
        .arg("run")
        .args(args)
        .env(
            "PATH",
            format!("{}:{}", dir.path().join("bin").display(), std::env::var("PATH").unwrap_or_default()),
        )
        .env("CARGO_TARGET_DIR", dir.path().join("target"))
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .expect("failed to run bash");
    let calls = fs::read_to_string(&called).unwrap_or_default();
    (output, calls)
}

/// The `-E` filters of the recorded `cargo nextest run` calls, in order.
fn run_calls(calls: &str) -> Vec<String> {
    calls.lines().filter(|l| l.starts_with("nextest run")).map(str::to_owned).collect()
}

#[test]
fn run_none_reports_no_sections_and_never_calls_cargo() {
    let (output, calls) = recording_run(&["none"]);
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("== no sections selected"), "{}", describe(&output));
    assert_eq!(calls, "", "cargo was called for `run none`");
}

#[test]
fn run_full_runs_the_same_sections_as_run_all() {
    let (full, full_calls) = recording_run(&["full"]);
    assert!(full.status.success(), "{}", describe(&full));
    let (all, all_calls) = recording_run(&["all"]);
    assert!(all.status.success(), "{}", describe(&all));
    assert_eq!(run_calls(&full_calls).len(), 12, "{full_calls}");
    assert_eq!(run_calls(&full_calls), run_calls(&all_calls));
}

// ---------------------------------------------------------------------------
// scripts/cut-release.sh check_release_merge_shape + wait_for_green_full_ci

const VERSION: &str = "9.9.9";

/// A scratch release: `release-9.9.9` pushed to a bare `origin` and merged
/// into `main` with the subject the gate expects, plus a stand-in `gh` that
/// answers `gh api <endpoint> --jq <filter>` by running the real `jq` on
/// `<fixtures>/runs-<n>.json` (the n-th runs query; the last file repeats) or
/// `<fixtures>/jobs-<run id>.json`. Every call is appended to
/// `<fixtures>/calls`; `<fixtures>/failures` holds how many runs queries
/// still fail with exit 1.
struct Release {
    dir: tempfile::TempDir,
}

impl Release {
    fn new() -> Self {
        Self::build(false)
    }

    /// `main` holds a commit the release branch lacks, so the merge's tree
    /// differs from the tested tip's.
    fn with_main_work() -> Self {
        Self::build(true)
    }

    fn build(main_work: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let origin = dir.path().join("origin.git");
        fs::create_dir_all(&root).unwrap();
        git(dir.path(), &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&root, &["init", "-q", "-b", "main"]);
        write_file(&root, "a.txt", "a\n");
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "base"]);
        git(&root, &["checkout", "-q", "-b", &format!("release-{VERSION}")]);
        write_file(&root, "b.txt", "b\n");
        git(&root, &["add", "b.txt"]);
        git(&root, &["commit", "-q", "-m", "release work"]);
        git(&root, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&root, &["push", "-q", "origin", &format!("release-{VERSION}")]);
        git(&root, &["checkout", "-q", "main"]);
        if main_work {
            write_file(&root, "c.txt", "c\n");
            git(&root, &["add", "c.txt"]);
            git(&root, &["commit", "-q", "-m", "main work"]);
        }
        git(
            &root,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                &format!("merge: release-{VERSION} into main"),
                &format!("release-{VERSION}"),
            ],
        );
        write_executable(
            &dir.path().join("bin/gh"),
            &format!(
                r#"#!/bin/sh
# gh api <endpoint> --jq <filter>
[ "$1" = api ] && [ "$3" = --jq ] || {{ echo "fake gh: unexpected call: $*" >&2; exit 99; }}
f="{fixtures}"
echo "$2" >> "$f/calls"
case "$2" in
*/workflows/ci.yml/runs*)
  left=$(cat "$f/failures" 2>/dev/null || echo 0)
  if [ "$left" -gt 0 ]; then echo $((left - 1)) > "$f/failures"; echo "fake gh: HTTP 502" >&2; exit 1; fi
  n=$(( $(cat "$f/runs.count" 2>/dev/null || echo 0) + 1 )); echo "$n" > "$f/runs.count"
  while [ ! -f "$f/runs-$n.json" ] && [ "$n" -gt 1 ]; do n=$((n - 1)); done
  file="runs-$n.json" ;;
*/actions/runs/*/jobs*) id="${{2#*/actions/runs/}}"; file="jobs-${{id%%/*}}.json" ;;
*) echo "fake gh: unexpected endpoint $2" >&2; exit 99 ;;
esac
[ -f "{fixtures}/$file" ] || {{ echo "fake gh: no fixture $file" >&2; exit 98; }}
exec jq -r "$4" "{fixtures}/$file"
"#,
                fixtures = dir.path().join("fixtures").display()
            ),
        );
        fs::create_dir_all(dir.path().join("fixtures")).unwrap();
        Release { dir }
    }

    fn tip(&self) -> String {
        git_out(&self.dir.path().join("repo"), &["rev-parse", "main^2"])
    }

    /// The answer of the `n`-th runs query: runs of `head_sha` as
    /// (run id, status, conclusion, attempt); conclusion "none" is null.
    fn runs_of(&self, n: usize, head_sha: &str, runs: &[(u64, &str, &str, u32)]) {
        let runs: Vec<_> = runs
            .iter()
            .map(|(id, status, c, attempt)| {
                let conclusion = if *c == "none" { json!(null) } else { json!(c) };
                json!({ "id": id, "status": status, "conclusion": conclusion, "head_sha": head_sha, "run_attempt": attempt })
            })
            .collect();
        fs::write(
            self.dir.path().join(format!("fixtures/runs-{n}.json")),
            json!({ "workflow_runs": runs }).to_string(),
        )
        .unwrap();
    }

    /// The `n`-th runs query answers with these runs of the tip.
    fn runs_at(&self, n: usize, runs: &[(u64, &str, &str, u32)]) {
        self.runs_of(n, &self.tip(), runs);
    }

    /// Every runs query answers with these completed runs of the tip, attempt 1.
    fn runs(&self, runs: &[(u64, &str)]) {
        let rows: Vec<_> = runs.iter().map(|(id, c)| (*id, "completed", *c, 1)).collect();
        self.runs_at(1, &rows);
    }

    fn fail_runs_queries(&self, times: u32) {
        fs::write(self.dir.path().join("fixtures/failures"), times.to_string()).unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("fixtures/calls")).unwrap_or_default()
    }

    fn runs_queries(&self) -> usize {
        self.calls().lines().filter(|l| l.contains("/workflows/ci.yml/runs")).count()
    }

    fn tags(&self) -> String {
        git_out(&self.dir.path().join("repo"), &["tag", "-l"])
    }

    /// The jobs of run `id`, as (name, conclusion).
    fn jobs(&self, id: u64, jobs: &[(&str, &str)]) {
        let jobs: Vec<_> = jobs.iter().map(|(n, c)| json!({ "name": n, "conclusion": c })).collect();
        fs::write(
            self.dir.path().join(format!("fixtures/jobs-{id}.json")),
            json!({ "jobs": jobs }).to_string(),
        )
        .unwrap();
    }

    fn check(&self) -> Output {
        self.check_within(90)
    }

    /// The gate as `main()` calls it, then the tag: `die` exits, so a tag
    /// exists afterwards only when the gate returned.
    fn check_within(&self, timeout_min: u32) -> Output {
        Command::new("bash")
            .arg("-c")
            .arg(
                r#"source "$1" && REPO_ROOT="$2" && check_release_merge_shape "$3" && wait_for_green_full_ci "$3" main "$4"; git -C "$2" tag -a "v$3" -m release"#,
            )
            .arg("cut-release-test")
            .arg(repo_root().join("scripts/cut-release.sh"))
            .arg(self.dir.path().join("repo"))
            .arg(VERSION)
            .arg(timeout_min.to_string())
            .env("CUT_RELEASE_CI_POLL_SECS", "0")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.path().join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .expect("failed to run bash")
    }
}

const FULL_JOB: &str = "Full test suite ran";

#[test]
fn a_tip_with_a_green_full_suite_job_passes_the_gate() {
    let release = Release::new();
    // A partial green run first: the gate must keep looking.
    release.runs(&[(11, "failure"), (12, "success"), (13, "success")]);
    release.jobs(12, &[("Select the test sections", "success"), ("tests", "success"), (FULL_JOB, "skipped")]);
    release.jobs(13, &[("Select the test sections", "success"), ("tests", "success"), (FULL_JOB, "success")]);
    let output = release.check();
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("passed a full ci.yml run (13)"), "{}", describe(&output));
}

fn assert_refused_as_partial(output: &Output) {
    assert!(!output.status.success(), "the gate let a partial run through\n{}", describe(output));
    let stderr = stderr_of(output);
    assert!(stderr.contains("only as a partial run"), "{}", describe(output));
    assert!(
        stderr.contains(&format!("gh workflow run ci.yml --ref release-{VERSION}")),
        "{}",
        describe(output)
    );
}

#[test]
fn a_green_run_without_a_full_suite_job_is_refused_with_the_dispatch_hint() {
    let release = Release::new();
    release.runs(&[(21, "success")]);
    release.jobs(21, &[("Select the test sections", "success"), ("tests", "success")]);
    assert_refused_as_partial(&release.check());
}

#[test]
fn a_green_run_whose_full_suite_job_did_not_succeed_is_refused() {
    let release = Release::new();
    release.runs(&[(31, "success"), (32, "success")]);
    release.jobs(31, &[("tests", "success"), (FULL_JOB, "failure")]);
    release.jobs(32, &[("tests", "success"), (FULL_JOB, "skipped")]);
    assert_refused_as_partial(&release.check());
}

#[test]
fn a_job_merely_named_like_the_full_suite_job_does_not_count() {
    let release = Release::new();
    release.runs(&[(41, "success")]);
    release.jobs(41, &[(&format!("{FULL_JOB} (dry)"), "success"), ("full-suite", "success")]);
    assert_refused_as_partial(&release.check());
}

#[test]
fn a_tip_without_a_green_run_is_refused_before_any_job_is_read() {
    let release = Release::new();
    release.runs(&[(51, "failure")]);
    let output = release.check();
    assert!(!output.status.success(), "{}", describe(&output));
    assert!(stderr_of(&output).contains("no green full ci.yml run"), "{}", describe(&output));
}

/// The gate looks for the `full-suite` job by its display name; renaming the
/// job in ci.yml without the script would refuse every release.
#[test]
fn the_gates_job_name_is_the_full_suite_jobs_name_in_ci_yml() {
    let script = fs::read_to_string(repo_root().join("scripts/cut-release.sh")).unwrap();
    let in_script = script
        .lines()
        .find_map(|l| l.strip_prefix("CI_FULL_SUITE_JOB="))
        .expect("CI_FULL_SUITE_JOB= line in cut-release.sh")
        .trim_matches('"');
    let ci = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let mut lines = ci.lines().skip_while(|l| *l != "  full-suite:");
    assert_eq!(lines.next(), Some("  full-suite:"), "ci.yml has no top-level full-suite job");
    let name = lines
        .take_while(|l| l.starts_with("    ") || l.is_empty())
        .find_map(|l| l.strip_prefix("    name: "))
        .expect("full-suite job has a name");
    assert_eq!(in_script, name.trim().trim_matches('"').trim_matches('\''));
    assert_eq!(in_script, FULL_JOB);
}

fn assert_died_untagged(release: &Release, output: &Output, message: &str) {
    assert!(!output.status.success(), "the gate let the tag through\n{}", describe(output));
    assert!(stderr_of(output).contains(message), "{}", describe(output));
    assert_eq!(release.tags(), "", "a tag exists after the gate died");
}

fn full_jobs(release: &Release, id: u64) {
    release.jobs(id, &[("tests", "success"), (FULL_JOB, "success")]);
}

#[test]
fn a_green_full_run_on_the_tip_lets_the_tag_through() {
    let release = Release::new();
    release.runs(&[(61, "success")]);
    full_jobs(&release, 61);
    let output = release.check();
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(release.tags(), format!("v{VERSION}"));
}

#[test]
fn a_running_run_is_waited_for_and_the_tag_follows_its_green() {
    let release = Release::new();
    release.runs_at(1, &[(71, "queued", "none", 1)]);
    release.runs_at(2, &[(71, "in_progress", "none", 1)]);
    release.runs_at(3, &[(71, "completed", "success", 1)]);
    full_jobs(&release, 71);
    let output = release.check();
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("waiting for ci.yml run 71"), "{}", describe(&output));
    assert_eq!(release.runs_queries(), 3);
    assert_eq!(release.tags(), format!("v{VERSION}"));
}

#[test]
fn a_failed_run_is_refused_naming_its_id_attempt_and_url() {
    let release = Release::new();
    release.runs_at(1, &[(81, "completed", "failure", 2)]);
    let output = release.check();
    assert_died_untagged(&release, &output, "no green full ci.yml run");
    let stderr = stderr_of(&output);
    assert!(stderr.contains("run 81: failure (attempt 2)"), "{}", describe(&output));
    assert!(stderr.contains("/actions/runs/81"), "{}", describe(&output));
    assert!(!stderr.contains("ci.yml concurrency"), "{}", describe(&output));
}

#[test]
fn a_cancelled_run_alone_is_refused_with_the_concurrency_note() {
    let release = Release::new();
    release.runs_at(1, &[(91, "completed", "cancelled", 1)]);
    let output = release.check();
    assert_died_untagged(&release, &output, "run 91: cancelled");
    assert!(stderr_of(&output).contains("ci.yml concurrency"), "{}", describe(&output));
}

#[test]
fn a_timed_out_or_never_started_run_is_refused() {
    for conclusion in ["timed_out", "startup_failure"] {
        let release = Release::new();
        release.runs_at(1, &[(101, "completed", conclusion, 1)]);
        let output = release.check();
        assert_died_untagged(&release, &output, &format!("run 101: {conclusion}"));
    }
}

#[test]
fn a_tip_without_any_run_is_refused_with_the_push_and_dispatch_hint() {
    let release = Release::new();
    release.runs_at(1, &[]);
    let output = release.check();
    assert_died_untagged(&release, &output, "no ci.yml run for");
    assert!(
        stderr_of(&output).contains(&format!("gh workflow run ci.yml --ref release-{VERSION}")),
        "{}",
        describe(&output)
    );
}

#[test]
fn a_release_branch_that_moved_on_origin_is_refused_before_any_query() {
    let release = Release::new();
    let repo = release.dir.path().join("repo");
    git(&repo, &["checkout", "-q", &format!("release-{VERSION}")]);
    write_file(&repo, "late.txt", "late\n");
    git(&repo, &["add", "late.txt"]);
    git(&repo, &["commit", "-q", "-m", "pushed after the merge"]);
    git(&repo, &["push", "-q", "origin", &format!("release-{VERSION}")]);
    git(&repo, &["checkout", "-q", "main"]);
    release.runs(&[(111, "success")]);
    full_jobs(&release, 111);
    let output = release.check();
    assert_died_untagged(&release, &output, &format!("origin/release-{VERSION} is at"));
    assert_eq!(release.calls(), "");
}

#[test]
fn a_green_run_of_another_commit_does_not_count() {
    let release = Release::new();
    release.runs_of(1, &"0".repeat(40), &[(121, "completed", "success", 1)]);
    full_jobs(&release, 121);
    let output = release.check();
    assert_died_untagged(&release, &output, "no ci.yml run for");
    assert!(!release.calls().contains("/jobs"), "{}", release.calls());
}

#[test]
fn a_main_with_commits_the_release_branch_lacks_is_refused_before_any_query() {
    let release = Release::with_main_work();
    release.runs(&[(131, "success")]);
    full_jobs(&release, 131);
    let output = release.check();
    assert_died_untagged(&release, &output, "main has commits that are not on");
    assert_eq!(release.calls(), "");
}

#[test]
fn a_run_still_running_at_the_timeout_is_refused_naming_it() {
    let release = Release::new();
    release.runs_at(1, &[(141, "in_progress", "none", 1)]);
    let output = release.check_within(0);
    assert_died_untagged(&release, &output, "run 141");
    assert!(stderr_of(&output).contains("still running after 0m"), "{}", describe(&output));
    assert_eq!(release.runs_queries(), 1);
}

#[test]
fn a_re_run_failed_run_is_waited_for_and_read_from_its_latest_attempt() {
    let release = Release::new();
    release.runs_at(1, &[(151, "in_progress", "none", 2)]);
    release.runs_at(2, &[(151, "completed", "success", 2)]);
    full_jobs(&release, 151);
    let output = release.check();
    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout_of(&output).contains("attempt 2"), "{}", describe(&output));
    assert!(release.calls().contains("/actions/runs/151/jobs?filter=latest"), "{}", release.calls());
    assert_eq!(release.tags(), format!("v{VERSION}"));
}

#[test]
fn a_green_full_run_wins_over_failed_cancelled_and_running_runs_of_the_tip() {
    let release = Release::new();
    release.runs_at(
        1,
        &[
            (161, "completed", "failure", 1),
            (162, "completed", "cancelled", 1),
            (163, "in_progress", "none", 1),
            (164, "completed", "success", 1),
        ],
    );
    full_jobs(&release, 164);
    let output = release.check();
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(release.runs_queries(), 1, "the gate waited although a green full run existed");
    assert_eq!(release.tags(), format!("v{VERSION}"));
}

#[test]
fn three_failed_queries_in_a_row_are_refused_and_one_is_retried() {
    let release = Release::new();
    release.runs(&[(171, "success")]);
    full_jobs(&release, 171);
    release.fail_runs_queries(3);
    let output = release.check();
    assert_died_untagged(&release, &output, "after 3 attempts");
    assert_eq!(release.runs_queries(), 3);

    let release = Release::new();
    release.runs(&[(172, "success")]);
    full_jobs(&release, 172);
    release.fail_runs_queries(1);
    let output = release.check();
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(release.runs_queries(), 2);
    assert_eq!(release.tags(), format!("v{VERSION}"));
}

/// The steps that carry what the old local run covered run on the Linux row of
/// a full run only, and are not named like a section (`tests: `).
#[test]
fn ci_yml_runs_the_doctests_and_shipped_scope_on_the_linux_row_of_full_runs_only() {
    let ci = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let test_job: String = ci
        .lines()
        .skip_while(|l| *l != "  test:")
        .take_while(|l| *l == "  test:" || l.starts_with("    ") || l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let steps: Vec<&str> = test_job.split("\n      - name: ").skip(1).collect();
    for name in ["doctests (workspace)", "\"shipped scope: cargo test -p g-mesh\""] {
        let step = steps
            .iter()
            .find(|s| s.starts_with(name))
            .unwrap_or_else(|| panic!("the test job has no step {name}"));
        let condition = step.lines().find(|l| l.trim_start().starts_with("if:")).unwrap_or_default();
        assert!(condition.contains("matrix.target == 'x86_64-unknown-linux-gnu'"), "{name}: {condition}");
        assert!(condition.contains("needs.select.outputs.mode == 'full'"), "{name}: {condition}");
        assert!(!name.trim_matches('"').starts_with("tests: "), "{name} reads as a section");
    }
}
