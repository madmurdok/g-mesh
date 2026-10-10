//! Daemon starts over two fake languages, `alpha` and `beta`: a full walk,
//! then each later start's activation, built as `daemon::run` builds it for a
//! walked project. Every start opens its own connection and registry, so
//! nothing but the index carries over from one start to the next.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::*;
use crate::daemon::manifest::discover;
use crate::daemon::test_plugin;
use crate::storage::connection;

const GENERATION: &str = "test-generation";
const ALPHA_FILE: &str = "src/a.alpha-src";
const BETA_FILE: &str = "src/b.beta-src";

/// How long a wait for a spawned process or a watcher event may take before
/// the test fails: generous, for a loaded machine.
const PATIENCE: Duration = Duration::from_secs(120);

/// One NDJSON node line.
fn node_line(id: &str, kind: &str, file_path: &str, language: &str) -> String {
    serde_json::json!({
        "id": id,
        "kind": kind,
        "name": id,
        "qualifiedName": id,
        "filePath": file_path,
        "range": { "start": { "line": 0, "col": 0 }, "end": { "line": 1, "col": 0 } },
        "visibility": "public",
        "language": language,
    })
    .to_string()
}

/// `language`'s walk of `file`: the file and one function in it.
fn one_file_stream(language: &str, file: &str) -> Vec<String> {
    vec![
        node_line(&format!("file:{file}"), "File", file, language),
        node_line(&format!("{language}-f1"), "Function", file, language),
    ]
}

/// Writes `rel` under `root` with an mtime an hour back, so a walk records
/// its staleness baseline.
fn write_old_file(root: &Path, rel: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "x").unwrap();
    let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
    std::fs::File::options().write(true).open(&path).unwrap().set_modified(an_hour_ago).unwrap();
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + PATIENCE;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// Opens the gate of [`test_plugin::gate_bulk_walks`] when dropped, so a
/// failing assertion never leaves a walk (and the thread joined on it) held.
struct BulkGate<'a>(&'a Path);

impl Drop for BulkGate<'_> {
    fn drop(&mut self) {
        test_plugin::open_bulk_walk_gate(self.0);
    }
}

struct Fixture {
    _project: tempfile::TempDir,
    _plugins: tempfile::TempDir,
    /// The project root, canonical, as `daemon::run` resolves it.
    root: PathBuf,
    alpha: PathBuf,
    beta: PathBuf,
    discovered: DiscoveredPlugins,
}

/// One start's activation, not yet run, and what it shares.
struct Start {
    ctx: ActivationCtx,
    registry: Arc<PluginRegistry>,
}

impl Fixture {
    /// `alpha` and `beta`, one file each; both walks succeed until a test
    /// says otherwise.
    fn new() -> Self {
        let project = tempfile::tempdir().expect("failed to create a project root");
        let plugins = tempfile::tempdir().expect("failed to create a plugin root");
        let alpha = test_plugin::install(plugins.path(), "alpha", &[".alpha-src"]);
        let beta = test_plugin::install(plugins.path(), "beta", &[".beta-src"]);
        let root = std::fs::canonicalize(project.path()).expect("failed to resolve the project root");
        write_old_file(&root, ALPHA_FILE);
        write_old_file(&root, BETA_FILE);
        test_plugin::set_bulk_stream(&root, "alpha", &one_file_stream("alpha", ALPHA_FILE), 0);
        let discovered = discover(&[plugins.path().to_path_buf()]).expect("the fixture manifests discover");
        let fixture = Self { _project: project, _plugins: plugins, root, alpha, beta, discovered };
        fixture.beta_exits(0);
        fixture
    }

    /// Every later walk of beta streams its file, then exits with `code`.
    fn beta_exits(&self, code: i32) {
        test_plugin::set_bulk_stream(&self.root, "beta", &one_file_stream("beta", BETA_FILE), code);
    }

    fn open(&self) -> IndexStore {
        let conn = connection::open(&self.root).expect("failed to open the index");
        schema::ensure_current(&conn, GENERATION).expect("failed to check the index");
        IndexStore::new(conn)
    }

    /// A connection of the test's own, for reading what the starts left.
    fn read(&self) -> Connection {
        connection::open(&self.root).expect("failed to open the index")
    }

    /// The full walk (`bulk_index::run`) and its completion marker, as an
    /// earlier daemon's walk or `g-mesh init` leaves them.
    fn full_walk(&self) {
        let store = self.open();
        bulk_index::run(&self.root, &store, &self.discovered).expect("the walk must not fail outright");
        store.with(schema::record_bulk_index).expect("failed to record the walk");
    }

    /// A start of a walked project, as `daemon::run` builds its activation:
    /// the failed set seeded from the index and the retries it owes; with
    /// `watcher`, the watcher it registered and left for activation to drain.
    fn prepare(&self, watcher: bool) -> Start {
        let store = Arc::new(self.open());
        assert!(store.with(schema::bulk_index_completed).unwrap(), "a start here follows a walk");
        let needs_semantic_pass_retry = !store.with(schema::semantic_pass_completed).unwrap();
        let languages: Vec<&str> = self.discovered.manifests.keys().map(String::as_str).collect();
        let retry_languages = store.with(|conn| schema::languages_owed_a_retry(conn, &languages)).unwrap();
        let state_dir = connection::project_dir(&self.root).expect("failed to resolve the state directory");
        let embedding = Arc::new(EmbeddingPipeline::disabled());
        let registry = Arc::new(PluginRegistry::new(
            &self.root,
            state_dir,
            self.discovered.clone(),
            None,
            None,
            Arc::clone(&embedding),
        ));
        registry.seed_failed_languages(&store);
        let watcher = watcher.then(|| ProjectWatcher::new(&self.root).expect("failed to start the watcher"));
        let ctx = ActivationCtx {
            conn: Arc::clone(&store),
            registry: Arc::clone(&registry),
            embedding,
            discovered_for_bulk_index: self.discovered.clone(),
            canonical_root: self.root.clone(),
            root: self.root.clone(),
            indexing: IndexingStatus::structural(),
            core_activity: CoreActivity::new(),
            needs_walk: false,
            needs_semantic_pass_retry,
            retry_languages,
            watcher,
        };
        Start { ctx, registry }
    }

    /// One whole start: its activation, run to the end. Returns the
    /// registry, to ask what it routes.
    fn start(&self) -> Arc<PluginRegistry> {
        let Start { mut ctx, registry, .. } = self.prepare(false);
        ctx.activate().expect("the activation of a walked index succeeds");
        registry
    }
}

fn outcome(conn: &Connection, language: &str) -> Option<LanguageOutcome> {
    schema::language_outcomes(conn)
        .unwrap()
        .into_iter()
        .find_map(|(recorded, outcome)| (recorded == language).then_some(outcome))
}

fn retries(conn: &Connection) -> BTreeMap<String, u32> {
    schema::language_retries(conn).unwrap()
}

fn rows(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(sql).unwrap().query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap()
}

/// Every alpha row, rowid included, so a rewrite shows even with the same
/// values.
fn alpha_rows(conn: &Connection) -> Vec<String> {
    rows(
        conn,
        "SELECT 'node ' || rowid || ' ' || id || ' ' || filePath FROM nodes WHERE language = 'alpha'
         UNION ALL
         SELECT 'file ' || rowid || ' ' || filePath || ' ' || mtimeMillis FROM indexed_files
          WHERE filePath LIKE '%.alpha-src'
         ORDER BY 1",
    )
}

/// Beta fails the walk, then works: the next start re-walks beta alone and
/// swaps it in, without `g-mesh reindex`. Its outcome is `indexed`, its retry
/// count is gone, its file has a staleness baseline and the registry routes
/// it again; alpha was walked once in all and its rows are untouched.
///
/// Controls: drop the `retry_failed_languages` call in `activate` (beta stays
/// absent); drop `retried`'s `record_language_retry_succeeded` in
/// `language_swap::swap_attached` (the outcome stays `failed` and the retry
/// row stays); drop the `record_walk_baselines` call in
/// `workspace_reindex::retry_failed` (no baseline); drop
/// `clear_failed_language` there (beta is still not routed).
#[test]
fn a_language_that_failed_the_walk_is_retried_and_indexed_on_the_next_start() {
    let fixture = Fixture::new();
    fixture.beta_exits(1);
    fixture.full_walk();
    let conn = fixture.read();
    assert!(matches!(outcome(&conn, "beta"), Some(LanguageOutcome::Failed { .. })), "beta failed the walk");
    let alpha_before = alpha_rows(&conn);
    assert!(!alpha_before.is_empty(), "alpha was walked");

    fixture.beta_exits(0);
    let registry = fixture.start();

    assert_eq!(test_plugin::bulk_walks(&fixture.beta), 2, "the walk, then the retry");
    assert_eq!(test_plugin::bulk_walks(&fixture.alpha), 1, "a retry re-walks the failed language only");
    assert!(
        matches!(outcome(&conn, "beta"), Some(LanguageOutcome::Indexed { files: 1 })),
        "{:?}",
        outcome(&conn, "beta")
    );
    assert_eq!(
        rows(&conn, "SELECT id FROM nodes WHERE language = 'beta' ORDER BY id"),
        ["beta-f1", "file:src/b.beta-src"]
    );
    assert!(retries(&conn).is_empty(), "a successful retry drops the count: {:?}", retries(&conn));
    assert_eq!(
        rows(&conn, "SELECT filePath FROM indexed_files WHERE filePath LIKE '%.beta-src'"),
        [BETA_FILE]
    );
    assert!(!registry.is_failed_language("beta"), "the retried language is routed again");
    assert_eq!(alpha_rows(&conn), alpha_before, "alpha's rows are untouched");
}

/// A walk that always fails is bounded: over four starts after the walk that
/// failed it, beta is walked three times in all (the walk and two retries),
/// and the fourth start spawns nothing of beta's. The outcome keeps the
/// latest retry's cause, not the walk's.
///
/// Controls: in `schema::languages_owed_a_retry` drop the `< MAX_LANGUAGE_RETRIES`
/// filter (beta is walked on every start: 5); drop the
/// `record_language_retry_failed` call in `retry_failed_languages` (the
/// error still names exit status 1).
#[test]
fn a_language_that_always_fails_is_walked_at_most_three_times() {
    let fixture = Fixture::new();
    fixture.beta_exits(1);
    fixture.full_walk();
    fixture.beta_exits(2);

    let mut walks = Vec::new();
    let mut spawned_by_the_fourth = None;
    for start in 1..=4 {
        let spawned_before = test_plugin::spawns(&fixture.beta).len();
        drop(fixture.start());
        walks.push(test_plugin::bulk_walks(&fixture.beta));
        if start == 4 {
            spawned_by_the_fourth = Some(test_plugin::spawns(&fixture.beta).len() - spawned_before);
        }
    }

    assert_eq!(walks, [2, 3, 3, 3], "the walk plus one retry on each of the next two starts");
    assert_eq!(spawned_by_the_fourth, Some(0), "a start with no retry left spawns nothing of beta's");
    let conn = fixture.read();
    assert_eq!(retries(&conn), BTreeMap::from([("beta".to_string(), schema::MAX_LANGUAGE_RETRIES)]));
    match outcome(&conn, "beta") {
        Some(LanguageOutcome::Failed { error }) => {
            assert!(error.contains("exit status: 2"), "the latest retry's cause: {error}")
        }
        other => panic!("beta must still be failed: {other:?}"),
    }
}

/// A retry is counted before its walk: while the retry's walk is in flight
/// (held by the fake plugin's gate), the count is already 1, so a retry that
/// hangs or whose daemon is killed still uses its attempt.
///
/// Control: call `schema::begin_language_retry` after
/// `workspace_reindex::retry_failed` returns in `retry_failed_languages`
/// (no row while the walk runs).
#[test]
fn a_retry_is_counted_before_its_walk_starts() {
    let fixture = Fixture::new();
    fixture.beta_exits(1);
    fixture.full_walk();
    test_plugin::gate_bulk_walks(&fixture.beta);
    let gate = BulkGate(&fixture.beta);

    let Start { mut ctx, .. } = fixture.prepare(false);
    let activation = thread::spawn(move || ctx.activate());
    wait_until("the retry's walk to start", || test_plugin::bulk_walks(&fixture.beta) == 2);

    let counted = retries(&fixture.read());
    drop(gate);
    activation.join().expect("the activation must not panic").expect("the activation succeeds");

    assert_eq!(counted, BTreeMap::from([("beta".to_string(), 1)]), "counted while the walk was in flight");
}

/// A full walk gives a language fresh retries: once beta's retries are used
/// up, `bulk_index::run` (here failing beta again) clears the count, and the
/// next start retries beta once more.
///
/// Control: drop the `DELETE FROM language_retry` in
/// `schema::record_language_outcomes` (the count stays at 2 and the last
/// start walks nothing).
#[test]
fn a_full_walk_gives_a_failed_language_fresh_retries() {
    let fixture = Fixture::new();
    fixture.beta_exits(1);
    fixture.full_walk();
    for _ in 0..schema::MAX_LANGUAGE_RETRIES {
        drop(fixture.start());
    }
    assert_eq!(test_plugin::bulk_walks(&fixture.beta), 3, "the bound is used up");

    fixture.full_walk();
    assert!(retries(&fixture.read()).is_empty(), "the full walk clears the count");
    assert_eq!(test_plugin::bulk_walks(&fixture.beta), 4);

    fixture.beta_exits(0);
    let registry = fixture.start();

    assert_eq!(test_plugin::bulk_walks(&fixture.beta), 5, "the start after the full walk retries beta");
    assert!(matches!(outcome(&fixture.read(), "beta"), Some(LanguageOutcome::Indexed { .. })));
    assert!(!registry.is_failed_language("beta"));
}

/// The watcher's consumer starts after the retry: edits made while beta's
/// retry walk is in flight (alpha's too) are not routed meanwhile, and once
/// the retry is done both are, beta's to a language no longer failed. The
/// watcher is the one `daemon::run` registers and leaves for activation.
///
/// The hold is checked over a window: the consumer, if it ran, would route
/// alpha's edit within it. The rest waits for events the code guarantees.
///
/// Control: start the watcher's consumer before `retry_failed_languages` in
/// `activate` (alpha's edit routes during the hold and beta's is dropped as
/// failed).
#[test]
fn edits_made_during_a_retry_are_routed_once_it_is_done() {
    let fixture = Fixture::new();
    fixture.beta_exits(1);
    fixture.full_walk();
    fixture.beta_exits(0);
    test_plugin::gate_bulk_walks(&fixture.beta);
    let gate = BulkGate(&fixture.beta);

    let Start { mut ctx, .. } = fixture.prepare(true);
    let activation = thread::spawn(move || ctx.activate());
    wait_until("the retry's walk to start", || test_plugin::bulk_walks(&fixture.beta) == 2);
    std::fs::write(fixture.root.join(BETA_FILE), "beta edited during the retry").unwrap();
    std::fs::write(fixture.root.join(ALPHA_FILE), "alpha edited during the retry").unwrap();
    thread::sleep(Duration::from_secs(5));
    let routed_during_the_hold = test_plugin::file_changed_requests(&fixture.alpha);
    drop(gate);
    activation.join().expect("the activation must not panic").expect("the activation succeeds");

    assert!(routed_during_the_hold.is_empty(), "nothing routes during the retry: {routed_during_the_hold:?}");
    let routed = |dir: &Path, file: &str| {
        test_plugin::file_changed_requests(dir).iter().any(|path| path.ends_with(file))
    };
    wait_until("alpha's edit to route", || routed(&fixture.alpha, ALPHA_FILE));
    wait_until("beta's edit to route", || routed(&fixture.beta, BETA_FILE));
}
