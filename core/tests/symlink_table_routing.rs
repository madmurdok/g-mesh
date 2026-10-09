//! GM-514 acceptance (docs/architecture/gm-514-core-symlink-table.md,
//! section 5, B1-B3, B8, B14): a file reached only through a link whose
//! target is gitignored stays fresh in the index while the daemon serves.
//!
//! The whole stack - shim, daemon, watcher, the SDK's toy plugin
//! (`g-mesh-plugin-toy`, `plugins/sdk/toy/main.rs`) - over a temp project.
//! Every assertion is on the index file, never on which spelling an event
//! arrived under (M2: FSEvents reports the real path, inotify either one).
//!
//! Polls are hang guards, not timing claims. Before any edit, a probe file is
//! written and awaited in the index, so the watcher is known to be live.
//!
//! Requires `cargo build --workspace` (the toy plugin binary beside this
//! profile's `g-mesh`).
//!
//! Controls: skip the remap in `ProjectWatcher::next_change` (route the path
//! as reported) - on macOS the edit to `gen/a.toy` never reaches the index
//! (B1 times out); in `daemon::watch_and_route_once`, never pass the changed
//! links to `gitignore_changed` - the mid-session link's file is never
//! indexed (B8 times out).
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use rusqlite::{Connection, OpenFlags};
use tokio::process::Command;

mod common;

use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// How long a change gets to reach the index: a deadlock guard well above a
/// debounce window plus a toy reindex, on a loaded machine.
const CHANGE_DEADLINE: Duration = Duration::from_secs(120);

/// The project, and outside it (so writes there cannot feed the watcher) the
/// plugin root holding only the toy plugin's manifest, and the daemon log.
struct Harness {
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self { project: tempfile::tempdir().unwrap(), aux: tempfile::tempdir().unwrap() };
        let dir = harness.plugins_root().join("toy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("plugin.toml"),
            format!(
                "[plugin]\nlanguage = \"toy\"\nprotocol_version = {}\nplugin_version = \"{}\"\n\n\
                 [plugin.spawn]\ncommand = \"{}/g-mesh-plugin-toy\"\n\n\
                 [plugin.languages]\nextensions = [\".toy\"]\n",
                g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION,
                env!("CARGO_PKG_VERSION"),
                daemon::manifest::BIN_DIR_PLACEHOLDER,
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

    fn log_text(&self) -> String {
        fs::read_to_string(self.log()).unwrap_or_default()
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn remove(&self, relative: &str) {
        fs::remove_file(self.root().join(relative)).unwrap();
    }

    fn link(&self, relative: &str, target: &str) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(target, path).unwrap();
    }

    /// Starts the daemon through a shim, waits for the cold-start walk, then
    /// proves the watcher routes saves by awaiting a probe file's node.
    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let root = self.root().to_path_buf();
        let (log, plugins) = (self.log(), self.plugins_root());
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.lifeline();
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                .env(daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, &plugins)
                // No real model: keeps the embedding backfill pass a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        }))
        .expect("failed to spawn the shim");
        let client = ().serve(transport).await.expect("the shim must reach the daemon");
        let root = self.root().to_path_buf();
        tokio::task::spawn_blocking(move || common::wait_until_indexed(&root)).await.unwrap();
        self.write("probe.toy", "fn probe_start\n");
        self.await_index("the probe to be indexed", |index| index.defined_in("probe_start") == ["probe.toy"])
            .await;
        client
    }

    /// Polls the index file until `ready` holds for it.
    async fn await_index(&self, what: &str, mut ready: impl FnMut(&Index) -> bool) {
        let deadline = Instant::now() + CHANGE_DEADLINE;
        loop {
            if let Some(index) = Index::open(self.root()) {
                if ready(&index) {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {CHANGE_DEADLINE:?} waiting for {what}; files: {:?}; daemon log:\n{}",
                Index::open(self.root()).map(|index| index.files_under("")).unwrap_or_default(),
                self.log_text()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    fn stop_daemon(&self) {
        let mut pid_files: Vec<PathBuf> =
            [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())]
                .into_iter()
                .flatten()
                .collect();
        if let Ok(state) = project_dir(self.root()) {
            pid_files.push(state.join("plugin-toy.pid"));
        }
        for path in pid_files {
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

/// A read-only view of the daemon's index file.
struct Index(Connection);

impl Index {
    fn open(root: &Path) -> Option<Self> {
        let path = project_dir(root).ok()?.join("index.db");
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok().map(Self)
    }

    /// Paths of the `File` nodes under `dir` (`""` for all), sorted. A read
    /// that fails (a swap in flight) reads as empty, and the poll retries.
    fn files_under(&self, dir: &str) -> Vec<String> {
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let Ok(mut statement) = self.0.prepare(
            "SELECT filePath FROM nodes WHERE kind = 'File' AND substr(filePath, 1, ?2) = ?1 ORDER BY 1",
        ) else {
            return Vec::new();
        };
        statement
            .query_map(rusqlite::params![prefix, prefix.len() as i64], |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
    }

    /// Files declaring a function named `name`, sorted.
    fn defined_in(&self, name: &str) -> Vec<String> {
        let Ok(mut statement) =
            self.0.prepare("SELECT filePath FROM nodes WHERE name = ?1 AND kind = 'Function' ORDER BY 1")
        else {
            return Vec::new();
        };
        statement
            .query_map([name], |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
    }

    /// Nothing is indexed under a real (gitignored) spelling.
    fn nothing_under_real_spellings(&self) -> bool {
        self.files_under("gen").is_empty() && self.files_under("cfg").is_empty()
    }
}

/// B1-B3 and B14: `src/api -> ../gen` (directory link) and
/// `src/config.toy -> ../cfg/config.toy` (file link), `gen/` and `cfg/`
/// gitignored. Editing, creating and deleting under the real spellings
/// updates the rows under the links, and nothing is ever indexed under the
/// real spellings. The deletes check M6: the removal is classified on the
/// remapped path.
#[tokio::test(flavor = "multi_thread")]
async fn edits_under_alias_only_link_targets_update_the_alias_rows() {
    let harness = Harness::new();
    harness.write(".gitignore", "gen/\ncfg/\n");
    harness.write("gen/a.toy", "fn alpha\n");
    harness.write("cfg/config.toy", "fn config_one\n");
    harness.write("src/main.toy", "fn main_fn\n");
    harness.link("src/api", "../gen");
    harness.link("src/config.toy", "../cfg/config.toy");
    let _client = harness.connect().await;
    harness
        .await_index("the bulk walk to index the alias spellings", |index| {
            index.defined_in("alpha") == ["src/api/a.toy"]
                && index.defined_in("config_one") == ["src/config.toy"]
                && index.nothing_under_real_spellings()
        })
        .await;

    // B1: edit the real spelling of a directory link's alias-only file.
    harness.write("gen/a.toy", "fn beta\n");
    harness
        .await_index("the edit to reach src/api/a.toy", |index| {
            index.defined_in("beta") == ["src/api/a.toy"]
                && index.defined_in("alpha").is_empty()
                && index.files_under("src/api") == ["src/api/a.toy"]
                && index.nothing_under_real_spellings()
        })
        .await;

    // B3: create a file under the real spelling.
    harness.write("gen/b.toy", "fn gamma\n");
    harness
        .await_index("gen/b.toy to be indexed as src/api/b.toy", |index| {
            index.defined_in("gamma") == ["src/api/b.toy"] && index.nothing_under_real_spellings()
        })
        .await;

    // B2: delete it under the real spelling.
    harness.remove("gen/a.toy");
    harness
        .await_index("src/api/a.toy to be removed", |index| {
            index.files_under("src/api") == ["src/api/b.toy"] && index.defined_in("beta").is_empty()
        })
        .await;

    // B14: the file link's target - edit, delete, re-create.
    harness.write("cfg/config.toy", "fn config_two\n");
    harness
        .await_index("the edit to reach src/config.toy", |index| {
            index.defined_in("config_two") == ["src/config.toy"]
                && index.defined_in("config_one").is_empty()
                && index.nothing_under_real_spellings()
        })
        .await;
    harness.remove("cfg/config.toy");
    harness
        .await_index("src/config.toy to be removed", |index| {
            !index.files_under("src").contains(&"src/config.toy".to_string())
                && index.defined_in("config_two").is_empty()
        })
        .await;
    harness.write("cfg/config.toy", "fn config_three\n");
    harness
        .await_index("the re-created target to be indexed as src/config.toy", |index| {
            index.defined_in("config_three") == ["src/config.toy"] && index.nothing_under_real_spellings()
        })
        .await;

    // Everything else untouched.
    let index = Index::open(harness.root()).unwrap();
    assert_eq!(index.defined_in("main_fn"), ["src/main.toy"]);
    assert_eq!(index.defined_in("gamma"), ["src/api/b.toy"]);
}

/// B8: a link created mid-session indexes its alias-only files (no
/// per-file event arrives for them; the link reaches the gate), and
/// removing it removes them.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_created_or_removed_mid_session_indexes_or_removes_its_files() {
    let harness = Harness::new();
    harness.write(".gitignore", "gen/\n");
    harness.write("gen/a.toy", "fn alpha\n");
    harness.write("src/main.toy", "fn main_fn\n");
    let _client = harness.connect().await;
    assert!(Index::open(harness.root()).unwrap().defined_in("alpha").is_empty());

    harness.link("src/api", "../gen");
    harness
        .await_index("the new link's file to be indexed", |index| {
            index.defined_in("alpha") == ["src/api/a.toy"] && index.files_under("gen").is_empty()
        })
        .await;

    harness.remove("src/api");
    harness
        .await_index("the removed link's file to be removed", |index| {
            index.defined_in("alpha").is_empty() && index.files_under("src/api").is_empty()
        })
        .await;
    assert_eq!(Index::open(harness.root()).unwrap().defined_in("main_fn"), ["src/main.toy"]);
}
