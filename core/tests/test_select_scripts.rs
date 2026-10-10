//! A change runs only the test sections it can affect (`scripts/test-select.sh`,
//! GM-539), and a release is cut only from a commit that passed the whole
//! suite (`scripts/cut-release.sh`'s `check_release_branch_ci_passed`).
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

const CORE: [&str; 6] = ["core-it", "core-mcp", "core-daemon", "core-cli", "core-graph", "core-rest"];

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

/// The six core sections followed by `extra`, space-separated.
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
/// change; only `scripts/test-local.sh`'s `--narrow` call narrows it.
#[test]
fn a_cli_or_mcp_change_runs_every_core_section_unless_narrowed() {
    let leaves =
        ["core/src/cli/status.rs", "core/src/cli/x/y.rs", "core/src/mcp/mod.rs", "core/src/mcp/x.rs"];
    assert_each(&leaves, &core_plus(&[]));

    let narrow = |paths: &[&str]| {
        select_raw_with(&["--narrow"], &paths.iter().map(|p| format!("{p}\n")).collect::<String>())
    };
    assert_eq!(narrow(&["core/src/cli/status.rs"]), "core-it core-cli");
    assert_eq!(narrow(&["core/src/cli/x/y.rs"]), "core-it core-cli");
    assert_eq!(narrow(&["core/src/mcp/mod.rs"]), "core-it core-mcp");
    assert_eq!(narrow(&["core/src/cli/a.rs", "core/src/mcp/b.rs"]), "core-it core-mcp core-cli");
    // Any other core path still selects every core section, narrowed or not.
    for other in ["core/src/graph/x.rs", "core/src/lib.rs", "core/tests/foo.rs", "core/Cargo.toml"] {
        assert_eq!(narrow(&[other]), core_plus(&[]), "path {other}");
        assert_eq!(narrow(&["core/src/cli/a.rs", other]), core_plus(&[]), "path {other} with a cli path");
    }
    // The flag's position does not matter.
    let dir = tempfile::tempdir().unwrap();
    let list = dir.path().join("paths.txt");
    fs::write(&list, "core/src/cli/a.rs\n").unwrap();
    let output = select_script(&repo_root(), &["--paths-from", list.to_str().unwrap(), "--narrow"]);
    assert_eq!(stdout_of(&output), "core-it core-cli\n", "{}", describe(&output));
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
    assert_each(&["scripts/install.sh", "scripts/cut-release.sh"], "core-it core-cli");
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
    assert_eq!(select(&["README.md", "scripts/install.sh"]), "core-it core-cli");
    assert_eq!(select(&["core/src/lib.rs", "wire/src/lib.rs"]), "full");
}

#[test]
fn crlf_lines_and_blank_lines_are_tolerated() {
    assert_eq!(select_raw("\r\nREADME.md\r\n\r\n\nscripts/install.sh\r\n"), "core-it core-cli");
    assert_eq!(select_raw("docs/a.md\r\n"), "none");
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
    assert_eq!(run_calls(&full_calls).len(), 11, "{full_calls}");
    assert_eq!(run_calls(&full_calls), run_calls(&all_calls));
}

// ---------------------------------------------------------------------------
// scripts/cut-release.sh check_release_branch_ci_passed

const VERSION: &str = "9.9.9";

/// A scratch release: `release-9.9.9` pushed to a bare `origin` and merged
/// into `main` with the subject the gate expects, plus a stand-in `gh` that
/// answers `gh api <endpoint> --jq <filter>` by running the real `jq` on
/// `<fixtures>/runs.json` or `<fixtures>/jobs-<run id>.json`.
struct Release {
    dir: tempfile::TempDir,
}

impl Release {
    fn new() -> Self {
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
case "$2" in
*/workflows/ci.yml/runs*) file=runs.json ;;
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

    /// ci.yml runs on the tip, as (run id, conclusion).
    fn runs(&self, runs: &[(u64, &str)]) {
        let runs: Vec<_> = runs.iter().map(|(id, c)| json!({ "id": id, "conclusion": c })).collect();
        fs::write(self.dir.path().join("fixtures/runs.json"), json!({ "workflow_runs": runs }).to_string())
            .unwrap();
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
        Command::new("bash")
            .arg("-c")
            .arg(r#"source "$1" && REPO_ROOT="$2" && check_release_branch_ci_passed "$3""#)
            .arg("cut-release-test")
            .arg(repo_root().join("scripts/cut-release.sh"))
            .arg(self.dir.path().join("repo"))
            .arg(VERSION)
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
    assert!(stderr_of(&output).contains("no successful ci.yml run"), "{}", describe(&output));
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
