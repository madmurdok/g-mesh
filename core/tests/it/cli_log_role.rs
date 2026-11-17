//! Shared code run by a CLI process logs under the CLI's prefix: `g-mesh
//! init` runs the same bulk walk a daemon does, and its stderr carries the
//! walk's lines as `g-mesh: ` and no `g-mesh daemon: ` line.
//!
//! The walk's hold line is the probe: `G_MESH_BULK_INDEX_DELAY_MS` makes
//! every finished walk print it. The project is one TypeScript file, so the
//! walk starts the bundled TypeScript plugin.

use std::process::Command;

use g_mesh::paths::HOME_ENV;

use crate::common::{IsolatedHome, Lifeline};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

#[test]
fn a_walk_run_by_init_logs_with_the_cli_prefix() {
    let home = IsolatedHome::new();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("index.ts"), "export const a = 1;\n").unwrap();

    let output = Command::new(BIN)
        .lifeline()
        .arg("init")
        .current_dir(project.path())
        .env(HOME_ENV, &home.dir)
        .env(g_mesh::knobs::BULK_INDEX_DELAY_MS.name, "1")
        .env_remove(g_mesh::knobs::BULK_INDEX_HOLD_FILE.name)
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .output()
        .expect("failed to run `g-mesh init`");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "`g-mesh init` failed with {}:\n{stderr}", output.status);

    assert!(
        stderr.lines().any(|line| line.starts_with("g-mesh: holding the finished bulk walk open for 1ms")),
        "no CLI-prefixed hold line:\n{stderr}"
    );
    assert!(!stderr.contains("g-mesh daemon: "), "a CLI process printed a daemon-prefixed line:\n{stderr}");
}
