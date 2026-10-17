//! The daemon log is one `O_APPEND` file shared by the daemon and every plugin
//! process it spawns (they inherit its stderr). This test fills it from both
//! sides at once - the daemon's `prepare:` call-trace lines and a plugin
//! logging as fast as it can - and asserts that every line in it is a whole
//! line from exactly one writer. Which writer's line comes first is not
//! asserted: nothing orders them.
//!
//! The daemon is bootstrapped through a real shim with `G_MESH_DAEMON_LOG`
//! (so its stderr is opened the way production opens it) and call tracing on.
//! The plugin is `g-mesh-fake-plugin` in its stub persona
//! (`plugins/sdk/fake/main.rs`), which logs `spam` lines while the file
//! `$G_MESH_FAKE_PLUGIN_STDERR_SPAM` exists; it exists only after
//! `cargo build --workspace`. The design is
//! `docs/architecture/gm-520-line-atomic-logs.md` (stress test T2).
#![cfg(unix)]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use g_mesh::daemon;
use g_mesh::mcp::TRACE_CALLS_ENV;
use g_mesh::storage::connection::project_dir;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::json;
use tokio::process::Command;

mod common;

use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const FAKE_PLUGIN_BIN: &str = "g-mesh-fake-plugin";
/// The fake plugin logs `spam` lines while the file this names exists.
const STDERR_SPAM_ENV: &str = "G_MESH_FAKE_PLUGIN_STDERR_SPAM";
/// Tool calls in flight at once, each logging several `prepare:` lines.
const CONCURRENT_CALLS: usize = 200;

const DAEMON_PREFIX: &str = "g-mesh daemon: ";
const PLUGIN_PREFIX: &str = "g-mesh-fake-plugin: ";

/// The project and, outside it so writes there cannot feed the watcher, the
/// stub's manifest, the daemon log and the spam gate.
struct Harness {
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self { project: tempfile::tempdir().unwrap(), aux: tempfile::tempdir().unwrap() };
        let stub = Path::new(BIN)
            .parent()
            .expect("the daemon binary has a directory")
            .join(format!("{FAKE_PLUGIN_BIN}{}", std::env::consts::EXE_SUFFIX));
        assert!(stub.is_file(), "{} does not exist - run `cargo build --workspace` first", stub.display());
        fs::write(harness.root().join("seed.ts"), "export const seed = 1;\n").unwrap();

        let language_dir = harness.plugins_root().join("typescript");
        fs::create_dir_all(&language_dir).unwrap();
        fs::write(
            language_dir.join("plugin.toml"),
            format!(
                "[plugin]\nlanguage = \"typescript\"\nprotocol_version = 2\nplugin_version = \"0.1.0\"\n\n\
                 [plugin.spawn]\ncommand = \"${{G_MESH_BIN_DIR}}/{FAKE_PLUGIN_BIN}\"\n\
                 args = [\"--language\", \"typescript\", \"--plugin-version\", \"0.1.0\"]\n\n\
                 [plugin.languages]\nextensions = [\".ts\"]\n\n\
                 [plugin.capabilities]\nsemantic_pass = true\n",
            ),
        )
        .unwrap();
        harness
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn plugins_root(&self) -> PathBuf {
        self.aux.path().join("plugins")
    }

    fn log(&self) -> PathBuf {
        self.aux.path().join("daemon.log")
    }

    fn spam_gate(&self) -> PathBuf {
        self.aux.path().join("stderr-spam")
    }

    fn log_text(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let root = self.root().to_path_buf();
        let (log, plugins, gate) = (self.log(), self.plugins_root(), self.spam_gate());
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.lifeline();
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                .env(daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, &plugins)
                .env(STDERR_SPAM_ENV, &gate)
                .env(TRACE_CALLS_ENV, "1")
                // No real model: keeps the embedding backfill pass a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        }))
        .expect("failed to spawn the shim");
        let client = ().serve(transport).await.expect("the shim must reach the daemon");
        let root = self.root().to_path_buf();
        tokio::task::spawn_blocking(move || common::wait_until_phase(&root, "ready")).await.unwrap();
        client
    }

    /// Stops the daemon and its plugin, so nothing writes to the log after.
    fn stop_daemon(&self) {
        for path in
            [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())].into_iter().flatten()
        {
            common::kill_pid_file(&path);
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop_daemon();
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = fs::remove_dir_all(&state);
        }
    }
}

fn outline_request() -> CallToolRequestParams {
    CallToolRequestParams::new("get_file_outline")
        .with_arguments(json!({ "file_path": "seed.ts" }).as_object().cloned().unwrap())
}

fn spam_line(n: u64) -> String {
    format!("{PLUGIN_PREFIX}spam seq={n} a={} b={} c={} end={n}", n * 3, n * 7, n % 13)
}

/// Whether `rest` (a `prepare:` line after its verb) is exactly `keys`, in
/// order, each with a well-formed value.
fn fields_match(rest: &str, keys: &[&str]) -> bool {
    let fields: Vec<_> = rest.split(' ').map(|field| field.split_once('=')).collect();
    fields.len() == keys.len()
        && fields.iter().zip(keys).all(|(field, key)| match field {
            Some((k, v)) if k == key => match *key {
                "tool" => *v == "get_file_outline",
                "progressToken" => *v == "present" || *v == "absent",
                "outcome" => ["satisfied", "failed", "timed_out"].contains(v),
                _ => !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()),
            },
            _ => false,
        })
}

/// Whether `line` is one whole `prepare:` trace line (`mcp::trace_call`'s
/// shapes in `GMeshMcpServer::prepare` and `wait_for_index`).
fn is_prepare_line(line: &str) -> bool {
    const SHAPES: [(&str, &[&str]); 5] = [
        ("entered ", &["tool", "request", "progressToken"]),
        ("past the indexing wait ", &["tool", "request"]),
        ("done ", &["tool", "request"]),
        ("wait over ", &["tool", "request", "outcome", "waited_ms", "progress_sent"]),
        ("cancelled ", &["tool", "request", "waited_ms", "progress_sent"]),
    ];
    let Some(rest) = line.strip_prefix(DAEMON_PREFIX).and_then(|rest| rest.strip_prefix("prepare: ")) else {
        return false;
    };
    SHAPES.iter().any(|(verb, keys)| rest.strip_prefix(verb).is_some_and(|fields| fields_match(fields, keys)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_trace_lines_and_plugin_lines_never_split_each_other() {
    let harness = Harness::new();
    let client = harness.connect().await;
    // One call first, so the interactive plugin is up and spamming before the
    // concurrent ones start.
    client.peer().call_tool(outline_request()).await.expect("the warm-up call failed");

    fs::write(harness.spam_gate(), "").unwrap();
    let log_path = harness.log();
    tokio::task::spawn_blocking(move || {
        common::wait_for("the plugin to start logging spam lines", common::startup_timeout(), || {
            fs::read_to_string(&log_path).unwrap_or_default().contains("spam seq=")
        })
    })
    .await
    .unwrap();

    let calls: Vec<_> = (0..CONCURRENT_CALLS)
        .map(|_| {
            let peer = client.peer().clone();
            tokio::spawn(async move { peer.call_tool(outline_request()).await })
        })
        .collect();
    for call in calls {
        call.await.unwrap().expect("a concurrent call failed");
    }

    fs::remove_file(harness.spam_gate()).unwrap();
    drop(client);
    harness.stop_daemon();

    let text = harness.log_text();
    assert!(text.ends_with('\n'), "the log does not end with a whole line");
    let mut malformed = Vec::new();
    let mut spam_seqs = HashSet::new();
    let (mut entered, mut done) = (0, 0);
    for line in text.lines() {
        let whole = if line.contains(PLUGIN_PREFIX.trim_end()) {
            // A plugin line: whole only if it starts the line, holds no daemon
            // text, and (a spam line) is exactly the line its seq names.
            line.starts_with(PLUGIN_PREFIX)
                && !line.contains(DAEMON_PREFIX.trim_end())
                && match line.strip_prefix(PLUGIN_PREFIX).and_then(|rest| rest.strip_prefix("spam seq=")) {
                    Some(rest) => {
                        let seq = rest.split(' ').next().and_then(|n| n.parse::<u64>().ok());
                        seq.is_some_and(|n| line == spam_line(n) && spam_seqs.insert(n))
                    }
                    None => !line.contains("spam"),
                }
        } else if line.contains(DAEMON_PREFIX.trim_end()) || line.contains("prepare: ") {
            line.starts_with(DAEMON_PREFIX) && (!line.contains("prepare: ") || is_prepare_line(line))
        } else {
            // Neither writer's prefix: a tail cut off its line, unless it is
            // free of every field these lines carry.
            !["seq=", "tool=", "request=", "end=", "spam"].iter().any(|field| line.contains(field))
        };
        if !whole {
            malformed.push(line.chars().take(300).collect::<String>());
        } else if line.starts_with(&format!("{DAEMON_PREFIX}prepare: entered ")) {
            entered += 1;
        } else if line.starts_with(&format!("{DAEMON_PREFIX}prepare: done ")) {
            done += 1;
        }
    }
    assert!(
        malformed.is_empty(),
        "{} line(s) not whole, i.e. split by another writer; first few:\n{}",
        malformed.len(),
        malformed.iter().take(5).cloned().collect::<Vec<_>>().join("\n")
    );
    assert!(spam_seqs.len() > 1_000, "too few plugin lines to have overlapped anything: {}", spam_seqs.len());
    let max = spam_seqs.iter().max().copied().unwrap_or(0);
    assert_eq!(spam_seqs.len() as u64, max + 1, "a plugin spam line is missing");
    // At least: `common::wait_until_phase`'s activation trigger is a call too.
    assert!(entered > CONCURRENT_CALLS, "a whole `prepare: entered` line per call, got {entered}");
    assert!(done > CONCURRENT_CALLS, "a whole `prepare: done` line per call, got {done}");
}
