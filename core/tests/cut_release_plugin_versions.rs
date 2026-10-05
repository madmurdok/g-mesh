//! `scripts/cut-release.sh` refuses to tag a release while a crate-backed
//! plugin's `plugin.toml` names a `plugin_version` other than the release's.
//!
//! The script is sourced (its `main` is guarded for exactly that) and only
//! `check_crate_backed_plugin_versions` is called, against a scratch
//! `REPO_ROOT` holding copies of the bundled manifests. Bash only, so Unix
//! only: the release is cut from a Unix shell.
#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Every manifest the script checks under the crate-backed rule. A copy of
/// each must exist in the scratch root, or the script dies on the missing one
/// before it compares anything.
const CRATE_BACKED: [&str; 3] = ["rust", "python", "typescript"];

const RELEASE: &str = env!("CARGO_PKG_VERSION");

fn repo_root() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}

/// A scratch root with each crate-backed plugin's manifest copied in, then
/// `typescript`'s `plugin_version` set to `typescript_version`.
fn scratch_root(typescript_version: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("failed to create a scratch repo root");
    for language in CRATE_BACKED {
        let relative = format!("plugins/{language}/plugin.toml");
        let mut text = fs::read_to_string(repo_root().join(&relative))
            .unwrap_or_else(|err| panic!("failed to read {relative}: {err}"));
        // Every copy agrees with the release, whatever the checkout says, so
        // only the TypeScript manifest can be the one that drifts.
        text = set_plugin_version(&text, RELEASE);
        if language == "typescript" {
            text = set_plugin_version(&text, typescript_version);
        }
        let target = root.path().join(&relative);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, text).unwrap();
    }
    root
}

fn set_plugin_version(manifest: &str, version: &str) -> String {
    let mut replaced = 0;
    let lines: Vec<String> = manifest
        .lines()
        .map(|line| {
            if line.starts_with("plugin_version") {
                replaced += 1;
                format!("plugin_version = \"{version}\"")
            } else {
                line.to_string()
            }
        })
        .collect();
    assert_eq!(replaced, 1, "expected exactly one plugin_version line:\n{manifest}");
    lines.join("\n") + "\n"
}

fn check_crate_backed_plugin_versions(root: &Path) -> Output {
    Command::new("bash")
        .arg("-c")
        .arg(r#"source "$1" && REPO_ROOT="$2" && check_crate_backed_plugin_versions "$3""#)
        .arg("cut-release-test")
        .arg(repo_root().join("scripts/cut-release.sh"))
        .arg(root)
        .arg(RELEASE)
        .output()
        .expect("failed to run bash")
}

#[test]
fn a_typescript_plugin_toml_that_drifts_from_the_release_stops_the_cut() {
    let root = scratch_root("0.0.0-drifted");
    let output = check_crate_backed_plugin_versions(root.path());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "the check must refuse; stderr: {stderr}");
    assert!(
        stderr.contains("plugins/typescript/plugin.toml says 0.0.0-drifted"),
        "the refusal must name the drifted manifest: {stderr}"
    );
}

/// The other half of the test above: the same scratch root, agreeing, passes,
/// so the refusal is about the TypeScript version and not about the setup.
#[test]
fn every_crate_backed_plugin_toml_agreeing_with_the_release_lets_the_cut_proceed() {
    let root = scratch_root(RELEASE);
    let output = check_crate_backed_plugin_versions(root.path());
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("plugins/typescript/plugin.toml"),
        "the agreement line lists every manifest it checked"
    );
}
