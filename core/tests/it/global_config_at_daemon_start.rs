//! A daemon resolves its embedding settings at start: a broken global
//! `config.toml` is in `daemon.log` once the daemon answers `initialize`,
//! with no tool call made. Its idle-timer knobs are read there too, so a bad
//! value's line, under the daemon's role prefix, is in the same log.
//!
//! A real daemon, bootstrapped through the shim against a `G_MESH_HOME` of
//! the test's own, so `<home>/config.toml` is the file it reads and its
//! `daemon.log` is the production one under `<home>/projects/<hash>/`. The
//! daemon answers `initialize` only from the accept loop, which starts after
//! the settings are resolved.
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};

use g_mesh::daemon::identity::project_hash;
use g_mesh::paths::state_dir::StateDir;
use g_mesh::paths::HOME_ENV;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use tokio::process::Command;

use crate::common;
use crate::common::{IsolatedHome, Lifeline};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

struct Harness {
    home: IsolatedHome,
    project: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self { home: IsolatedHome::new(), project: tempfile::tempdir().unwrap() };
        fs::write(harness.home.dir.join("config.toml"), "[rerank\nenabled = = true\n").unwrap();
        harness
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn state_dir(&self) -> PathBuf {
        self.home.projects_root().join(project_hash(self.root()).expect("failed to hash the project root"))
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.state_dir().join(g_mesh::shim::DAEMON_LOG_FILE)).unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        common::kill_pid_file(&StateDir::at(self.state_dir()).pid_file());
    }
}

#[tokio::test]
async fn a_broken_global_config_and_a_bad_idle_value_are_logged_at_daemon_start() {
    let harness = Harness::new();
    let (root, home) = (harness.root().to_path_buf(), harness.home.dir.clone());
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.lifeline();
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(&root)
            .env(HOME_ENV, &home)
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env_remove(g_mesh::shim::DAEMON_LOG_ENV)
            .env_remove(g_mesh::knobs::EMBEDDING_CACHE.name)
            .env_remove(g_mesh::knobs::RERANK.name)
            .env(g_mesh::knobs::CORE_IDLE_MS.name, "abc")
            .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("the shim must reach the daemon");

    let log = harness.log_text();
    // The parse error spans several lines, so each failure line is matched
    // by its start and its end.
    let failures = log.matches("g-mesh: failed to read the global config (").count();
    assert_eq!(failures, 2, "one failure line per part that needed the file:\n{log}");
    assert!(
        log.contains(") - using the embedding cache's defaults\n"),
        "no cache line in daemon.log:\n{log}"
    );
    assert!(log.contains(") - the rerank keeps its default (on)\n"), "no rerank line in daemon.log:\n{log}");
    assert!(
        log.lines().any(|line| line.starts_with("g-mesh daemon: ignoring G_MESH_CORE_IDLE_MS=\"abc\" - ")),
        "no daemon-prefixed bad-value line in daemon.log:\n{log}"
    );

    client.cancel().await.expect("failed to shut the client down");
}
