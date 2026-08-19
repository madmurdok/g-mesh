//! The guard for task 217: a `cargo test` run must not write into the
//! developer's own `~/.g-mesh`.
//!
//! Isolation is configured, not coded - `<repo>/.cargo/config.toml` sets
//! `G_MESH_HOME` for everything cargo launches - and configuration is exactly
//! the kind of thing that gets deleted, moved, or quietly not picked up
//! (cargo discovers it by walking up from the *current directory*, so running
//! the suite from somewhere unexpected is enough). Without this file that
//! would fail silently, in the only way that matters: 1422 project
//! directories accumulating under a real home, and the occasional test
//! failing against state something else on the machine was mutating.
//!
//! So this asserts the isolation itself rather than any product behaviour,
//! and it does so on both sides of the process boundary, because the two can
//! break independently.

use std::path::{Path, PathBuf};
use std::process::Command;

use g_mesh::storage::connection::projects_root;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// `~/.g-mesh`, spelled the way the un-overridden code would spell it.
fn real_home() -> PathBuf {
    dirs::home_dir().expect("a home directory").join(".g-mesh")
}

/// The half a test binary can see for itself: every unit test and every
/// in-process helper here resolves state through this.
#[test]
fn the_test_run_resolves_project_state_outside_the_real_g_mesh_home() {
    let root = projects_root().expect("failed to resolve the projects root");

    assert!(
        !root.starts_with(real_home()),
        "tests are pointed at the real home ({}); is <repo>/.cargo/config.toml still there, \
         and was cargo run from inside the repository?",
        root.display()
    );
}

/// The other half, and the one that actually matters: nearly every
/// integration test drives `g-mesh` as a subprocess, and it is the
/// subprocess - not this binary - that writes the state directory. It sees
/// the override only by inheriting the environment, which nothing else here
/// checks.
#[test]
fn a_spawned_g_mesh_resolves_the_same_isolated_state_root() {
    let project = tempfile::tempdir().expect("failed to create a temp project");

    // `status` reports the state directory it would use without creating it
    // or starting a daemon - the cheapest question that has this answer.
    let output = Command::new(BIN)
        .arg("status")
        .current_dir(project.path())
        .output()
        .expect("failed to run g-mesh status");
    assert!(
        output.status.success(),
        "g-mesh status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = state_directory_line(&stdout)
        .unwrap_or_else(|| panic!("g-mesh status did not report a state directory:\n{stdout}"));

    assert!(
        !reported.starts_with(real_home()),
        "a spawned g-mesh writes into the real home ({}); the override did not cross the \
         process boundary",
        reported.display()
    );
    assert!(
        reported.starts_with(projects_root().expect("failed to resolve the projects root")),
        "a spawned g-mesh disagrees with this test binary about where state lives: {} vs {}",
        reported.display(),
        projects_root().unwrap().display()
    );
}

/// The path `g-mesh status` prints as its state directory, or `None` if the
/// report did not contain one.
fn state_directory_line(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("state directory:"))
        .map(|path| PathBuf::from(path.trim()))
}

/// A sanity check on the parser above, so a rewording of `status` output
/// surfaces as this failing rather than as the isolation test quietly
/// asserting nothing.
#[test]
fn the_state_directory_parser_reads_the_line_status_actually_prints() {
    let stdout = "project: a1b2c3d4\n  state directory: /tmp/somewhere/a1b2c3d4\n";

    let parsed = state_directory_line(stdout);

    assert_eq!(parsed, Some(Path::new("/tmp/somewhere/a1b2c3d4").to_path_buf()));
    assert_eq!(state_directory_line("project: a1b2c3d4\n"), None);
}
