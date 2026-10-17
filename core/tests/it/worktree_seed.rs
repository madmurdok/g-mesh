//! A linked git worktree's first daemon seeds its index from the main
//! checkout's and reconciles it with the worktree's files before any tool
//! call answers (`daemon::seed`, docs/design/GM-564-seed-worktree-index.md).
//!
//! The whole stack - shim, daemon, the real TypeScript plugin - over a temp
//! git repository: the main checkout is indexed by its own daemon and
//! stopped, `git worktree add` makes the worktree, the worktree is changed,
//! and the worktree's daemon starts. Lookups are by symbol name only, so no
//! query-time freshness check (file-anchored calls only) can stand in for
//! the reconcile.
//!
//! Requires the plugin binaries (`cargo build --workspace`) and the
//! TypeScript plugin's `node_modules` (`npm ci` in plugins/typescript).
#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::common;

use crate::common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// A hang guard for one start's reconcile, its semantic pass and a
/// TypeScript reindex of a handful of files on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(180);

/// The test-only hold of `daemon::seed::hold_the_seeded_reconcile_for_tests`.
const HOLD_ENV: &str = "G_MESH_SEED_HOLD_RECONCILE_FILE";

fn function(name: &str, value: u32) -> String {
    format!("export function {name}(): number {{\n  return {value};\n}}\n")
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=g-mesh test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("failed to run git");
    assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
}

/// The counts of one start's "reconciled N files" line.
#[derive(Debug, PartialEq, Eq)]
struct Reconciled {
    seeded: bool,
    deleted: usize,
    created: usize,
    reindexed: usize,
}

fn parse_reconciled(line: &str) -> Reconciled {
    let counts = line.rsplit_once(": ").map(|(_, counts)| counts).unwrap_or_else(|| panic!("{line}"));
    let count = |label: &str| -> usize {
        counts
            .split(", ")
            .find_map(|part| part.strip_suffix(label))
            .and_then(|number| number.trim().parse().ok())
            .unwrap_or_else(|| panic!("no `{label}` count in: {line}"))
    };
    Reconciled {
        seeded: line.contains("of the seeded index"),
        deleted: count(" deleted"),
        created: count(" created"),
        reindexed: count(" reindexed"),
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    main: PathBuf,
    worktree: PathBuf,
    /// Outside both checkouts: the plugin root and one daemon log per start.
    aux: tempfile::TempDir,
    starts: usize,
}

impl Harness {
    /// The main checkout, committed (`src/kept.ts`, `src/changed.ts`,
    /// `src/deleted.ts`, `generated/gen.ts`, `package.json`,
    /// `tsconfig.json`, `.gitignore`) plus one untracked file
    /// (`src/untracked.ts`), every file an hour old as a real project's are.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let aux = tempfile::tempdir().unwrap();
        let plugins = aux.path().join("plugins");
        fs::create_dir_all(&plugins).unwrap();
        let checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript");
        std::os::unix::fs::symlink(checkout.canonicalize().unwrap(), plugins.join("typescript")).unwrap();
        let harness = Self { main: base.join("main"), worktree: base.join("wt"), _dir: dir, aux, starts: 0 };

        let main = &harness.main;
        let files = [
            ("package.json", "{ \"name\": \"seed-fixture\", \"private\": true }\n".to_string()),
            ("tsconfig.json", "{ \"compilerOptions\": { \"strict\": true } }\n".to_string()),
            (".gitignore", "node_modules/\n".to_string()),
            ("src/kept.ts", function("keptThing", 1)),
            ("src/changed.ts", function("changedThing", 1)),
            ("src/deleted.ts", function("goner", 1)),
            ("generated/gen.ts", function("genThing", 1)),
        ];
        for (relative, contents) in &files {
            write(main, relative, contents);
        }
        git(main, &["init", "-q"]);
        git(main, &["add", "."]);
        git(main, &["commit", "-q", "-m", "init"]);
        write(main, "src/untracked.ts", &function("untrackedThing", 1));
        for (relative, _) in &files {
            common::backdate_an_hour(&main.join(relative));
        }
        common::backdate_an_hour(&main.join("src/untracked.ts"));
        harness
    }

    /// Indexes the main checkout with its own daemon, returns its answer for
    /// `keptThing`, and stops it.
    async fn index_main(&mut self) -> Value {
        let main = self.main.clone();
        let client = self.connect(&main, &[]).await;
        let root = main.clone();
        tokio::task::spawn_blocking(move || common::wait_until_indexed(&root)).await.unwrap();
        self.await_index(&main, "the main checkout's semantic pass", |index| index.semantic_pass_completed())
            .await;
        let kept = definition(&client, "keptThing").await;
        self.stop(&main, client).await;
        kept
    }

    /// `git worktree add`, then the worktree's own changes: `changedThing`
    /// edited and moved down, `src/deleted.ts` deleted, `src/created.ts`
    /// created, and `generated/` gitignored (still on disk).
    fn add_worktree(&self) {
        git(&self.main, &["worktree", "add", "-q", "-b", "wt-branch", self.worktree.to_str().unwrap()]);
        let wt = &self.worktree;
        write(wt, "src/changed.ts", &format!("// moved\n// down\n\n{}", function("changedThing", 2)));
        fs::remove_file(wt.join("src/deleted.ts")).unwrap();
        write(wt, "src/created.ts", &function("createdThing", 3));
        write(wt, ".gitignore", "node_modules/\ngenerated/\n");
        assert!(wt.join("generated/gen.ts").is_file());
    }

    fn log(&self, start: usize) -> PathBuf {
        self.aux.path().join(format!("daemon-{start}.log"))
    }

    /// The latest start's daemon log so far.
    fn log_text(&self) -> String {
        fs::read_to_string(self.log(self.starts)).unwrap_or_default()
    }

    /// Starts a daemon for `root` through a shim, logging to this start's own
    /// file, and returns once the session is up.
    async fn connect(&mut self, root: &Path, env: &[(&str, &Path)]) -> RunningService<RoleClient, ()> {
        self.starts += 1;
        let log = self.log(self.starts);
        let plugins = self.aux.path().join("plugins");
        let root = root.to_path_buf();
        let env: Vec<(String, PathBuf)> = env.iter().map(|(k, v)| (k.to_string(), v.to_path_buf())).collect();
        let transport = TokioChildProcess::new(tokio::process::Command::new(BIN).configure(|cmd| {
            cmd.lifeline();
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                .env_remove("G_MESH_SEED")
                .env(g_mesh::shim::DAEMON_LOG_ENV, &log)
                .env(daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, &plugins)
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
            for (key, value) in &env {
                cmd.env(key, value);
            }
        }))
        .expect("failed to spawn the shim");
        ().serve(transport).await.expect("the shim must reach the daemon")
    }

    async fn stop(&self, root: &Path, client: RunningService<RoleClient, ()>) {
        client.cancel().await.expect("failed to shut the client down");
        let output = Command::new(BIN).arg("stop").current_dir(root).output().unwrap();
        assert!(output.status.success(), "`g-mesh stop` failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    /// Waits for the latest start's log to hold a line containing `needle`.
    async fn await_log_line(&self, needle: &str) -> String {
        let deadline = Instant::now() + DEADLINE;
        loop {
            let text = self.log_text();
            if let Some(line) = text.lines().find(|line| line.contains(needle)) {
                return line.to_string();
            }
            assert!(Instant::now() < deadline, "no `{needle}` line within {DEADLINE:?}; daemon log:\n{text}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn reconciled(&self) -> Reconciled {
        parse_reconciled(&self.await_log_line("with the disk in").await)
    }

    /// Polls `root`'s index file until `ready` holds for it.
    async fn await_index(&self, root: &Path, what: &str, mut ready: impl FnMut(&Index) -> bool) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(index) = Index::open(root) {
                if ready(&index) {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out after {DEADLINE:?} waiting for {what}; daemon log:\n{}",
                self.log_text()
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Waits until the worktree's seeded start has stamped its reconcile and
    /// its scoped semantic pass has cleared every semantic-pending file.
    async fn await_seed_settled(&self) {
        self.await_index(&self.worktree, "the seed's reconcile stamp and its semantic pass", |index| {
            index.seed_reconciled() && index.semantic_pending_files().is_empty()
        })
        .await;
    }

    /// Writes into the worktree's index with no daemon running.
    fn edit_worktree_index(&self, sql: &str) {
        let conn = Connection::open(project_dir(&self.worktree).unwrap().join("index.db")).unwrap();
        conn.execute_batch(sql).unwrap();
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for root in [&self.worktree, &self.main] {
            if let Ok(state) = project_dir(root) {
                for (_, path) in daemon::registry::discovered_pid_files(&state) {
                    common::kill_pid_file(&path);
                }
                common::kill_pid_file(&daemon::pid_path_in(&state));
                let _ = fs::remove_dir_all(&state);
            }
        }
    }
}

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

/// A read-only view of a daemon's index file.
struct Index(Connection);

impl Index {
    fn open(root: &Path) -> Option<Self> {
        let path = project_dir(root).ok()?.join("index.db");
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok().map(Self)
    }

    fn semantic_pass_completed(&self) -> bool {
        g_mesh::storage::schema::semantic_pass_completed(&self.0).unwrap_or(false)
    }

    /// Whether the seed's first-start reconcile is stamped; a failed read
    /// reads as "not yet".
    fn seed_reconciled(&self) -> bool {
        self.0
            .query_row("SELECT reconciledAt IS NOT NULL FROM seed_origin WHERE id = 1", [], |row| row.get(0))
            .unwrap_or(false)
    }

    fn semantic_pending_files(&self) -> Vec<String> {
        self.strings("SELECT filePath FROM semantic_pending_files ORDER BY 1", [])
    }

    fn files(&self) -> Vec<String> {
        self.strings("SELECT filePath FROM nodes WHERE kind = 'File' ORDER BY 1", [])
    }

    fn defined_in(&self, name: &str) -> Vec<String> {
        self.strings("SELECT filePath FROM nodes WHERE name = ?1 AND kind = 'Function' ORDER BY 1", [name])
    }

    fn strings<P: rusqlite::Params>(&self, sql: &str, params: P) -> Vec<String> {
        let Ok(mut statement) = self.0.prepare(sql) else { return vec![format!("unreadable: {sql}")] };
        statement
            .query_map(params, |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .unwrap_or_else(|err| vec![format!("unreadable: {err}")])
    }
}

fn text(result: &CallToolResult) -> String {
    match &result.content[0] {
        ContentBlock::Text(block) => block.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// `find_definition` by name alone, as JSON; a tool error panics with its
/// text.
async fn definition(client: &RunningService<RoleClient, ()>, name: &str) -> Value {
    let result = client
        .call_tool(
            CallToolRequestParams::new("find_definition".to_string())
                .with_arguments(json!({ "symbol_name": name }).as_object().cloned().unwrap()),
        )
        .await
        .unwrap_or_else(|err| panic!("find_definition {name} must return a result: {err}"));
    assert_ne!(result.is_error, Some(true), "find_definition {name} failed: {}", text(&result));
    serde_json::from_str(&text(&result)).expect("tool result is not JSON")
}

/// AC2: the seeded worktree answers an unchanged symbol exactly as the main
/// checkout does and a worktree-only change with the worktree's content; the
/// worktree's deletion, main's untracked file and a file the worktree
/// gitignores are gone; nothing is reindexed whole (the watch files are
/// identical); the reparsed files get their semantic pass.
///
/// Controls (code reverts, never the test):
/// - skip `reconcile_since_index` in `spawn_watch_consumer`'s `Seeded` arm
///   (seed only): `changedThing` answers main's source;
/// - seeded mode with the plain deleted rule (`unwalked` empty, `deleted` =
///   live and absent on disk): `generated/gen.ts` stays, `2 deleted`;
/// - drop the `owed_semantic_file_passes` call in `spawn_watch_consumer`:
///   the reparsed files stay semantic-pending.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_seeded_worktree_answers_like_main_and_with_its_own_changes() {
    let mut harness = Harness::new();
    let main_kept = harness.index_main().await;
    harness.add_worktree();
    let worktree = harness.worktree.clone();

    let client = harness.connect(&worktree, &[]).await;
    let kept = definition(&client, "keptThing").await;
    let changed = definition(&client, "changedThing").await;
    let created = definition(&client, "createdThing").await;

    assert!(harness.log_text().contains("index seeded from"), "daemon log:\n{}", harness.log_text());
    assert_eq!(kept, main_kept, "an unchanged file answers as the main checkout's index does");
    let changed_text = changed.to_string();
    assert!(
        changed_text.contains("return 2") && !changed_text.contains("return 1"),
        "the changed symbol answers the worktree's source: {changed}"
    );
    assert!(created.to_string().contains("src/created.ts"), "{created}");
    assert_eq!(
        harness.reconciled().await,
        Reconciled { seeded: true, deleted: 3, created: 1, reindexed: 2 },
        "src/deleted.ts, src/untracked.ts and generated/gen.ts retired; src/created.ts and src/changed.ts \
         reparsed"
    );
    harness.await_seed_settled().await;
    let index = Index::open(&worktree).unwrap();
    let files = index.files();
    for gone in ["src/deleted.ts", "src/untracked.ts", "generated/gen.ts"] {
        assert!(!files.contains(&gone.to_string()), "{gone} must be gone: {files:?}");
    }
    assert!(index.defined_in("genThing").is_empty() && index.defined_in("untrackedThing").is_empty());
    assert_eq!(index.defined_in("changedThing"), ["src/changed.ts"]);
    drop(index);
    let log = harness.log_text();
    assert!(
        !log.contains("reindexing typescript"),
        "identical watch files reindex nothing; daemon log:\n{log}"
    );
    harness.stop(&worktree, client).await;
}

/// A tool call made while the seeded reconcile is held waits for it, and
/// answers with the worktree's content (a symbol only the worktree has).
///
/// Control: start a seeded index at `Structural` instead of `Walking` in
/// `daemon::run`: the call answers from the main checkout's copy, which has
/// no `createdThing`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_call_waits_for_the_seeded_reconcile() {
    let mut harness = Harness::new();
    harness.index_main().await;
    harness.add_worktree();
    let worktree = harness.worktree.clone();
    let hold = harness.aux.path().join("hold-the-seeded-reconcile");
    fs::write(&hold, "").unwrap();

    let client = harness.connect(&worktree, &[(HOLD_ENV, &hold)]).await;
    harness.await_log_line("holding the seeded reconcile").await;
    let client = std::sync::Arc::new(client);
    let call = tokio::spawn({
        let client = std::sync::Arc::clone(&client);
        async move { definition(&client, "createdThing").await }
    });
    // Time for an ungated daemon to answer from the copy; a gated one waits.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !call.is_finished() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!call.is_finished(), "the call answered while the seeded reconcile was held");
    fs::remove_file(&hold).unwrap();
    let created = call.await.unwrap();
    assert!(created.to_string().contains("src/created.ts"), "{created}");
    harness.await_seed_settled().await;
    let client = std::sync::Arc::into_inner(client).expect("the call is done with the client");
    harness.stop(&worktree, client).await;
}

/// A start that died before stamping its seeded reconcile (`reconciledAt`
/// cleared with no daemon running: the state it leaves) runs the seeded
/// reconcile again, and compares the watch files again: a differing
/// `tsconfig.json` reindexes TypeScript. Once stamped, the next start is a
/// plain one, and retries the per-file semantic pass a semantic-pending file
/// is still owed.
///
/// Controls (code reverts, never the test):
/// - drop the `reconcile_watch_files_with_seed` call in
///   `spawn_watch_consumer`: no "differs from the seed's source" line;
/// - drop the `owed_semantic_file_passes` call in `spawn_watch_consumer`:
///   the owed row of the third start stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unstamped_seed_is_reconciled_again_and_a_stamped_one_starts_plain() {
    let mut harness = Harness::new();
    harness.index_main().await;
    harness.add_worktree();
    let worktree = harness.worktree.clone();

    let client = harness.connect(&worktree, &[]).await;
    assert!(harness.reconciled().await.seeded);
    harness.await_seed_settled().await;
    harness.stop(&worktree, client).await;

    harness.edit_worktree_index("UPDATE seed_origin SET reconciledAt = NULL WHERE id = 1;");
    write(&worktree, "tsconfig.json", "{ \"compilerOptions\": { \"strict\": false } }\n");
    let client = harness.connect(&worktree, &[]).await;
    let reconciled = harness.reconciled().await;
    assert!(reconciled.seeded, "{reconciled:?}");
    assert!(harness.log_text().contains("the index is a seed"));
    assert!(!harness.log_text().contains("index seeded from"), "the existing index is not copied again");
    harness.await_log_line("tsconfig.json differs from the seed's source - reindexing typescript").await;
    harness.await_seed_settled().await;
    harness.stop(&worktree, client).await;

    harness.edit_worktree_index(
        "INSERT INTO semantic_pending_files (language, filePath) VALUES ('typescript', 'src/kept.ts');",
    );
    let client = harness.connect(&worktree, &[]).await;
    let reconciled = harness.reconciled().await;
    assert!(!reconciled.seeded, "{reconciled:?}");
    assert!(!harness.log_text().contains("the index is a seed"));
    harness
        .await_index(&worktree, "the owed semantic-pending file's pass", |index| {
            index.semantic_pending_files().is_empty()
        })
        .await;
    harness.await_log_line("semantic pass over 1 semantic-pending typescript file(s)").await;
    harness.stop(&worktree, client).await;
}
