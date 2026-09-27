//! A language server started early, by `SemanticEngine::prepare`, is reaped
//! by `kill_live_servers` - the path a plugin takes when core closes the
//! control stream while the plugin is busy - like one a pass started.
//!
//! Its own test binary, because `kill_live_servers` kills every server this
//! process has started: next to the other bridge tests it would kill theirs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use g_mesh_plugin_sdk::lsp::{kill_live_servers, Budgets, LspBridge, SemanticConfig};
use g_mesh_plugin_sdk::SemanticEngine;
use serde_json::json;

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn asked(log: &Path, method: &str) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter(|line| line.trim() == method).count()
}

/// Control: drop the `LIVE_SERVERS` registration from `LspClient::start` and
/// the killed server is still running, so the second `prepare` finds it
/// alive and starts nothing - `initialize` stays at one.
#[test]
fn a_server_started_by_prepare_is_reaped_by_kill_live_servers() {
    let scratch =
        Scratch(std::env::temp_dir().join(format!("g-mesh-lsp-prepare-reaped-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&scratch.0);
    std::fs::create_dir_all(&scratch.0).unwrap();
    let log = scratch.0.join("server.log");
    let script = scratch.0.join("script.json");
    std::fs::write(
        &script,
        serde_json::to_vec(&json!({
            "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 0 },
            "log": log.to_string_lossy(),
        }))
        .unwrap(),
    )
    .unwrap();
    let mut config = SemanticConfig::new(env!("CARGO_BIN_EXE_g-mesh-fake-lsp"));
    config.args = vec!["--script".to_string(), script.to_string_lossy().into_owned()];
    config.engine = "fake-lsp".to_string();
    let budgets = Budgets { single_file: Duration::from_secs(30), ..Budgets::default() };
    let mut bridge = LspBridge::with_budgets("toy", &scratch.0, config, budgets);

    bridge.prepare();
    assert_eq!(asked(&log, "initialize"), 1, "prepare started a server");

    kill_live_servers();

    // A bridge whose server is gone starts a new one; one whose server
    // survived the kill has nothing to do.
    bridge.prepare();
    assert_eq!(asked(&log, "initialize"), 2, "the prepared server was killed, so a new one had to start");
}
