//! A `.gitignore` edit re-evaluates what is indexed without a daemon
//! restart (docs/architecture/gm-508-gitignore-reevaluation.md, section 6).
//!
//! The whole stack - shim, daemon, watcher, the real TypeScript plugin - over
//! a temp project. Each test edits, creates or deletes a `.gitignore` while
//! the daemon serves, then polls the index file until the graph reflects the
//! new rules: the un-ignored files' nodes are there and an import of them
//! resolves (the plugin's presence set saw them), or the newly ignored files'
//! nodes are gone and the import is unresolved.
//!
//! Polls are hang guards, not timing claims. Before any edit, a probe file is
//! written and awaited in the index, so the watcher is known to be live.
//!
//! Requires the TypeScript plugin binary (`cargo build --workspace`) and its
//! `node_modules` (`npm ci` in plugins/typescript).
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
/// debounce window plus a TypeScript reindex of a handful of files.
const CHANGE_DEADLINE: Duration = Duration::from_secs(120);

/// The project, and outside it (so writes there cannot feed the watcher) the
/// plugin root linking only the checked-in TypeScript plugin and the daemon
/// log.
struct Harness {
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self { project: tempfile::tempdir().unwrap(), aux: tempfile::tempdir().unwrap() };
        let plugins = harness.plugins_root();
        fs::create_dir_all(&plugins).unwrap();
        let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript");
        std::os::unix::fs::symlink(
            checkout.canonicalize().expect("the checked-in TypeScript plugin directory"),
            plugins.join("typescript"),
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
        self.probe("probe-start.ts").await;
        client
    }

    /// Writes `relative` and waits until its file node is indexed: the
    /// watcher is live and has routed everything it settled before.
    async fn probe(&self, relative: &str) {
        self.write(relative, "export const probe = 1;\n");
        self.await_index(&format!("the probe {relative} to be indexed"), |index| {
            index.files_under("").contains(&relative.to_string())
        })
        .await;
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
                "timed out after {CHANGE_DEADLINE:?} waiting for {what}; daemon log:\n{}",
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
            pid_files.push(state.join("plugin-typescript.pid"));
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

    /// Files declaring a function named `name` (an importer's own `Module`
    /// node can carry the imported name too).
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

    /// Target file paths of `from`'s `IMPORTS` edges with this `resolved`
    /// flag, sorted.
    fn imports_of(&self, from: &str, resolved: bool) -> Vec<String> {
        let Ok(mut statement) = self.0.prepare(
            "SELECT t.filePath FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
             WHERE e.kind = 'IMPORTS' AND f.filePath = ?1 AND e.resolved = ?2 ORDER BY 1",
        ) else {
            return Vec::new();
        };
        statement
            .query_map(rusqlite::params![from, resolved as i64], |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
    }

    /// The un-ignored target is indexed, its symbol defined, and `importer`'s
    /// import of it resolves to it.
    fn target_indexed_and_resolved(&self, importer: &str, target: &str) -> bool {
        self.files_under("").contains(&target.to_string())
            && self.defined_in(TARGET_SYMBOL) == [target.to_string()]
            && self.imports_of(importer, true).contains(&target.to_string())
    }

    /// Nothing under `dir` is indexed, the symbol is gone, and `importer`
    /// still has an import that is now unresolved.
    fn dir_gone_and_import_unresolved(&self, importer: &str, dir: &str) -> bool {
        self.files_under(dir).is_empty()
            && self.defined_in(TARGET_SYMBOL).is_empty()
            && self.files_under("").contains(&importer.to_string())
            && self.imports_of(importer, true).iter().all(|path| !path.starts_with(&format!("{dir}/")))
            && !self.imports_of(importer, false).is_empty()
    }
}

const TARGET_SYMBOL: &str = "generatedThing";
const TARGET_SOURCE: &str = "export function generatedThing(): number {\n  return 1;\n}\n";

fn importer_of(specifier: &str) -> String {
    format!("import {{ {TARGET_SYMBOL} }} from \"{specifier}\";\n\nexport const value = {TARGET_SYMBOL}();\n")
}

/// Sections 6.2-6.4: un-ignoring `generated/` in the root `.gitignore`
/// indexes `generated/x.ts` and resolves `src/use.ts`'s import of it;
/// ignoring it again removes its nodes and leaves the import unresolved.
#[tokio::test(flavor = "multi_thread")]
async fn editing_the_root_gitignore_indexes_and_removes_a_directory() {
    let harness = Harness::new();
    harness.write(".gitignore", "generated/\n");
    harness.write("generated/x.ts", TARGET_SOURCE);
    harness.write("src/use.ts", &importer_of("../generated/x"));
    let client = harness.connect().await;

    let index = Index::open(harness.root()).expect("the index exists after the walk");
    assert!(index.files_under("generated").is_empty(), "generated/ starts ignored");
    assert!(index.files_under("").contains(&"src/use.ts".to_string()));
    drop(index);

    harness.write(".gitignore", "");
    harness
        .await_index("generated/x.ts indexed and src/use.ts's import of it resolved", |index| {
            index.target_indexed_and_resolved("src/use.ts", "generated/x.ts")
        })
        .await;

    harness.write(".gitignore", "generated/\n");
    harness
        .await_index("generated/ removed and src/use.ts's import unresolved", |index| {
            index.dir_gone_and_import_unresolved("src/use.ts", "generated")
        })
        .await;

    client.cancel().await.expect("failed to shut the client down");
}

/// Section 6.5: the same through a nested `src/.gitignore`, edited.
#[tokio::test(flavor = "multi_thread")]
async fn editing_a_nested_gitignore_indexes_and_removes_a_directory() {
    let harness = Harness::new();
    harness.write("src/.gitignore", "gen/\n");
    harness.write("src/gen/x.ts", TARGET_SOURCE);
    harness.write("src/use.ts", &importer_of("./gen/x"));
    let client = harness.connect().await;

    let index = Index::open(harness.root()).expect("the index exists after the walk");
    assert!(index.files_under("src/gen").is_empty(), "src/gen/ starts ignored by src/.gitignore");
    drop(index);

    harness.write("src/.gitignore", "");
    harness
        .await_index("src/gen/x.ts indexed and src/use.ts's import of it resolved", |index| {
            index.target_indexed_and_resolved("src/use.ts", "src/gen/x.ts")
        })
        .await;

    harness.write("src/.gitignore", "gen/\n");
    harness
        .await_index("src/gen/ removed and src/use.ts's import unresolved", |index| {
            index.dir_gone_and_import_unresolved("src/use.ts", "src/gen")
        })
        .await;

    client.cancel().await.expect("failed to shut the client down");
}

/// Section 6.5: a `.gitignore` created (ignoring an indexed directory), then
/// deleted (un-ignoring it) - the trigger is the name, not an edit.
#[tokio::test(flavor = "multi_thread")]
async fn creating_and_deleting_a_gitignore_removes_and_indexes_a_directory() {
    let harness = Harness::new();
    harness.write("src/gen/x.ts", TARGET_SOURCE);
    harness.write("src/use.ts", &importer_of("./gen/x"));
    let client = harness.connect().await;

    let index = Index::open(harness.root()).expect("the index exists after the walk");
    assert!(index.target_indexed_and_resolved("src/use.ts", "src/gen/x.ts"), "src/gen/ starts indexed");
    drop(index);

    harness.write("src/.gitignore", "gen/\n");
    harness
        .await_index("src/gen/ removed after src/.gitignore was created", |index| {
            index.dir_gone_and_import_unresolved("src/use.ts", "src/gen")
        })
        .await;

    harness.remove("src/.gitignore");
    harness
        .await_index("src/gen/x.ts indexed again after src/.gitignore was deleted", |index| {
            index.target_indexed_and_resolved("src/use.ts", "src/gen/x.ts")
        })
        .await;

    client.cancel().await.expect("failed to shut the client down");
}

/// Section 6.8 / Q2: un-ignoring more TypeScript files than the guard
/// (10,000) reindexes nothing and logs one line pointing at `g-mesh reindex`.
#[tokio::test(flavor = "multi_thread")]
async fn un_ignoring_more_files_than_the_guard_logs_once_and_reindexes_nothing() {
    const OVER_THE_GUARD: usize = 10_001;
    let harness = Harness::new();
    harness.write(".gitignore", "generated/\n");
    let generated = harness.root().join("generated");
    fs::create_dir_all(&generated).unwrap();
    for index in 0..OVER_THE_GUARD {
        fs::File::create(generated.join(format!("f{index}.ts"))).unwrap();
    }
    harness.write("src/a.ts", "export const a = 1;\n");
    let client = harness.connect().await;

    let guard_lines = |log: &str| {
        log.lines().filter(|line| line.contains("g-mesh reindex") && line.contains(".gitignore")).count()
    };
    harness.write(".gitignore", "");
    let deadline = Instant::now() + CHANGE_DEADLINE;
    while guard_lines(&harness.log_text()) == 0 {
        assert!(Instant::now() < deadline, "no guard line was logged; daemon log:\n{}", harness.log_text());
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // A later save is routed after the gate's batch, so once it is indexed the
    // gate has long finished.
    harness.probe("probe-after.ts").await;

    let log = harness.log_text();
    assert_eq!(guard_lines(&log), 1, "exactly one guard line: {log}");
    let line =
        log.lines().find(|line| line.contains("g-mesh reindex") && line.contains(".gitignore")).unwrap();
    assert!(line.contains(&OVER_THE_GUARD.to_string()), "the line names the count: {line}");
    let index = Index::open(harness.root()).unwrap();
    assert!(index.files_under("generated").is_empty(), "nothing under generated/ was indexed");

    client.cancel().await.expect("failed to shut the client down");
}
