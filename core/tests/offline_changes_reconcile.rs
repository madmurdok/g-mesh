//! Files changed while no daemon ran - a worktree switched to another branch,
//! an edit made with g-mesh stopped - are reconciled with the index when the
//! next daemon starts (`PluginRegistry::reconcile_since_index`,
//! docs/design/GM-524-worktree-daemon-lifecycle.md, D4).
//!
//! The whole stack - shim, daemon, the real TypeScript plugin - over a temp
//! project, started, stopped with `g-mesh stop`, changed on disk, and started
//! again. The daemon's "reconciled N files" log line (one per start) is the
//! completion signal and carries the counts; the index file is then polled for
//! what the graph must show. No tool call touches a changed file after the
//! restart, so nothing but the reconcile can have brought it up to date.
//!
//! The Rust plugin is linked beside the TypeScript one only so that `dist/`
//! (excluded by TypeScript, not by Rust) is walked at all: a `.ts` file there
//! is then kept out by the per-language filter alone.
//!
//! Requires the plugin binaries (`cargo build --workspace`) and the
//! TypeScript plugin's `node_modules` (`npm ci` in plugins/typescript).
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use rusqlite::{Connection, OpenFlags};

mod common;

use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// A hang guard for one start's reconcile and the reindex it triggers, well
/// above a TypeScript reindex of a handful of files on a loaded machine.
const RECONCILE_DEADLINE: Duration = Duration::from_secs(180);

/// The counts of one start's "reconciled N files" line.
#[derive(Debug, PartialEq, Eq)]
struct Reconciled {
    deleted: usize,
    created: usize,
    reindexed: usize,
}

struct Harness {
    project: tempfile::TempDir,
    /// Outside the project, so nothing written here reaches the walk: the
    /// plugin root and one daemon log per start.
    aux: tempfile::TempDir,
    starts: usize,
}

impl Harness {
    fn new() -> Self {
        let harness =
            Self { project: tempfile::tempdir().unwrap(), aux: tempfile::tempdir().unwrap(), starts: 0 };
        let plugins = harness.aux.path().join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        for language in ["typescript", "rust"] {
            let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins").join(language);
            std::os::unix::fs::symlink(checkout.canonicalize().unwrap(), plugins.join(language)).unwrap();
        }
        harness
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn log(&self, start: usize) -> PathBuf {
        self.aux.path().join(format!("daemon-{start}.log"))
    }

    /// Writes `relative`. Before the first start its mtime is set an hour
    /// back: the walk records no baseline for a file modified just before it
    /// (it cannot tell such a file from one still being written), and a real
    /// project's files are older than its walk.
    fn write(&self, relative: &str, contents: &str) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        if self.starts == 0 {
            let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
            fs::File::options().write(true).open(&path).unwrap().set_modified(an_hour_ago).unwrap();
        }
    }

    /// Starts a daemon through a shim, logging to a file of this start's own,
    /// and waits until the index is complete and answering.
    async fn start(&mut self) -> RunningService<RoleClient, ()> {
        self.starts += 1;
        let root = self.root().to_path_buf();
        let log = self.log(self.starts);
        let plugins = self.aux.path().join("plugins");
        let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
            cmd.lifeline();
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                .env(daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, &plugins)
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
        }))
        .expect("failed to spawn the shim");
        let client = ().serve(transport).await.expect("the shim must reach the daemon");
        let root = self.root().to_path_buf();
        tokio::task::spawn_blocking(move || common::wait_until_indexed(&root)).await.unwrap();
        // A start whose predecessor stopped before its semantic pass finished
        // retries that pass before the reconcile runs; every start here waits
        // for it, so the next start owes nothing but the reconcile.
        self.await_index("the semantic pass to complete", Index::semantic_pass_completed).await;
        client
    }

    /// Ends the session, then the daemon and its plugins, the way a user
    /// would; `stop` returns once they are gone.
    async fn stop(&self, client: RunningService<RoleClient, ()>) {
        client.cancel().await.expect("failed to shut the client down");
        let output = Command::new(BIN).arg("stop").current_dir(self.root()).output().unwrap();
        assert!(output.status.success(), "`g-mesh stop` failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    /// This start's "reconciled" line, parsed, waiting up to
    /// [`RECONCILE_DEADLINE`] for it.
    async fn reconciled(&self) -> Reconciled {
        let log = self.log(self.starts);
        let deadline = Instant::now() + RECONCILE_DEADLINE;
        loop {
            let text = fs::read_to_string(&log).unwrap_or_default();
            if let Some(line) = text.lines().find(|line| line.contains("files with the disk in")) {
                return parse_reconciled(line);
            }
            assert!(
                Instant::now() < deadline,
                "no reconcile line within {RECONCILE_DEADLINE:?}; daemon log:\n{text}",
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Polls the index file until `ready` holds for it.
    async fn await_index(&self, what: &str, mut ready: impl FnMut(&Index) -> bool) {
        let deadline = Instant::now() + RECONCILE_DEADLINE;
        loop {
            if let Some(index) = Index::open(self.root()) {
                if ready(&index) {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {RECONCILE_DEADLINE:?} waiting for {what}; daemon log:\n{}",
                fs::read_to_string(self.log(self.starts)).unwrap_or_default()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            for (_, path) in daemon::registry::discovered_pid_files(&state) {
                common::kill_pid_file(&path);
            }
            common::kill_pid_file(&daemon::pid_path_in(&state));
            let _ = fs::remove_dir_all(&state);
        }
    }
}

/// `... reconciled N files with the disk in M ms: D deleted, C created, R reindexed`.
fn parse_reconciled(line: &str) -> Reconciled {
    let counts = line.rsplit_once(": ").map(|(_, counts)| counts).unwrap_or_else(|| panic!("{line}"));
    let count = |label: &str| -> usize {
        counts
            .split(", ")
            .find_map(|part| part.strip_suffix(label))
            .and_then(|number| number.trim().parse().ok())
            .unwrap_or_else(|| panic!("no `{label}` count in: {line}"))
    };
    Reconciled { deleted: count(" deleted"), created: count(" created"), reindexed: count(" reindexed") }
}

/// A read-only view of the daemon's index file.
struct Index(Connection);

impl Index {
    fn open(root: &Path) -> Option<Self> {
        let path = project_dir(root).ok()?.join("index.db");
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok().map(Self)
    }

    fn semantic_pass_completed(&self) -> bool {
        g_mesh::storage::schema::semantic_pass_completed(&self.0).unwrap_or(false)
    }

    /// Paths of every `File` node. A failed read (a swap in flight) reads as
    /// empty, and the poll retries.
    fn files(&self) -> Vec<String> {
        self.strings("SELECT filePath FROM nodes WHERE kind = 'File' ORDER BY 1", [])
    }

    /// Files declaring a function named `name`.
    fn defined_in(&self, name: &str) -> Vec<String> {
        self.strings("SELECT filePath FROM nodes WHERE name = ?1 AND kind = 'Function' ORDER BY 1", [name])
    }

    fn baseline_mtime_millis(&self, file_path: &str) -> Option<i64> {
        self.0
            .query_row("SELECT mtimeMillis FROM indexed_files WHERE filePath = ?1", [file_path], |row| {
                row.get(0)
            })
            .ok()
    }

    fn strings<P: rusqlite::Params>(&self, sql: &str, params: P) -> Vec<String> {
        let Ok(mut statement) = self.0.prepare(sql) else { return Vec::new() };
        statement
            .query_map(params, |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_default()
    }
}

fn function(name: &str) -> String {
    format!("export function {name}(): number {{\n  return 1;\n}}\n")
}

fn mtime_millis(path: &Path) -> i64 {
    let modified = fs::metadata(path).unwrap().modified().unwrap();
    modified.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// A file edited, one deleted, one created and one only touched while no
/// daemon ran: the next start reindexes the edited and created ones, drops
/// the deleted one, refreshes the touched one's baseline without reindexing
/// it, and leaves alone what is excluded (`dist/`, `node_modules/`). The start
/// after that has nothing left to do.
///
/// Controls (one per behaviour; see the slice's control list):
/// - skip `reconcile_since_index` in `spawn_watch_consumer`: no line, the
///   edit is not seen;
/// - drop the deleted loop: `0 deleted`, `goner` stays;
/// - drop the created chain (`announce_created` and `ensure_fresh` of
///   created files): `createdThing` never appears;
/// - drop the `indexing_language` filter on the walk: `2 created`
///   (`dist/excluded.ts` counted);
/// - route known files with `route_settled_path` instead of `ensure_fresh`:
///   the touched file's baseline keeps its old mtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes_made_while_no_daemon_ran_are_reconciled_at_the_next_start() {
    let mut harness = Harness::new();
    harness.write("src/edited.ts", &function("oldName"));
    harness.write("src/deleted.ts", &function("goner"));
    harness.write("src/touched.ts", &function("touched"));
    let client = harness.start().await;
    harness
        .await_index("the first walk's graph", |index| {
            index.defined_in("oldName") == ["src/edited.ts"]
                && index.defined_in("goner") == ["src/deleted.ts"]
        })
        .await;
    harness.stop(client).await;

    harness.write("src/edited.ts", &function("newName"));
    fs::remove_file(harness.root().join("src/deleted.ts")).unwrap();
    harness.write("src/created.ts", &function("createdThing"));
    harness.write("dist/excluded.ts", &function("excludedThing"));
    harness.write("node_modules/pkg/index.ts", &function("nodeThing"));
    let touched = harness.root().join("src/touched.ts");
    let later = SystemTime::now() + Duration::from_secs(5);
    fs::File::options().write(true).open(&touched).unwrap().set_modified(later).unwrap();
    let touched_mtime = mtime_millis(&touched);

    let client = harness.start().await;
    assert_eq!(harness.reconciled().await, Reconciled { deleted: 1, created: 1, reindexed: 2 });
    harness
        .await_index("the offline changes in the graph", |index| {
            index.defined_in("newName") == ["src/edited.ts"]
                && index.defined_in("oldName").is_empty()
                && index.defined_in("goner").is_empty()
                && !index.files().contains(&"src/deleted.ts".to_string())
                && index.defined_in("createdThing") == ["src/created.ts"]
                && index
                    .baseline_mtime_millis("src/touched.ts")
                    .is_some_and(|at| (at - touched_mtime).abs() <= 1)
        })
        .await;
    let index = Index::open(harness.root()).unwrap();
    assert!(index.defined_in("excludedThing").is_empty(), "dist/ is excluded for TypeScript");
    assert!(index.defined_in("nodeThing").is_empty(), "node_modules/ is excluded");
    assert_eq!(index.defined_in("touched"), ["src/touched.ts"], "a touched file keeps its graph");
    drop(index);
    harness.stop(client).await;

    let client = harness.start().await;
    assert_eq!(
        harness.reconciled().await,
        Reconciled { deleted: 0, created: 0, reindexed: 0 },
        "the reconciled start wrote the baselines, so the next one has nothing to do"
    );
    harness.stop(client).await;
}

/// A start with nothing changed since the last one - the first after a
/// cold walk - reindexes nothing: the walk's baselines are what the reconcile
/// compares against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_start_reindexes_nothing() {
    let mut harness = Harness::new();
    harness.write("src/a.ts", &function("alpha"));
    harness.write("src/b.ts", &function("beta"));
    let client = harness.start().await;
    harness.await_index("the first walk's graph", |index| index.defined_in("beta") == ["src/b.ts"]).await;
    harness.stop(client).await;

    let client = harness.start().await;
    assert_eq!(harness.reconciled().await, Reconciled { deleted: 0, created: 0, reindexed: 0 });
    harness.stop(client).await;
}
