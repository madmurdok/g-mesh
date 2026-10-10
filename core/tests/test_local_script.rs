//! `scripts/test-local.sh` runs the selected sections in two parts (core lib
//! tests in one libtest process, the rest under nextest's `local` profile),
//! and `--full` runs every section under the `ci` profile, then
//! `scripts/test-heavy-report.py`. Why: docs/adr/0031-local-test-runs.md.
//!
//! The script runs for real with a stand-in `cargo` on `PATH` that records
//! each call (its arguments and `G_MESH_CONTAINERS_SEEDS`) and answers
//! `nextest list` with a fixed listing, so no cargo work is paid for. The
//! selection (`scripts/test-select.sh`) and the section filters
//! (`scripts/test-sections.sh filter`) are the real ones. The heavy-list
//! report runs for real on fixture files. Bash and python3 only, so Unix
//! only, like `test_sections_scripts.rs`.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

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

// ---------------------------------------------------------------------------
// scripts/test-local.sh

/// The stand-in `cargo`. Appends one JSON line per call to `$FAKE_CARGO_LOG`.
/// `nextest list` prints a listing with two matching lib tests, one lib test
/// the profile filtered out, and one integration test. `test` exits
/// `$FAKE_CARGO_TEST_EXIT` (default 0). A `nextest run --profile ci` leaves a
/// JUnit file where nextest would.
const FAKE_CARGO: &str = r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.environ["FAKE_CARGO_LOG"], "a") as log:
    log.write(json.dumps({"args": args, "seeds": os.environ.get("G_MESH_CONTAINERS_SEEDS")}) + "\n")
if args[:2] == ["nextest", "list"]:
    matches = {"filter-match": {"status": "matches"}}
    print(json.dumps({"rust-suites": {
        "g-mesh": {"binary-id": "g-mesh", "testcases": {
            "cli::a": matches,
            "cli::b": matches,
            "cli::heavy": {"filter-match": {"status": "mismatch", "reason": "default-filter"}},
        }},
        "g-mesh::cli_init": {"binary-id": "g-mesh::cli_init", "testcases": {"c": matches}},
    }}))
elif args[:1] == ["test"]:
    code = int(os.environ.get("FAKE_CARGO_TEST_EXIT", "0"))
    if code == 0:
        print("test result: ok. 2 passed; 0 failed; 0 ignored")
    else:
        print("test result: FAILED. 1 passed; 1 failed; 0 ignored")
    sys.exit(code)
elif args[:2] == ["nextest", "run"] and "--no-run" not in args:
    if args[args.index("--profile") + 1] == "ci":
        junit = os.path.join(os.environ["CARGO_TARGET_DIR"], "nextest", "ci")
        os.makedirs(junit, exist_ok=True)
        with open(os.path.join(junit, "junit.xml"), "w") as f:
            f.write('<testsuites tests="1" failures="0" errors="0"><testsuite name="g-mesh">'
                    '<testcase classname="g-mesh" name="x" time="0.1"/></testsuite></testsuites>')
    print("     Summary [   0.1s] 1 test run: 1 passed, 0 skipped")
"#;

/// One recorded `cargo` call.
struct Call {
    args: Vec<String>,
    seeds: Option<String>,
}

impl Call {
    fn starts_with(&self, prefix: &[&str]) -> bool {
        self.args.len() >= prefix.len() && self.args.iter().zip(prefix).all(|(a, p)| a == p)
    }

    /// The value after `flag`.
    fn value_of(&self, flag: &str) -> &str {
        let at = self
            .args
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag} in {:?}", self.args));
        &self.args[at + 1]
    }
}

/// Runs `scripts/test-local.sh args` with the stand-in cargo and
/// `extra_env`, returning its output and the recorded calls.
fn run_local(args: &[&str], extra_env: &[(&str, &str)]) -> (Output, Vec<Call>) {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let cargo = bin.join("cargo");
    fs::write(&cargo, FAKE_CARGO).unwrap();
    fs::set_permissions(&cargo, fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.path().join("cargo-calls.jsonl");

    let mut cmd = Command::new("bash");
    cmd.arg(repo_root().join("scripts/test-local.sh"))
        .args(args)
        .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
        .env("CARGO_TARGET_DIR", dir.path().join("target"))
        .env("FAKE_CARGO_LOG", &log)
        .env_remove("G_MESH_CONTAINERS_SEEDS")
        .env_remove("FAKE_CARGO_TEST_EXIT")
        .env_remove("GITHUB_STEP_SUMMARY");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("failed to run bash");
    let calls = fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let call: Value = serde_json::from_str(line).unwrap();
            Call {
                args: call["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_owned())
                    .collect(),
                seeds: call["seeds"].as_str().map(str::to_owned),
            }
        })
        .collect();
    (output, calls)
}

/// A `--paths-from` file listing `paths`, kept alive by the returned dir.
fn paths_file(paths: &[&str]) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("paths.txt");
    fs::write(&file, paths.iter().map(|p| format!("{p}\n")).collect::<String>()).unwrap();
    let file = file.to_str().unwrap().to_owned();
    (dir, file)
}

fn section_filter(section: &str) -> String {
    let output = Command::new("bash")
        .arg(repo_root().join("scripts/test-sections.sh"))
        .args(["filter", section])
        .output()
        .expect("failed to run bash");
    assert!(output.status.success(), "{}", describe(&output));
    stdout_of(&output).trim_end().to_owned()
}

fn calls_of<'a>(calls: &'a [Call], prefix: &[&str]) -> Vec<&'a Call> {
    calls.iter().filter(|c| c.starts_with(prefix)).collect()
}

/// The `nextest run` calls that run tests (not the `--no-run` build).
fn test_runs(calls: &[Call]) -> Vec<&Call> {
    calls_of(calls, &["nextest", "run"])
        .into_iter()
        .filter(|c| !c.args.iter().any(|a| a == "--no-run"))
        .collect()
}

#[test]
fn a_none_selection_reports_no_sections_and_never_calls_cargo() {
    let (_dir, file) = paths_file(&["docs/a.md"]);
    let (output, calls) = run_local(&["--paths-from", &file], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(stdout_of(&output).contains("== no sections selected"), "{}", describe(&output));
    assert!(calls.is_empty(), "cargo was called: {:?}", calls.iter().map(|c| &c.args).collect::<Vec<_>>());
}

/// The local run passes `--narrow`: a cli-only change selects `core-cli` and
/// `core-it`, not every core section as CI's call does.
#[test]
fn a_cli_change_selects_only_the_cli_section_and_the_integration_tests() {
    let (_dir, file) = paths_file(&["core/src/cli/status.rs"]);
    let (output, _) = run_local(&["--paths-from", &file], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        stdout_of(&output).lines().any(|l| l == "== selected: core-it core-cli"),
        "{}",
        describe(&output)
    );
}

#[test]
fn the_lib_part_runs_the_listed_names_in_one_process_and_nextest_runs_the_rest() {
    let (_dir, file) = paths_file(&["core/src/cli/status.rs"]);
    let (output, calls) = run_local(&["--paths-from", &file], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));

    // The names come from a `local`-profile listing; the one the profile
    // filtered out and the other binary's test are not passed.
    let lists = calls_of(&calls, &["nextest", "list"]);
    assert_eq!(lists.len(), 1, "{:?}", calls.iter().map(|c| &c.args).collect::<Vec<_>>());
    assert_eq!(lists[0].value_of("--profile"), "local");
    let lib_filter = lists[0].value_of("-E");
    let tests = calls_of(&calls, &["test"]);
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0].args, ["test", "--workspace", "--lib", "--", "--exact", "cli::a", "cli::b"]);
    assert_eq!(tests[0].seeds.as_deref(), Some("2"));

    // The lib filter is the core-cli section's lib tests; nextest runs the
    // union of the selected sections minus exactly that filter.
    assert!(
        lib_filter.starts_with(&format!(
            "package(g-mesh) & kind(lib) & (({})) & not (",
            section_filter("core-cli")
        )),
        "{lib_filter}"
    );
    let union = format!("({}) | ({})", section_filter("core-it"), section_filter("core-cli"));
    let runs = test_runs(&calls);
    assert_eq!(runs.len(), 1, "{:?}", calls.iter().map(|c| &c.args).collect::<Vec<_>>());
    assert_eq!(runs[0].value_of("--profile"), "local");
    assert_eq!(runs[0].value_of("-E"), format!("({union}) & not ({lib_filter})"));
    assert_eq!(runs[0].seeds.as_deref(), Some("2"));

    let stdout = stdout_of(&output);
    assert!(stdout.contains("== lib (one process): PASS, 2 passed, 0 failed"), "{}", describe(&output));
    assert!(stdout.contains("== nextest: PASS"), "{}", describe(&output));
}

#[test]
fn per_process_makes_no_cargo_test_call_and_runs_every_selected_test_under_nextest() {
    let (_dir, file) = paths_file(&["core/src/cli/status.rs"]);
    let (output, calls) = run_local(&["--per-process", "--paths-from", &file], &[]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(
        calls_of(&calls, &["test"]).is_empty(),
        "{:?}",
        calls.iter().map(|c| &c.args).collect::<Vec<_>>()
    );
    let runs = test_runs(&calls);
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].value_of("--profile"), "local");
    assert_eq!(
        runs[0].value_of("-E"),
        format!("({}) | ({})", section_filter("core-it"), section_filter("core-cli"))
    );
}

#[test]
fn a_failing_lib_part_fails_the_run_and_skips_nextest_unless_keep_going() {
    let (_dir, file) = paths_file(&["core/src/cli/status.rs"]);
    let failing = [("FAKE_CARGO_TEST_EXIT", "101")];

    let (output, calls) = run_local(&["--paths-from", &file], &failing);
    assert_ne!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(calls_of(&calls, &["test"]).len(), 1);
    assert!(test_runs(&calls).is_empty(), "nextest ran after a failed lib part");
    assert!(stdout_of(&output).contains("== lib (one process): FAIL"), "{}", describe(&output));

    let (output, calls) = run_local(&["--keep-going", "--paths-from", &file], &failing);
    assert_ne!(output.status.code(), Some(0), "{}", describe(&output));
    assert_eq!(test_runs(&calls).len(), 1, "--keep-going did not run nextest");
}

#[test]
fn full_runs_every_section_under_the_ci_profile_with_every_seed_then_the_report() {
    let (output, calls) = run_local(&["--full"], &[("G_MESH_CONTAINERS_SEEDS", "2")]);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let runs = test_runs(&calls);
    assert_eq!(runs.len(), 11, "{:?}", calls.iter().map(|c| &c.args).collect::<Vec<_>>());
    for run in &runs {
        assert_eq!(run.value_of("--profile"), "ci", "{:?}", run.args);
        assert_eq!(run.seeds, None, "G_MESH_CONTAINERS_SEEDS reached a full run: {:?}", run.args);
    }
    assert!(calls_of(&calls, &["test"]).is_empty());
    assert!(stdout_of(&output).contains("== heavy list: "), "{}", describe(&output));
}

#[test]
fn full_takes_no_other_option() {
    let (_dir, file) = paths_file(&["core/src/cli/status.rs"]);
    for extra in [vec!["--per-process"], vec!["--dry-run"], vec!["--paths-from", file.as_str()]] {
        let mut args = vec!["--full"];
        args.extend(&extra);
        let (output, calls) = run_local(&args, &[]);
        assert_eq!(output.status.code(), Some(2), "{args:?}\n{}", describe(&output));
        assert!(calls.is_empty(), "{args:?}: cargo was called");
    }
}

// ---------------------------------------------------------------------------
// scripts/test-heavy-report.py

const HEAVY_CONFIG: &str = r#"[profile.default]
slow-timeout = "60s"

[profile.local]
inherits = "default"
default-filter = """
not (
    (binary_id(g-mesh) & test(=listed::fast)) |
    (binary_id(g-mesh) & test(=listed::slow)) |
    (binary_id(g-mesh) & test(=listed::at_threshold)) |
    (binary_id(g-mesh::cli_init) & test(=listed::gone))
)
"""

[profile.ci]
inherits = "default"
"#;

const HEAVY_JUNIT: &str = r#"<testsuites><testsuite name="g-mesh">
<testcase classname="g-mesh" name="listed::fast" time="5.0"/>
<testcase classname="g-mesh" name="listed::slow" time="20.0"/>
<testcase classname="g-mesh" name="listed::at_threshold" time="10.0"/>
<testcase classname="g-mesh" name="unlisted::slow" time="15.0"/>
<testcase classname="g-mesh" name="unlisted::at_threshold" time="10.0"/>
<testcase classname="g-mesh" name="unlisted::fast" time="9.9"/>
<testcase classname="g-mesh" name="graph::containers::tests::membership_invariants_hold_after_every_diff_of_a_random_sequence" time="300.0"/>
<testcase classname="g-mesh" name="graph::containers::tests::bulk_batch_boundaries_do_not_change_the_result" time="60.0"/>
</testsuite></testsuites>
"#;

fn heavy_report(config: &str, junit: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("nextest.toml"), config).unwrap();
    fs::write(dir.path().join("junit-core-it.xml"), junit).unwrap();
    Command::new("python3")
        .arg(repo_root().join("scripts/test-heavy-report.py"))
        .arg(dir.path().join("nextest.toml"))
        .arg(dir.path().join("junit-core-it.xml"))
        .output()
        .expect("failed to run python3")
}

/// The lines under the group header starting with `header`, trimmed.
fn group(stdout: &str, header: &str) -> Vec<String> {
    let mut lines = stdout.lines().skip_while(|l| !l.starts_with(header));
    let first = lines.next().unwrap_or_else(|| panic!("no {header:?} group in\n{stdout}"));
    let count: usize = first.rsplit(' ').next().unwrap().parse().unwrap();
    let body: Vec<String> = lines.take_while(|l| l.starts_with(' ')).map(|l| l.trim().to_owned()).collect();
    assert_eq!(body.len(), count, "{header:?}: count and lines disagree\n{stdout}");
    body
}

#[test]
fn the_heavy_report_names_lighter_heavier_and_missing_tests() {
    let output = heavy_report(HEAVY_CONFIG, HEAVY_JUNIT);
    assert_eq!(output.status.code(), Some(0), "{}", describe(&output));
    let stdout = stdout_of(&output);
    let names = |header: &str| -> Vec<String> {
        group(&stdout, header).iter().map(|l| l.split_whitespace().last().unwrap().to_owned()).collect()
    };
    assert_eq!(names("== listed, now under"), ["listed::fast"]);
    // Heaviest first; the two containers tests run reduced locally, so their
    // full-run time never makes them candidates.
    assert_eq!(names("== not listed, now at or over"), ["unlisted::slow", "unlisted::at_threshold"]);
    assert_eq!(names("== listed, not in this run"), ["listed::gone"]);
    assert!(group(&stdout, "== listed, not in this run")[0].starts_with("g-mesh::cli_init "), "{stdout}");
}

#[test]
fn the_heavy_report_refuses_a_config_without_a_local_profile() {
    let output = heavy_report("[profile.default]\nslow-timeout = \"60s\"\n", HEAVY_JUNIT);
    assert_ne!(output.status.code(), Some(0), "{}", describe(&output));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no [profile.local]"), "{}", describe(&output));
}
