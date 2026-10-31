//! The test suite runs as named sections (`scripts/test-sections.sh`), and a
//! coverage check (`scripts/test-sections-check.py`) proves that the sections
//! partition it: every test in exactly one section, none in two, and no
//! section matching a test the whole-suite list lacks.
//!
//! The checker runs for real on fixture `cargo nextest list` JSON, so no cargo
//! work is paid for. The section script's argument handling runs with a
//! stand-in `cargo` on `PATH` that records any call. One test runs `check` on
//! the real tree; it lists the whole suite twelve times, so it is ignored here
//! and CI runs the same `check` as its own step. Bash and python3 only, so Unix
//! only, like `release_packaging_scripts.rs`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Map, Value};

const SECTIONS: [&str; 12] = [
    "core-it",
    "core-scripts",
    "core-mcp",
    "core-daemon",
    "core-cli",
    "core-graph",
    "core-rest",
    "sdk",
    "plugin-typescript",
    "plugin-rust",
    "plugin-python",
    "wire",
];

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

fn stdout_lines(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect()
}

// ---------------------------------------------------------------------------
// scripts/test-sections-check.py

/// A test as the checker keys it, with its `filter-match.status`.
type Case<'a> = (&'a str, &'a str, &'a str);

/// Writes a `cargo nextest list --message-format json` document holding
/// `cases` as (binary-id, test, status) and returns its path.
fn write_list(dir: &Path, file: &str, cases: &[Case]) -> PathBuf {
    let mut suites: Map<String, Value> = Map::new();
    for (binary, test, status) in cases {
        let suite = suites
            .entry(binary.to_string())
            .or_insert_with(|| json!({ "binary-id": binary, "testcases": {} }));
        suite["testcases"]
            .as_object_mut()
            .unwrap()
            .insert(test.to_string(), json!({ "ignored": false, "filter-match": { "status": status } }));
    }
    let path = dir.join(file);
    let doc = json!({ "rust-build-meta": {}, "test-count": cases.len(), "rust-suites": suites });
    fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    path
}

fn matching<'a>(cases: &[(&'a str, &'a str)]) -> Vec<Case<'a>> {
    cases.iter().map(|(b, t)| (*b, *t, "matches")).collect()
}

/// Runs the checker on `all` plus `sections` as (name, list file).
fn run_checker(all: &Path, sections: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new("python3");
    cmd.arg(repo_root().join("scripts/test-sections-check.py")).arg(all);
    for (name, path) in sections {
        cmd.arg(format!("{name}={}", path.display()));
    }
    cmd.output().expect("failed to run python3")
}

const A: (&str, &str) = ("g-mesh", "mcp::tools::a");
const B: (&str, &str) = ("g-mesh", "daemon::b");
const C: (&str, &str) = ("g-mesh::cli_init", "c");

#[test]
fn a_clean_partition_passes_and_prints_each_sections_count() {
    let dir = tempfile::tempdir().unwrap();
    let all = write_list(dir.path(), "all.json", &matching(&[A, B, C]));
    let one = write_list(dir.path(), "one.json", &matching(&[A, B]));
    let two = write_list(dir.path(), "two.json", &matching(&[C]));

    let output = run_checker(&all, &[("one", &one), ("two", &two)]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let lines = stdout_lines(&output);
    assert!(lines.contains(&"one: 2".to_owned()), "{}", describe(&output));
    assert!(lines.contains(&"two: 1".to_owned()), "{}", describe(&output));
    assert!(lines.contains(&"OK: every test is in exactly one section".to_owned()), "{}", describe(&output));
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("missing:") || l.starts_with("overlap:") || l.starts_with("phantom:")),
        "{}",
        describe(&output)
    );
}

#[test]
fn a_test_in_no_section_fails_and_every_missing_test_is_printed() {
    let dir = tempfile::tempdir().unwrap();
    // Fifty uncovered tests: the checker must name each one, not a sample.
    let names: Vec<String> = (0..50).map(|i| format!("graph::uncovered_{i:02}")).collect();
    let mut all_cases: Vec<Case> = matching(&[A]);
    all_cases.extend(names.iter().map(|n| ("g-mesh", n.as_str(), "matches")));
    let all = write_list(dir.path(), "all.json", &all_cases);
    let only = write_list(dir.path(), "only.json", &matching(&[A]));

    let output = run_checker(&all, &[("only", &only)]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let missing: Vec<String> =
        stdout_lines(&output).into_iter().filter(|l| l.starts_with("missing:")).collect();
    let expected: Vec<String> = names.iter().map(|n| format!("missing: g-mesh {n}")).collect();
    assert_eq!(missing, expected, "{}", describe(&output));
}

#[test]
fn a_test_in_two_sections_fails_naming_both_sections_in_argument_order() {
    let dir = tempfile::tempdir().unwrap();
    let all = write_list(dir.path(), "all.json", &matching(&[A, B]));
    // `zeta` comes first on the command line, so the names are not sorted.
    let zeta = write_list(dir.path(), "zeta.json", &matching(&[A, B]));
    let alpha = write_list(dir.path(), "alpha.json", &matching(&[B]));

    let output = run_checker(&all, &[("zeta", &zeta), ("alpha", &alpha)]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let flagged: Vec<String> = stdout_lines(&output)
        .into_iter()
        .filter(|l| l.starts_with("overlap:") || l.starts_with("missing:") || l.starts_with("phantom:"))
        .collect();
    assert_eq!(flagged, vec!["overlap: g-mesh daemon::b in zeta,alpha".to_owned()], "{}", describe(&output));
}

#[test]
fn a_section_matching_a_test_the_whole_list_lacks_fails_as_phantom() {
    let dir = tempfile::tempdir().unwrap();
    let all = write_list(dir.path(), "all.json", &matching(&[A]));
    let ghost = ("g-mesh-wire", "frames::ghost");
    let only = write_list(dir.path(), "only.json", &matching(&[A, ghost]));

    let output = run_checker(&all, &[("only", &only)]);
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let flagged: Vec<String> = stdout_lines(&output)
        .into_iter()
        .filter(|l| l.starts_with("overlap:") || l.starts_with("missing:") || l.starts_with("phantom:"))
        .collect();
    assert_eq!(
        flagged,
        vec!["phantom: g-mesh-wire frames::ghost in only".to_owned()],
        "{}",
        describe(&output)
    );
}

#[test]
fn a_section_holds_only_the_tests_its_filter_matches() {
    // `cargo nextest list -E` lists every test and marks the unmatched ones
    // `mismatch`; those are not the section's. Counting them would put A and B
    // in both sections.
    let dir = tempfile::tempdir().unwrap();
    let all = write_list(dir.path(), "all.json", &matching(&[A, B]));
    let one = write_list(dir.path(), "one.json", &[(A.0, A.1, "matches"), (B.0, B.1, "mismatch")]);
    let two = write_list(dir.path(), "two.json", &[(A.0, A.1, "mismatch"), (B.0, B.1, "matches")]);

    let output = run_checker(&all, &[("one", &one), ("two", &two)]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let lines = stdout_lines(&output);
    assert!(lines.contains(&"one: 1".to_owned()), "{}", describe(&output));
    assert!(lines.contains(&"two: 1".to_owned()), "{}", describe(&output));
}

#[test]
fn the_checker_refuses_a_call_without_sections_or_with_a_malformed_section() {
    let dir = tempfile::tempdir().unwrap();
    let all = write_list(dir.path(), "all.json", &matching(&[A]));

    let output = run_checker(&all, &[]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));

    let output = Command::new("python3")
        .arg(repo_root().join("scripts/test-sections-check.py"))
        .arg(&all)
        .arg(&all)
        .output()
        .expect("failed to run python3");
    assert_eq!(
        output.status.code(),
        Some(2),
        "a section without name= must be refused\n{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------
// scripts/test-sections.sh

/// A `PATH` whose `cargo` only records that it was called, in `<dir>/cargo-called`.
fn recording_cargo(dir: &Path) -> String {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    fs::write(
        &cargo,
        format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", dir.join("cargo-called").display()),
    )
    .unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

fn sections_script(path_env: &str, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/test-sections.sh"))
        .args(args)
        .env("PATH", path_env)
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .expect("failed to run bash")
}

#[test]
fn list_prints_the_twelve_sections_in_run_order() {
    let dir = tempfile::tempdir().unwrap();
    let output = sections_script(&recording_cargo(dir.path()), &["list"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(stdout_lines(&output), SECTIONS.map(str::to_owned).to_vec(), "{}", describe(&output));
}

#[test]
fn filter_prints_a_filterset_for_every_listed_section_and_refuses_an_unknown_one() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());
    for name in SECTIONS {
        let output = sections_script(&path_env, &["filter", name]);
        assert_eq!(output.status.code(), Some(0), "{name}\n{}", describe(&output));
        let filter = String::from_utf8_lossy(&output.stdout);
        assert!(filter.contains("package("), "{name}: {filter}");
    }

    let output = sections_script(&path_env, &["filter", "core-nope"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    let stderr = String::from_utf8_lossy(&output.stderr);
    for name in SECTIONS {
        assert!(stderr.contains(name), "the refusal should name {name}\n{}", describe(&output));
    }
}

#[test]
fn run_refuses_an_unknown_or_missing_section_before_calling_cargo() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());

    // The unknown name comes after a valid one: nothing may run.
    let output = sections_script(&path_env, &["run", "core-mcp", "core-nope"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("core-nope"), "{}", describe(&output));

    let output = sections_script(&path_env, &["run"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));

    assert!(
        !dir.path().join("cargo-called").exists(),
        "cargo ran: {}",
        fs::read_to_string(dir.path().join("cargo-called")).unwrap_or_default()
    );
}

/// `sections_script` on a host whose `uname -s` is `uname`, with an empty
/// target dir so the real script's object pruning has nothing to touch.
fn sections_on(uname: &str, dir: &Path, path_env: &str, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(repo_root().join("scripts/test-sections.sh"))
        .args(args)
        .env("PATH", path_env)
        .env("TEST_SECTIONS_UNAME", uname)
        .env("CARGO_TARGET_DIR", dir.join("empty-target"))
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .expect("failed to run bash")
}

fn cargo_calls(dir: &Path) -> String {
    fs::read_to_string(dir.join("cargo-called")).unwrap_or_default()
}

const WINDOWS_UNAMES: [&str; 3] = ["MINGW64_NT-10.0-20348", "MSYS_NT-10.0", "CYGWIN_NT-10.0"];

#[test]
fn on_windows_run_core_scripts_skips_with_a_skip_line_and_never_runs_cargo_tests() {
    for uname in WINDOWS_UNAMES {
        let dir = tempfile::tempdir().unwrap();
        let path_env = recording_cargo(dir.path());

        let output = sections_on(uname, dir.path(), &path_env, &["run", "core-scripts"]);
        assert_eq!(output.status.code(), Some(0), "{uname}: {}", describe(&output));
        assert!(
            stdout_lines(&output).iter().any(|l| l.starts_with("== core-scripts: SKIP")),
            "{uname}: {}",
            describe(&output)
        );
        // The `nextest --version` probe is allowed; running tests is not.
        let calls = cargo_calls(dir.path());
        assert!(!calls.contains("nextest run"), "{uname}: cargo ran tests: {calls}");
    }
}

#[test]
fn on_windows_a_section_with_tests_still_runs_with_no_tests_fail() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());

    let output = sections_on(WINDOWS_UNAMES[0], dir.path(), &path_env, &["run", "core-it"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("SKIP"), "{}", describe(&output));
    let calls = cargo_calls(dir.path());
    assert!(calls.contains("nextest run"), "{calls}");
    assert!(calls.contains("--no-tests=fail"), "{calls}");
}

#[test]
fn on_windows_run_all_skips_only_core_scripts() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());

    let output = sections_on(WINDOWS_UNAMES[0], dir.path(), &path_env, &["run", "all"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let skipped: Vec<String> = stdout_lines(&output).into_iter().filter(|l| l.contains("SKIP")).collect();
    assert_eq!(skipped.len(), 1, "{}", describe(&output));
    assert!(skipped[0].starts_with("== core-scripts: SKIP"), "{}", describe(&output));
    let runs = cargo_calls(dir.path()).lines().filter(|l| l.contains("nextest run")).count();
    assert_eq!(runs, SECTIONS.len() - 1, "{}", cargo_calls(dir.path()));
}

#[test]
fn on_unix_run_core_scripts_calls_cargo_with_no_tests_fail() {
    for uname in ["Linux", "Darwin"] {
        let dir = tempfile::tempdir().unwrap();
        let path_env = recording_cargo(dir.path());

        let output = sections_on(uname, dir.path(), &path_env, &["run", "core-scripts"]);
        assert_eq!(output.status.code(), Some(0), "{uname}: {}", describe(&output));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("SKIP"), "{uname}: {}", describe(&output));
        let calls = cargo_calls(dir.path());
        assert!(calls.contains("binary_id(g-mesh::scripts)"), "{uname}: {calls}");
        assert!(calls.contains("--no-tests=fail"), "{uname}: {calls}");
    }
}

/// A `cargo` that answers `nextest list` for a two-test suite (one scripts
/// test, one `mcp::` lib test) by evaluating just the section filters it is
/// given, so `check` can run without building anything.
fn listing_cargo(dir: &Path) -> String {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    let script = r#"#!/bin/sh
case "$*" in
*--version*) exit 0 ;;
esac
filter=""
while [ $# -gt 0 ]; do
	[ "$1" != -E ] || filter="$2"
	shift
done
scripts='"g-mesh::scripts":{"binary-id":"g-mesh::scripts","testcases":{"t":{"ignored":false,"filter-match":{"status":"matches"}}}}'
mcp='"g-mesh":{"binary-id":"g-mesh","testcases":{"mcp::t":{"ignored":false,"filter-match":{"status":"matches"}}}}'
case "$filter" in
"") suites="$scripts,$mcp" ;;
*'!binary_id(g-mesh::scripts)'*) suites="" ;;
*'binary_id(g-mesh::scripts)'*) suites="$scripts" ;;
*'test(/^mcp::/)'*) suites="$mcp" ;;
*) suites="" ;;
esac
printf '{"rust-build-meta":{},"test-count":0,"rust-suites":{%s}}\n' "$suites"
"#;
    fs::write(&cargo, script).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

#[test]
fn check_on_windows_still_counts_core_scripts_tests() {
    for uname in [WINDOWS_UNAMES[0], "Linux"] {
        let dir = tempfile::tempdir().unwrap();
        let path_env = listing_cargo(dir.path());

        let output = sections_on(uname, dir.path(), &path_env, &["check"]);
        assert_eq!(output.status.code(), Some(0), "{uname}: {}", describe(&output));
        let lines = stdout_lines(&output);
        assert!(lines.contains(&"core-scripts: 1".to_owned()), "{uname}: {}", describe(&output));
        assert!(lines.contains(&"core-mcp: 1".to_owned()), "{uname}: {}", describe(&output));
        assert!(
            lines.contains(&"OK: every test is in exactly one section".to_owned()),
            "{uname}: {}",
            describe(&output)
        );
    }
}

/// `core-it` holds core's `kind(test)` binaries but `g-mesh::scripts`, which
/// `core-scripts` holds alone.
#[test]
fn core_scripts_is_the_scripts_binary_and_core_it_is_every_other_core_test_binary() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());
    let filter_of = |name: &str| {
        let output = sections_script(&path_env, &["filter", name]);
        assert_eq!(output.status.code(), Some(0), "{name}\n{}", describe(&output));
        String::from_utf8_lossy(&output.stdout).trim_end().to_owned()
    };
    assert_eq!(filter_of("core-scripts"), "package(g-mesh) & binary_id(g-mesh::scripts)");
    assert_eq!(filter_of("core-it"), "package(g-mesh) & kind(test) & !binary_id(g-mesh::scripts)");
}

/// A section nobody runs in CI drops its tests silently: each listed section
/// has exactly one `tests: <section>` step, whose `if:` selects it by name.
#[test]
fn every_listed_section_has_exactly_one_ci_step_that_runs_it_when_selected() {
    let dir = tempfile::tempdir().unwrap();
    let listed = stdout_lines(&sections_script(&recording_cargo(dir.path()), &["list"]));
    assert_eq!(listed.len(), SECTIONS.len(), "{listed:?}");
    let ci = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let mut steps: Vec<Vec<&str>> = Vec::new();
    for line in ci.lines() {
        if line.trim_start().starts_with("- name:") {
            steps.push(Vec::new());
        }
        if let Some(step) = steps.last_mut() {
            step.push(line);
        }
    }
    for name in &listed {
        let run_line = format!("run: scripts/test-sections.sh run --profile ci {name}");
        let running: Vec<&Vec<&str>> =
            steps.iter().filter(|s| s.iter().any(|l| l.trim() == run_line)).collect();
        assert_eq!(
            running.len(),
            1,
            "section {name}: {} ci.yml steps run it, want exactly one",
            running.len()
        );
        let step = running[0];
        assert!(step[0].contains(&format!("\"tests: {name}\"")), "section {name}: step is `{}`", step[0]);
        let condition = step.iter().find(|l| l.trim_start().starts_with("if:")).copied().unwrap_or("");
        assert!(
            condition.contains(&format!("' {name} '")),
            "section {name}: its step's `if:` is `{condition}`"
        );
    }
}

/// A scratch checkout: the real `scripts/test-sections.sh` and `map`, when
/// given, as `scripts/test-plugin-spawns.txt`.
fn sections_root(map: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("scripts/test-sections.sh");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    for name in ["test-sections.sh", "prune-stale-objects.sh"] {
        fs::copy(repo_root().join("scripts").join(name), dir.path().join("scripts").join(name)).unwrap();
    }
    if let Some(map) = map {
        fs::write(dir.path().join("scripts/test-plugin-spawns.txt"), map).unwrap();
    }
    dir
}

fn sections_in(root: &Path, path_env: &str, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(root.join("scripts/test-sections.sh"))
        .args(args)
        .env("PATH", path_env)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env_remove("GITHUB_STEP_SUMMARY")
        .output()
        .expect("failed to run bash")
}

const SPAWN_MAP: &str = "# comment\n\
    it:alpha typescript rust\n\
    it:beta   typescript   # trailing comment\n\
    it:gamma none\n\
    lib:m::tests typescript go\n\
    scripts:s rust\n";

fn spawns_filter(root: &Path, plugin: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let output =
        sections_in(root, &recording_cargo(dir.path()), &["filter", &format!("core-spawns-{plugin}")]);
    assert_eq!(output.status.code(), Some(0), "{plugin}\n{}", describe(&output));
    String::from_utf8_lossy(&output.stdout).trim_end().to_owned()
}

/// The filter names exactly the modules whose row lists the plugin, per
/// binary; a plugin no row lists selects nothing.
#[test]
fn a_core_spawns_filter_names_the_modules_whose_map_rows_list_that_plugin() {
    let root = sections_root(Some(SPAWN_MAP));
    let it = |mods: &str| format!("(binary_id(g-mesh::it) & test(/^({mods})::/))");
    assert_eq!(
        spawns_filter(root.path(), "typescript"),
        format!("package(g-mesh) & ({} | (kind(lib) & test(/^(m::tests)::/)))", it("alpha|beta"))
    );
    assert_eq!(
        spawns_filter(root.path(), "rust"),
        format!("package(g-mesh) & ({} | (binary_id(g-mesh::scripts) & test(/^(s)::/)))", it("alpha"))
    );
    assert_eq!(spawns_filter(root.path(), "go"), "package(g-mesh) & ((kind(lib) & test(/^(m::tests)::/)))");
    assert_eq!(spawns_filter(root.path(), "python"), "package(g-mesh) & (none())");
}

#[test]
fn a_core_spawns_filter_refuses_a_missing_map_or_a_row_of_another_binary() {
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());

    let root = sections_root(None);
    let output = sections_in(root.path(), &path_env, &["filter", "core-spawns-typescript"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot read"), "{}", describe(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "a filter was printed");

    let root = sections_root(Some("it:alpha typescript\nbogus:x typescript\n"));
    let output = sections_in(root.path(), &path_env, &["filter", "core-spawns-typescript"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("bad row"), "{}", describe(&output));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "a filter was printed");
}

/// The selectors overlap sections, so they are not listed (the partition check
/// would see two owners); `run` takes them, and the refusal of an unknown name
/// says they exist.
#[test]
fn core_spawns_selectors_are_runnable_but_not_listed_sections() {
    let root = sections_root(Some(SPAWN_MAP));
    let dir = tempfile::tempdir().unwrap();
    let path_env = recording_cargo(dir.path());

    let listed = stdout_lines(&sections_in(root.path(), &path_env, &["list"]));
    assert_eq!(listed, SECTIONS.map(str::to_owned).to_vec());

    let output = sections_in(root.path(), &path_env, &["run", "core-spawns-typescript"]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let calls = fs::read_to_string(dir.path().join("cargo-called")).unwrap();
    let runs: Vec<&str> = calls.lines().filter(|l| l.starts_with("nextest run")).collect();
    assert_eq!(runs.len(), 1, "{calls}");
    assert!(runs[0].contains("alpha|beta"), "{calls}");

    let output = sections_in(root.path(), &path_env, &["filter", "core-nope"]);
    assert_eq!(output.status.code(), Some(2), "{}", describe(&output));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("core-spawns-<plugin>"),
        "{}",
        describe(&output)
    );
}

/// The real tree: lists the whole workspace suite once plus once per section,
/// on the current build. Ignored because that is twelve full listings; CI
/// runs the same `scripts/test-sections.sh check` as its own step on the
/// linux row, and cut-release runs it before waiting for CI. Run it with
/// `cargo nextest run -p g-mesh --test scripts -E 'test(/^test_sections_scripts::/)' --run-ignored only`.
#[test]
#[ignore = "lists the whole suite 12 times; CI's coverage-check step runs the same check"]
fn the_real_sections_partition_the_real_suite() {
    let mut cmd = Command::new("bash");
    cmd.arg(repo_root().join("scripts/test-sections.sh")).arg("check");
    // The outer nextest run's own settings (profile, run id) must not steer
    // the inner listings.
    for (key, _) in std::env::vars() {
        if key.starts_with("NEXTEST") {
            cmd.env_remove(key);
        }
    }
    let output = cmd.output().expect("failed to run bash");
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        stdout_lines(&output).contains(&"OK: every test is in exactly one section".to_owned()),
        "{}",
        describe(&output)
    );
}

// ---------------------------------------------------------------------------
// .config/nextest.toml [profile.local]

/// The `(binary_id(B) & test(=N))` terms of `[profile.local]`, as (B, N).
fn local_profile_heavy_terms() -> Vec<(String, String)> {
    let config = fs::read_to_string(repo_root().join(".config/nextest.toml")).unwrap();
    let start = config.find("[profile.local]").expect("no [profile.local] in .config/nextest.toml");
    let section = &config[start..];
    let section = match section[1..].find("\n[") {
        Some(end) => &section[..end + 1],
        None => section,
    };
    let mut terms = Vec::new();
    for line in section.lines() {
        let Some(rest) = line.split_once("binary_id(").map(|(_, r)| r) else { continue };
        let (binary, rest) = rest.split_once(')').unwrap();
        let name = rest.split_once("test(=").unwrap().1.split_once(')').unwrap().0;
        terms.push((binary.trim().to_owned(), name.trim().to_owned()));
    }
    terms
}

/// The tests `cargo nextest list --workspace --profile <profile>` runs, as
/// (binary id, name).
fn listed_under(profile: &str) -> std::collections::BTreeSet<(String, String)> {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(repo_root()).args([
        "nextest",
        "list",
        "--workspace",
        "--message-format",
        "json",
        "--profile",
        profile,
    ]);
    for (key, _) in std::env::vars() {
        if key.starts_with("NEXTEST") {
            cmd.env_remove(key);
        }
    }
    let output = cmd.output().expect("failed to run cargo");
    assert!(output.status.success(), "{}", describe(&output));
    let list: Value = serde_json::from_slice(&output.stdout).unwrap();
    let mut tests = std::collections::BTreeSet::new();
    for suite in list["rust-suites"].as_object().unwrap().values() {
        let binary = suite["binary-id"].as_str().unwrap();
        for (name, case) in suite["testcases"].as_object().unwrap() {
            if case["filter-match"]["status"] == "matches" {
                tests.insert((binary.to_owned(), name.clone()));
            }
        }
    }
    tests
}

/// The real tree: what the `local` profile skips is exactly its heavy list,
/// each term one existing test, and the two containers random tests (run
/// locally with fewer seeds) are never skipped. Ignored because it lists the
/// whole suite twice; run it with `cargo nextest run -p g-mesh --test
/// test_sections_scripts --run-ignored only`.
#[test]
#[ignore = "lists the whole suite twice"]
fn the_local_profile_skips_exactly_its_heavy_list() {
    let terms = local_profile_heavy_terms();
    let unique: std::collections::BTreeSet<_> = terms.iter().cloned().collect();
    assert_eq!(unique.len(), terms.len(), "a heavy term is listed twice: {terms:#?}");
    assert!(!terms.is_empty(), "no heavy terms parsed");

    let default = listed_under("default");
    let local = listed_under("local");
    assert!(
        local.is_subset(&default),
        "local runs a test default does not: {:#?}",
        local.difference(&default)
    );
    let skipped: std::collections::BTreeSet<_> = default.difference(&local).cloned().collect();
    assert_eq!(skipped, unique, "default minus local is not the heavy list");
    for name in [
        "graph::containers::tests::membership_invariants_hold_after_every_diff_of_a_random_sequence",
        "graph::containers::tests::bulk_batch_boundaries_do_not_change_the_result",
    ] {
        let key = ("g-mesh".to_owned(), name.to_owned());
        assert!(default.contains(&key), "{name} is not in the suite");
        assert!(local.contains(&key), "{name} is skipped locally; it should run with fewer seeds");
    }
}
