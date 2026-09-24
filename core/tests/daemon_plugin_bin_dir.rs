//! GM-404: the bundled cargo-workspace plugins' `command` follows the
//! running g-mesh executable's own profile directory.
//!
//! `plugins/rust/plugin.toml` and `plugins/python/plugin.toml` used to
//! hard-code `../../target/debug/g-mesh-plugin-*`, so a g-mesh run from
//! `target/release` still spawned the unoptimized debug plugins. They now say
//! `${G_MESH_BIN_DIR}/g-mesh-plugin-*`, which `daemon::manifest` expands to the
//! directory of the running executable.
//!
//! This test runs the real daemon from a `target/release/` directory without
//! paying for a release build: it places a link (or copy) of this build's
//! `g-mesh` binary at `<scratch>/target/release/g-mesh` and points discovery
//! at the real, checked-in `plugins/rust/plugin.toml`. No plugin binary exists
//! in that release directory, so the daemon's first walk must fail naming
//! `<scratch>/target/release/g-mesh-plugin-rust` - the path it resolved - and
//! the `--release` build command. A manifest still spelling
//! `../../target/debug/...` resolves somewhere else entirely, and the
//! assertions below fail.

use std::path::{Path, PathBuf};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

struct Project {
    dir: tempfile::TempDir,
}

impl Drop for Project {
    fn drop(&mut self) {
        let root = self.dir.path();
        if let Ok(path) = daemon::pid_path(root) {
            common::kill_pid_file(&path);
        }
        if let Ok(endpoint) = daemon::endpoint(root) {
            endpoint.clear_stale();
        }
        if let Ok(state) = project_dir(root) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// `<scratch>/target/release/g-mesh(.exe)`, the same executable as [`BIN`].
/// A hard link where possible (`CARGO_TARGET_TMPDIR` is inside the target
/// directory, so usually on the same filesystem), a copy otherwise. Not a
/// symlink: `current_exe` resolves one on Linux, which would put the
/// "running" executable back in `target/debug/`.
fn release_layout_exe(scratch: &Path) -> PathBuf {
    std::fs::write(scratch.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    let release = scratch.join("target").join("release");
    std::fs::create_dir_all(&release).unwrap();
    let exe = release.join(Path::new(BIN).file_name().unwrap());
    if std::fs::hard_link(BIN, &exe).is_err() {
        std::fs::copy(BIN, &exe).expect("failed to copy the g-mesh binary into the release layout");
    }
    exe
}

#[tokio::test]
async fn a_release_daemon_resolves_the_bundled_rust_plugin_in_its_own_release_directory() {
    let scratch = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let exe = release_layout_exe(scratch.path());
    let release_dir = exe.parent().unwrap().to_path_buf();

    // Discovery sees only the real rust manifest, read from its checked-in
    // file, so the walk has exactly one plugin to spawn.
    let plugin_root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(plugin_root.path().join("rust")).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/rust/plugin.toml"),
        plugin_root.path().join("rust").join("plugin.toml"),
    )
    .unwrap();

    let project = Project { dir: tempfile::tempdir().unwrap() };
    let root = project.dir.path().to_path_buf();
    let transport = TokioChildProcess::new(Command::new(&exe).configure(|cmd| {
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&root)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            // Inherited by the daemon the shim bootstraps from `exe`.
            .env("G_MESH_PLUGIN_ROOTS_OVERRIDE", plugin_root.path())
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("the shim must reach the daemon");

    let result = client
        .call_tool(CallToolRequestParams::new("find_definition").with_arguments(
            json!({ "symbol_name": "anything" }).as_object().cloned().expect("an object literal"),
        ))
        .await
        .expect("a failed walk must come back as a tool result");
    client.cancel().await.expect("failed to shut the client down");

    let message = text(&result);
    assert_eq!(result.is_error, Some(true), "no release plugin exists, so the walk must fail: {message}");
    let expected = release_dir.join(format!("g-mesh-plugin-rust{}", std::env::consts::EXE_SUFFIX));
    assert!(
        message.contains(&expected.display().to_string()),
        "the daemon must resolve the plugin in its own release directory {}: {message}",
        release_dir.display()
    );
    assert!(
        message.contains("cargo build --workspace --release"),
        "a missing release plugin must name the release build command: {message}"
    );
}
