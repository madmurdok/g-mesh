//! A push to `main` whose tree already passed a full ci.yml run reuses that
//! run (GM-729): `scripts/ci-green-for-commit.sh` finds the run, and ci.yml's
//! `select` step turns its answer into `mode`/`reused` and the run summary.
//!
//! Both run for real against a stand-in `gh` that answers
//! `gh api [--paginate] <endpoint> --jq <filter>` by running the real `jq` on
//! fixture JSON, so the script's own filters decide. The `select` step is the
//! `run: |` block read out of ci.yml, run by bash from the repository root
//! with the environment GitHub gives it. Bash, git and jq only, so Unix only.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::json;

const SLUG: &str = "owner/repo";
const FULL_JOB: &str = "Full test suite ran";

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

/// A 40-hex-digit commit or tree id made of one repeated digit.
fn sha(digit: char) -> String {
    std::iter::repeat_n(digit, 40).collect()
}

/// Fixtures for the stand-in `gh`: `commit-<sha>.json`, `runs-<sha>.json`,
/// `jobs-<run id>.json`. A missing fixture answers like a 404 (exit 1); a
/// `fail` file makes every call fail like a bad token. Every endpoint asked
/// is appended to `calls`.
struct Gh {
    dir: tempfile::TempDir,
}

impl Gh {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fixtures = dir.path().join("fixtures");
        fs::create_dir_all(&fixtures).unwrap();
        let gh = dir.path().join("bin/gh");
        fs::create_dir_all(gh.parent().unwrap()).unwrap();
        fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
# gh api [--paginate] <endpoint> --jq <filter>
f="{fixtures}"
[ "$1" = api ] || {{ echo "fake gh: unexpected call: $*" >&2; exit 99; }}
shift
[ "$1" = --paginate ] && shift
[ "$2" = --jq ] || {{ echo "fake gh: unexpected call: $*" >&2; exit 99; }}
echo "$1" >> "$f/calls"
[ -f "$f/fail" ] && {{ echo "fake gh: HTTP 401" >&2; exit 1; }}
case "$1" in
*/git/commits/*) file="commit-${{1##*/}}.json" ;;
*/actions/workflows/ci.yml/runs*) s="${{1#*head_sha=}}"; file="runs-${{s%%&*}}.json" ;;
*/actions/runs/*/jobs*) id="${{1#*/actions/runs/}}"; file="jobs-${{id%%/*}}.json" ;;
*) echo "fake gh: unexpected endpoint $1" >&2; exit 99 ;;
esac
[ -f "$f/$file" ] || {{ echo "fake gh: HTTP 404 ($file)" >&2; exit 1; }}
exec jq -r "$3" "$f/$file"
"#,
                fixtures = fixtures.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
        Gh { dir }
    }

    fn write(&self, name: &str, value: serde_json::Value) {
        fs::write(self.dir.path().join("fixtures").join(name), value.to_string()).unwrap();
    }

    fn commit(&self, sha: &str, tree: &str, parents: &[&str]) {
        let parents: Vec<_> = parents.iter().map(|p| json!({ "sha": p })).collect();
        self.write(
            &format!("commit-{sha}.json"),
            json!({ "sha": sha, "tree": { "sha": tree }, "parents": parents }),
        );
    }

    /// Completed runs of `sha`, as (run id, conclusion).
    fn runs(&self, sha: &str, runs: &[(u64, &str)]) {
        let runs: Vec<_> = runs
            .iter()
            .map(|(id, c)| json!({ "id": id, "head_sha": sha, "status": "completed", "conclusion": c }))
            .collect();
        self.write(&format!("runs-{sha}.json"), json!({ "workflow_runs": runs }));
    }

    /// The jobs of run `id`, as (name, conclusion).
    fn jobs(&self, id: u64, jobs: &[(&str, &str)]) {
        let jobs: Vec<_> = jobs
            .iter()
            .enumerate()
            .map(|(i, (n, c))| json!({ "id": i + 1, "name": n, "conclusion": c }))
            .collect();
        self.write(&format!("jobs-{id}.json"), json!({ "jobs": jobs }));
    }

    fn full_jobs(&self, id: u64) {
        self.jobs(id, &[("tests (x86_64-unknown-linux-gnu)", "success"), (FULL_JOB, "success")]);
    }

    fn fail_every_call(&self) {
        fs::write(self.dir.path().join("fixtures/fail"), "").unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.dir.path().join("fixtures/calls")).unwrap_or_default()
    }

    fn path(&self) -> String {
        format!("{}:{}", self.dir.path().join("bin").display(), std::env::var("PATH").unwrap_or_default())
    }

    /// `scripts/ci-green-for-commit.sh <slug> <sha>`.
    fn gate(&self, sha: &str) -> Output {
        Command::new("bash")
            .arg(repo_root().join("scripts/ci-green-for-commit.sh"))
            .arg(SLUG)
            .arg(sha)
            .env("PATH", self.path())
            .env_remove("GH")
            .output()
            .expect("failed to run bash")
    }
}

/// The merge `M` on main whose release-side parent `T` has the same tree
/// (what a release merge looks like), and main's old tip `B` with another tree.
struct Merge {
    m: String,
    t: String,
    b: String,
}

fn release_merge(gh: &Gh) -> Merge {
    let (m, t, b) = (sha('a'), sha('b'), sha('c'));
    gh.commit(&m, &sha('1'), &[&b, &t]);
    gh.commit(&b, &sha('2'), &[]);
    gh.commit(&t, &sha('1'), &[]);
    gh.runs(&m, &[]);
    Merge { m, t, b }
}

const GREEN_FALSE: &str = "green=false\n";

// ---------------------------------------------------------------------------
// scripts/ci-green-for-commit.sh

#[test]
fn a_green_full_run_on_a_same_tree_parent_is_reused() {
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.b, &[]);
    gh.runs(&merge.t, &[(701, "success")]);
    gh.full_jobs(701);
    let output = gh.gate(&merge.m);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(
        stdout_of(&output),
        format!("green=true\nrun=701\nsha={}\n", merge.t),
        "{}",
        describe(&output)
    );
}

#[test]
fn no_green_full_run_on_the_commit_or_a_same_tree_parent_is_not_reused() {
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.t, &[]);
    let output = gh.gate(&merge.m);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(stdout_of(&output), GREEN_FALSE, "{}", describe(&output));
}

#[test]
fn a_green_full_run_on_a_parent_with_another_tree_is_not_reused() {
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.b, &[(711, "success")]);
    gh.full_jobs(711);
    gh.runs(&merge.t, &[]);
    let output = gh.gate(&merge.m);
    assert_eq!(stdout_of(&output), GREEN_FALSE, "{}", describe(&output));
    assert!(
        !gh.calls().contains(&format!("head_sha={}", merge.b)),
        "asked for the runs of {}:\n{}",
        merge.b,
        gh.calls()
    );
}

#[test]
fn a_lookup_error_is_not_reused() {
    // The runs of the same-tree parent cannot be read (no fixture: a 404).
    let gh = Gh::new();
    let merge = release_merge(&gh);
    let output = gh.gate(&merge.m);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(stdout_of(&output), GREEN_FALSE, "{}", describe(&output));

    // A green full run exists, but every call fails as with a bad token.
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.t, &[(721, "success")]);
    gh.full_jobs(721);
    gh.fail_every_call();
    let output = gh.gate(&merge.m);
    assert!(output.status.success(), "{}", describe(&output));
    assert_eq!(stdout_of(&output), GREEN_FALSE, "{}", describe(&output));
}

#[test]
fn a_green_run_without_a_successful_full_suite_job_is_not_reused() {
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.t, &[(731, "success"), (732, "success")]);
    gh.jobs(731, &[("tests (x86_64-unknown-linux-gnu)", "success"), (FULL_JOB, "skipped")]);
    gh.jobs(732, &[("tests (x86_64-unknown-linux-gnu)", "success")]);
    let output = gh.gate(&merge.m);
    assert_eq!(stdout_of(&output), GREEN_FALSE, "{}", describe(&output));
}

#[test]
fn a_cancelled_or_failed_run_is_not_reused_even_with_a_green_full_suite_job() {
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.t, &[(741, "cancelled"), (742, "failure")]);
    gh.full_jobs(741);
    gh.full_jobs(742);
    let output = gh.gate(&merge.m);
    assert_eq!(stdout_of(&output), GREEN_FALSE, "{}", describe(&output));
}

// ---------------------------------------------------------------------------
// ci.yml's `select` step

/// The `run: |` block of ci.yml's `select` step, dedented.
fn select_step() -> String {
    let ci = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let mut lines = ci.lines().skip_while(|l| *l != "  select:").skip_while(|l| *l != "      - id: select");
    assert_eq!(lines.next(), Some("      - id: select"), "ci.yml has no `select` step in a `select` job");
    let mut lines = lines.skip_while(|l| *l != "        run: |");
    assert_eq!(lines.next(), Some("        run: |"), "the `select` step has no `run: |` block");
    let body: Vec<&str> = lines
        .take_while(|l| l.starts_with("          ") || l.trim().is_empty())
        .map(|l| l.strip_prefix("          ").unwrap_or(""))
        .collect();
    assert!(body.len() > 20, "the `select` step's script looks cut short:\n{}", body.join("\n"));
    body.join("\n") + "\n"
}

/// What one run of the `select` step left behind.
struct Selected {
    output: Output,
    /// `$GITHUB_OUTPUT`.
    outputs: String,
    /// `$GITHUB_STEP_SUMMARY`.
    summary: String,
}

impl Selected {
    fn get(&self, key: &str) -> Option<&str> {
        self.outputs.lines().find_map(|l| l.strip_prefix(&format!("{key}=")))
    }
}

/// Runs the `select` step from the repository root as GitHub would for
/// `event` on `git_ref`, with `full_suite` as `inputs.full_suite`.
fn run_select(gh: &Gh, event: &str, git_ref: &str, full_suite: &str, github_sha: &str) -> Selected {
    let out = gh.dir.path().join("github_output");
    let summary = gh.dir.path().join("step_summary");
    fs::write(&out, "").unwrap();
    fs::write(&summary, "").unwrap();
    let output = Command::new("bash")
        .arg("-c")
        .arg(select_step())
        .current_dir(repo_root())
        .env("PATH", gh.path())
        .env_remove("GH")
        .env("EVENT", event)
        .env("REF", git_ref)
        .env("FULL_SUITE", full_suite)
        .env("PR_BASE", "")
        .env("BEFORE", "0000000000000000000000000000000000000000")
        .env("GH_TOKEN", "fake")
        .env("GITHUB_REPOSITORY", SLUG)
        .env("GITHUB_SHA", github_sha)
        .env("GITHUB_SERVER_URL", "https://github.com")
        .env("GITHUB_OUTPUT", &out)
        .env("GITHUB_STEP_SUMMARY", &summary)
        .output()
        .expect("failed to run bash");
    Selected {
        output,
        outputs: fs::read_to_string(&out).unwrap(),
        summary: fs::read_to_string(&summary).unwrap(),
    }
}

/// A release merge on main whose release-side parent passed full run 801.
fn merge_with_green_parent(gh: &Gh) -> Merge {
    let merge = release_merge(gh);
    gh.runs(&merge.t, &[(801, "success")]);
    gh.full_jobs(801);
    merge
}

#[test]
fn a_push_to_main_whose_tree_passed_a_full_run_skips_the_tests_and_says_why() {
    let gh = Gh::new();
    let merge = merge_with_green_parent(&gh);
    let selected = run_select(&gh, "push", "refs/heads/main", "", &merge.m);
    let output = &selected.output;
    assert!(output.status.success(), "{}", describe(output));
    assert_eq!(selected.get("mode"), Some("none"), "{}\n{}", selected.outputs, describe(output));
    assert_eq!(selected.get("sections"), Some(""), "{}", selected.outputs);
    assert_eq!(selected.get("reused"), Some("true"), "{}", selected.outputs);
    let summary = &selected.summary;
    assert!(summary.contains("## Tests skipped: this tree already passed a full run"), "{summary}");
    assert!(summary.contains(&format!("https://github.com/{SLUG}/actions/runs/801")), "{summary}");
    assert!(summary.contains(&format!("`{}`", merge.t)), "{summary}");
    assert!(summary.contains(&format!("`{}`", merge.m)), "{summary}");
    assert!(!summary.contains("Partial run"), "{summary}");
}

fn assert_full_not_reused(selected: &Selected) {
    let output = &selected.output;
    assert!(output.status.success(), "{}", describe(output));
    assert_eq!(selected.get("mode"), Some("full"), "{}\n{}", selected.outputs, describe(output));
    assert_eq!(selected.get("reused"), Some("false"), "{}", selected.outputs);
    assert!(
        selected.get("sections").is_some_and(|s| s.split(' ').any(|s| s == "core-it")),
        "{}",
        selected.outputs
    );
    assert_eq!(selected.summary, "", "a full run wrote a summary");
}

#[test]
fn a_push_to_main_without_a_green_full_run_on_its_tree_stays_full() {
    let gh = Gh::new();
    let merge = release_merge(&gh);
    gh.runs(&merge.t, &[(811, "success")]);
    gh.jobs(811, &[(FULL_JOB, "skipped")]);
    assert_full_not_reused(&run_select(&gh, "push", "refs/heads/main", "", &merge.m));
}

#[test]
fn a_push_to_main_whose_lookup_fails_stays_full() {
    let gh = Gh::new();
    let merge = merge_with_green_parent(&gh);
    gh.fail_every_call();
    assert_full_not_reused(&run_select(&gh, "push", "refs/heads/main", "", &merge.m));
    assert!(!gh.calls().is_empty(), "the step never asked");
}

#[test]
fn a_call_or_a_dispatch_on_main_stays_full_without_asking() {
    for (event, full_suite) in [("workflow_call", "true"), ("push", "true"), ("workflow_dispatch", "")] {
        let gh = Gh::new();
        let merge = merge_with_green_parent(&gh);
        let selected = run_select(&gh, event, "refs/heads/main", full_suite, &merge.m);
        assert_full_not_reused(&selected);
        assert_eq!(gh.calls(), "", "{event} (full_suite={full_suite}) asked GitHub for an earlier run");
    }
}

#[test]
fn a_push_to_another_branch_is_never_reused() {
    let gh = Gh::new();
    let merge = merge_with_green_parent(&gh);
    assert_full_not_reused(&run_select(&gh, "push", "refs/heads/release-9.9.9", "", &merge.m));
    assert_eq!(gh.calls(), "", "a push to a release branch asked GitHub for an earlier run");
}

/// When `select` reuses a run (`mode` none, `reused` true), every other job
/// but `matrix` is skipped: by `reused`, or by a `mode` that `none` fails.
#[test]
fn every_job_after_select_is_skipped_on_a_reused_run() {
    let ci = fs::read_to_string(repo_root().join(".github/workflows/ci.yml")).unwrap();
    let jobs_at = ci.lines().position(|l| l == "jobs:").expect("ci.yml has a jobs: key");
    let lines: Vec<&str> = ci.lines().skip(jobs_at + 1).collect();
    let mut checked = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let is_job = line.len() > 3
            && line.starts_with("  ")
            && !line.starts_with("   ")
            && line.ends_with(':')
            && !line.trim_start().starts_with('#');
        i += 1;
        if !is_job {
            continue;
        }
        let job = line.trim().trim_end_matches(':');
        let body: Vec<&str> = lines[i..]
            .iter()
            .take_while(|l| l.starts_with("    ") || l.trim().is_empty() || l.trim_start().starts_with('#'))
            .copied()
            .collect();
        if job == "matrix" || job == "select" {
            continue;
        }
        let needs = body
            .iter()
            .find_map(|l| l.strip_prefix("    needs: "))
            .unwrap_or_else(|| panic!("job {job} has no needs:"));
        assert!(
            needs.split(|c: char| !c.is_alphanumeric() && c != '-').any(|n| n == "select"),
            "job {job} does not need select: {needs}"
        );
        let condition = body
            .iter()
            .find_map(|l| l.strip_prefix("    if: "))
            .unwrap_or_else(|| panic!("job {job} has no if:"));
        assert!(
            condition.contains("needs.select.outputs.reused != 'true'")
                || condition.contains("needs.select.outputs.mode != 'none'")
                || condition.starts_with("needs.select.outputs.mode == 'full'"),
            "job {job} runs on a reused run: if: {condition}"
        );
        checked.push(job.to_string());
    }
    for job in ["test", "full-suite", "shellcheck", "installer", "go-plugin-without-toolchain", "lint"] {
        assert!(checked.iter().any(|j| j == job), "job {job} not found in ci.yml; checked {checked:?}");
    }
}
