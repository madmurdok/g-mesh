//! Cold-start bulk index: the one full walk of a project that gives a
//! never-indexed codebase a populated graph before its daemon answers
//! anything. The file watcher cannot do this: `notify` reports only changes
//! made while it runs.
//!
//! Each discovered language's plugin is spawned in its one-shot
//! `--bulk-index` mode, one process per language, from the manifest's own
//! `command`/`args`. Its stdout is the EOF-terminated NDJSON stream
//! `protocol::ndjson::NdjsonReader` consumes. Why a second process rather
//! than the control-plane pipe, and why one failed language fails the whole
//! walk: [ADR 0002](../../../docs/adr/0002-bulk-walk.md).

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::daemon::indexing_status::IndexingStatus;
use crate::daemon::manifest::{DiscoveredPlugins, PluginManifest};
use crate::daemon::plugin;
use crate::embedding::EmbeddingPipeline;
use crate::protocol::ndjson::{BulkItem, NdjsonReader};
use crate::storage::index_store::{IndexStore, Unit, Writer};
use crate::storage::schema;
use crate::storage::write::Diff;
use crate::watcher::apply::{to_edge_record, to_node_record};
use crate::watcher::staleness;

/// Puts the plugin in one-shot bulk-index mode; must stay in sync with
/// `BULK_INDEX_FLAG` in plugins/typescript/src/index.ts.
pub(crate) const BULK_INDEX_FLAG: &str = "--bulk-index";

/// Set to `1` on every bulk spawn, telling the plugin its stdin is a lifeline:
/// a pipe core holds open and never writes, whose EOF means core is gone, so
/// the walk should stop. Opt-in by the spawner, so a plugin run by an older
/// core or by hand with `< /dev/null` does not read an immediate EOF as "exit
/// before walking". Must stay in sync with the plugins' own copies
/// (`plugins/sdk/src/run.rs`, `plugins/go/main.go`,
/// `plugins/typescript/src/index.ts`) - see
/// `docs/architecture/plugin-lifetime.md` §2.
pub(crate) const BULK_STDIN_LIFELINE_ENV: &str = "G_MESH_BULK_STDIN_LIFELINE";

/// Nodes plus edges accumulated before a batch is committed: bounds both the
/// memory held before a write and the number of transactions.
const BATCH_ITEMS: usize = 2_000;

/// `NodeKind::File`'s storage spelling (`watcher::apply::to_node_record`).
const FILE_NODE_KIND: &str = "File";

/// Test-only: holds a finished walk open for this many milliseconds before
/// [`run`] returns, so the daemon has not yet recorded the walk as complete.
/// Real installs never set it.
///
/// Applied *after* everything is committed: a test can wait for the row it is
/// about to ask for and only then ask, so a "still indexing" answer proves the
/// indexing flag gates the response rather than an empty table.
pub const WALK_DELAY_ENV: &str = "G_MESH_BULK_INDEX_DELAY_MS";

/// Test-only: path whose *deletion* releases the finished walk, so completion
/// is an event the test causes rather than a duration (unlike
/// [`WALK_DELAY_ENV`]). [`HOLD_LOCK_FILE_ENV`] is the sibling knob that holds
/// the batch-commit lock itself open.
pub const WALK_HOLD_FILE_ENV: &str = "G_MESH_BULK_INDEX_HOLD_FILE";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BulkIndexSummary {
    pub nodes: usize,
    pub edges: usize,
    /// Lines that parsed as neither a node nor an edge. Skipped rather than
    /// fatal, matching `NdjsonReader`'s contract: one unreadable line costs
    /// one symbol, not the project's whole index.
    pub skipped_lines: usize,
    /// `IMPORTS` edges the post-walk linking pass repointed from a module
    /// placeholder onto the real file it names (`graph::imports`).
    pub linked_imports: usize,
    /// `CALLS`/`REFERENCES`/`SUPERTYPE_OF` edges the post-walk linking pass
    /// repointed from a pending-symbol placeholder onto the symbol another
    /// file exports (`graph::symbol_links`).
    pub linked_symbols: usize,
}

/// Everything one walk carries from language to language and batch to batch:
/// the store it writes, what it reports to, and what it accumulates.
pub(crate) struct WalkContext<'a> {
    pub(crate) store: &'a IndexStore,
    /// `None` for the structural-only cold-start walk (`embedding::backfill`
    /// fills the vectors afterwards); `Some` for
    /// `daemon::workspace_reindex`'s per-language re-walk.
    pub(crate) embedding: Option<&'a EmbeddingPipeline>,
    /// The daemon's walk counters; `None` when nobody is waiting on them.
    pub(crate) progress: Option<&'a IndexingStatus>,
    pub(crate) summary: BulkIndexSummary,
    /// The file paths of every `File` node ingested, when the caller records
    /// staleness baselines for them; `None` otherwise.
    pub(crate) walked_files: Option<BTreeSet<String>>,
}

impl<'a> WalkContext<'a> {
    /// A structural walk into `store` that reports nowhere and records no
    /// walked files; set the other fields with struct-update syntax.
    pub(crate) fn new(store: &'a IndexStore) -> Self {
        Self {
            store,
            embedding: None,
            progress: None,
            summary: BulkIndexSummary::default(),
            walked_files: None,
        }
    }
}

/// Walks `project_root` through every plugin `discovered` names - one
/// one-shot `--bulk-index` process per language, run one after another - and
/// commits everything each of them emits, returning only once every child has
/// exited and the last batch is durable. `daemon::run` turns that return into
/// the moment its `IndexingStatus` flips and tools start answering for real,
/// so "returned" has to mean "complete".
///
/// Batching is safe to cut anywhere in one language's stream even though
/// edges are foreign keys onto nodes: a plugin emits a file's nodes before
/// that same file's edges, and never an edge between files (see the
/// dangling-edge guard in extract.ts). Every cross-file edge appears only
/// afterwards, when `graph::imports` and `graph::symbol_links` link the walk's
/// placeholders - once, project-wide, after every language's stream is over,
/// because only then does every node that will exist, exist.
pub fn run(
    project_root: &Path,
    conn: &IndexStore,
    embedding: Option<&EmbeddingPipeline>,
    discovered: &DiscoveredPlugins,
) -> Result<BulkIndexSummary> {
    run_with_progress(project_root, conn, embedding, discovered, None)
}

/// [`run`], also reporting through `progress`'s walk counters: languages done
/// out of total, the language being walked, and items ingested so far.
pub fn run_with_progress(
    project_root: &Path,
    conn: &IndexStore,
    embedding: Option<&EmbeddingPipeline>,
    discovered: &DiscoveredPlugins,
    progress: Option<&IndexingStatus>,
) -> Result<BulkIndexSummary> {
    // Sorted so languages are walked, and a failure is named, in a
    // deterministic order. Ingestion order does not change the result.
    let mut manifests: Vec<&PluginManifest> = discovered.manifests.values().collect();
    manifests.sort_by(|a, b| a.language.cmp(&b.language));

    if let Some(progress) = progress {
        progress.start_walk_progress(u32::try_from(manifests.len()).unwrap_or(u32::MAX));
    }
    // Taken before the first plugin is spawned, so no plugin can have read a
    // file before it - `staleness::record_walk_baselines` relies on that.
    let walk_started = std::time::SystemTime::now();
    let mut ctx =
        WalkContext { embedding, progress, walked_files: Some(BTreeSet::new()), ..WalkContext::new(conn) };
    for manifest in manifests {
        if let Some(progress) = progress {
            progress.mark_language_started(&manifest.language);
        }
        // One language's failure fails the whole walk, even for a language
        // the project has no files of: an index that silently skipped a
        // language would look complete to every tool (ADR 0002).
        walk_one_language(project_root, manifest, &mut ctx)?;
        if let Some(progress) = progress {
            progress.mark_language_done();
        }
    }
    let WalkContext { mut summary, walked_files, .. } = ctx;
    let walked_files = walked_files.unwrap_or_default();

    // Once, after every language's stream: an import links only to a file
    // that is already a node, and a cross-file usage only once every language
    // that might define it has run.
    let links = conn.link_all()?;
    summary.linked_imports = links.imports;
    summary.linked_symbols = links.symbols;

    // A staleness baseline for every walked file, so the first query of an
    // untouched file takes `staleness::ensure_fresh`'s fast path instead of a
    // synchronous reindex. Written after linking, so a row only exists for a
    // complete graph, and before the walk is reported done, so no query races
    // it. Best-effort: without baselines the walk is still complete; its files
    // reindex on first touch.
    match staleness::record_walk_baselines(
        conn,
        project_root,
        walked_files.iter().map(String::as_str),
        walk_started,
    ) {
        Ok(baselines) => {
            if baselines.skipped > 0 {
                eprintln!(
                    "g-mesh: {} of {} walked files got no staleness baseline (modified during or just \
                     before the walk) - each reindexes on its first query",
                    baselines.skipped,
                    walked_files.len()
                );
            }
        }
        Err(err) => eprintln!(
            "g-mesh: could not record the walk's staleness baselines - every file reindexes on its first \
             query: {err:#}"
        ),
    }

    hold_the_walk_open_for_tests();
    Ok(summary)
}

/// Spawns `manifest`'s plugin in its one-shot `--bulk-index` mode for
/// `project_root` and folds everything it emits into `ctx`, returning only
/// once the child has exited and every batch it produced is durable. Also
/// called by `daemon::workspace_reindex` to re-walk one language after
/// deleting its rows.
///
/// Runs as one [`Unit::BulkWalk`]: its batch commits and its bookkeeping row
/// are the unit's steps.
pub(crate) fn walk_one_language(
    project_root: &Path,
    manifest: &PluginManifest,
    ctx: &mut WalkContext<'_>,
) -> Result<()> {
    let store = ctx.store;
    store.unit(Unit::BulkWalk, |store| walk_one_language_in(project_root, manifest, store, ctx))
}

fn walk_one_language_in(
    project_root: &Path,
    manifest: &PluginManifest,
    store: &mut Writer<'_>,
    ctx: &mut WalkContext<'_>,
) -> Result<()> {
    // The check `daemon::plugin::PluginState::spawn` makes too: an unbuilt
    // cargo-workspace plugin binary gets a message naming the build command
    // instead of a bare "No such file or directory".
    if let Some(hint) = plugin::missing_plugin_binary_hint(&manifest.command, &manifest.args) {
        bail!("failed to spawn the {} plugin's bulk index: {hint}", manifest.language);
    }

    let mut command = Command::new(&manifest.command);
    command
        .args(&manifest.args)
        .arg(BULK_INDEX_FLAG)
        .arg(project_root)
        // The lifeline: a pipe this process never writes to. It stays inside
        // `child` - never taken, never dropped early - so the plugin sees EOF
        // exactly when this process's end closes, which the kernel does even
        // on SIGKILL. `Child::wait` below closes it before waiting, which is
        // harmless: by then stdout has reached EOF, so the plugin is already
        // exiting. The env var arms the plugin's watcher.
        .stdin(Stdio::piped())
        .env(BULK_STDIN_LIFELINE_ENV, "1")
        .stdout(Stdio::piped())
        // Plugin logs are diagnostic only: they go wherever the daemon's own
        // stderr goes.
        .stderr(Stdio::inherit());
    let mut child = crate::process::spawn_serialized(&mut command).with_context(|| {
        format!(
            "failed to spawn the {} plugin's bulk index ({})",
            manifest.language,
            manifest.command.display()
        )
    })?;

    let stdout = child.stdout.take().context("bulk-index plugin process has no stdout")?;

    if let Err(err) = ingest_in(BufReader::new(stdout), store, ctx) {
        // Nobody will read the rest of this walk: a plugin left writing into
        // an undrained pipe would otherwise outlive the failure.
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    let status = child.wait().context("failed to wait for the bulk-index plugin process")?;
    if !status.success() {
        bail!("the {} plugin's bulk index exited with {status}", manifest.language);
    }

    // This language's `language_state.bulkIndexedAt`, recorded here because
    // this is the one place that knows which language just finished;
    // `schema::record_bulk_index` is the project-wide roll-up its caller
    // writes once, after every language. The one write site of
    // `pluginFingerprint`, the digest `daemon::registry::indexer_version`
    // computes per plugin.
    store
        .step(|conn| {
            schema::record_language_bulk_indexed(
                conn,
                &manifest.language,
                Some(&plugin::fingerprint(manifest)),
            )
        })
        .with_context(|| format!("failed to record that {} was bulk-indexed", manifest.language))?;

    Ok(())
}

/// Honors [`WALK_HOLD_FILE_ENV`] and [`WALK_DELAY_ENV`]. A no-op unless one is
/// set, which is every real run.
fn hold_the_walk_open_for_tests() {
    // The file gate wins: a test that uses it wants the release to be its own
    // action, not blurred by a delay on top.
    if let Some(path) = std::env::var_os(WALK_HOLD_FILE_ENV).filter(|p| !p.is_empty()) {
        let path = std::path::PathBuf::from(path);
        eprintln!(
            "g-mesh daemon: holding the finished bulk walk until {} is removed ({WALK_HOLD_FILE_ENV})",
            path.display()
        );
        // Polled, and bounded so a test that forgets to release it fails as a
        // test timeout rather than wedging the daemon.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        return;
    }

    let Some(millis) = std::env::var(WALK_DELAY_ENV).ok().and_then(|v| v.trim().parse().ok()) else {
        return;
    };
    eprintln!("g-mesh daemon: holding the finished bulk walk open for {millis}ms ({WALK_DELAY_ENV})");
    std::thread::sleep(std::time::Duration::from_millis(millis));
}

/// Reads one language's whole NDJSON bulk stream to EOF into `ctx`,
/// committing it in batches, as one [`Unit::BulkWalk`]. Lets the ingestion
/// rules run without a plugin on the other end.
///
/// Adds only to `nodes`/`edges`/`skipped_lines`: `linked_imports`/
/// `linked_symbols` are [`run`]'s, computed once after every language.
pub(crate) fn ingest<R: BufRead>(reader: R, ctx: &mut WalkContext<'_>) -> Result<()> {
    let store = ctx.store;
    store.unit(Unit::BulkWalk, |store| ingest_in(reader, store, ctx))
}

fn ingest_in<R: BufRead>(reader: R, store: &mut Writer<'_>, ctx: &mut WalkContext<'_>) -> Result<()> {
    let mut batch = Diff::default();
    let mut batched = 0usize;

    for item in NdjsonReader::new(reader) {
        match item {
            Ok(BulkItem::Node(node)) => {
                let record = to_node_record(*node);
                // A `File` node is the plugin saying it parsed that file - the
                // one statement `run`'s baselines may rest on. Other kinds'
                // `filePath` need not be a file this walk read.
                if record.kind == FILE_NODE_KIND {
                    if let Some(walked) = ctx.walked_files.as_mut() {
                        walked.insert(record.file_path.clone());
                    }
                }
                batch.upsert_nodes.push(record);
                ctx.summary.nodes += 1;
            }
            Ok(BulkItem::Edge(edge)) => {
                batch.upsert_edges.push(to_edge_record(edge));
                ctx.summary.edges += 1;
            }
            Err(err) => {
                // A read failure (a broken pipe, say) repeats on every next
                // iteration, so skipping it like a malformed line would spin
                // forever: the stream is over.
                if err.downcast_ref::<std::io::Error>().is_some() {
                    return Err(err).context("failed to read the plugin's bulk-index stream");
                }
                eprintln!("g-mesh daemon: skipping malformed bulk-index line: {err:#}");
                ctx.summary.skipped_lines += 1;
                continue;
            }
        }

        if let Some(progress) = ctx.progress {
            progress.add_items_ingested(1);
        }
        batched += 1;
        if batched >= BATCH_ITEMS {
            commit(store, &mut batch, ctx)?;
            batched = 0;
        }
    }

    commit(store, &mut batch, ctx)?;
    Ok(())
}

/// Commits one batch and empties it. Embedding inference runs first, outside
/// the store; the commit and the vector store are then one step of the walk's
/// unit, so nothing inside the hold scales with inference.
fn commit(store: &mut Writer<'_>, batch: &mut Diff, ctx: &WalkContext<'_>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let embedding = ctx.embedding;
    let computed = embedding.map(|embedding| embedding.compute(batch)).unwrap_or_default();
    store.commit_batch(batch, embedding.map(|embedding| (embedding, computed.as_slice())))?;
    *batch = Diff::default();
    Ok(())
}

/// Path whose deletion releases a batch commit holding the store open; the
/// hook fires inside [`IndexStore::commit_batch`]'s hold.
pub use crate::storage::index_store::HOLD_LOCK_FILE_ENV;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{
        EdgeKind, NodeKind, Position, Range, SourceTier, Visibility, WireEdge, WireNode,
    };
    use crate::storage::schema;
    use rusqlite::Connection;
    use std::io::Cursor;

    fn setup_conn() -> IndexStore {
        let conn = Connection::open_in_memory().unwrap();
        // Foreign keys on, unlike the daemon's own connection: the point of
        // several of these tests is that batching never presents SQLite with
        // an edge whose endpoints aren't in yet, and that only bites when the
        // constraint is actually enforced.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        IndexStore::new(conn)
    }

    fn count(conn: &IndexStore, table: &str) -> i64 {
        conn.lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
    }

    fn node_line(id: &str) -> String {
        serde_json::to_string(&WireNode {
            id: id.to_string(),
            kind: NodeKind::Function,
            name: id.to_string(),
            qualified_name: id.to_string(),
            file_path: "src/a.ts".to_string(),
            range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 2, col: 0 } },
            signature: None,
            visibility: Visibility::Public,
            doc_comment: None,
            language: "typescript".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: None,
            container_parent: None,
            target: None,
        })
        .unwrap()
    }

    fn edge_line(id: &str, from: &str, to: &str) -> String {
        serde_json::to_string(&WireEdge {
            id: id.to_string(),
            from_id: from.to_string(),
            to_id: to.to_string(),
            kind: EdgeKind::Calls,
            source: SourceTier::Syntactic,
            engine: "tree-sitter".to_string(),
            resolved: false,
            to_declaration: None,
        })
        .unwrap()
    }

    fn ingest_str(stream: &str, conn: &IndexStore) -> Result<BulkIndexSummary> {
        let mut ctx = WalkContext::new(conn);
        ingest(Cursor::new(stream.as_bytes().to_vec()), &mut ctx)?;
        Ok(ctx.summary)
    }

    #[test]
    fn a_whole_stream_lands_in_sqlite() {
        let conn = setup_conn();
        let stream = format!("{}\n{}\n{}\n", node_line("n1"), node_line("n2"), edge_line("e1", "n1", "n2"));

        let summary = ingest_str(&stream, &conn).unwrap();

        assert_eq!(
            summary,
            BulkIndexSummary { nodes: 2, edges: 1, skipped_lines: 0, linked_imports: 0, linked_symbols: 0 }
        );
        assert_eq!(count(&conn, "nodes"), 2);
        assert_eq!(count(&conn, "edges"), 1);
        let name: String = conn
            .lock()
            .unwrap()
            .query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(name, "n1");
    }

    /// One unreadable line costs one symbol; refusing the whole walk over it
    /// would cost the project its entire index.
    #[test]
    fn a_malformed_line_is_counted_and_skipped_without_losing_the_rest() {
        let conn = setup_conn();
        let stream = format!("{}\nnot json at all\n{}\n", node_line("n1"), node_line("n2"));

        let summary = ingest_str(&stream, &conn).unwrap();

        assert_eq!(
            summary,
            BulkIndexSummary { nodes: 2, edges: 0, skipped_lines: 1, linked_imports: 0, linked_symbols: 0 }
        );
        assert_eq!(count(&conn, "nodes"), 2, "lines after a bad one must still be committed");
    }

    /// The batching contract: a stream longer than one batch commits every
    /// item exactly once, and a batch boundary landing between a node and an
    /// edge that points at it must not produce a dangling edge.
    #[test]
    fn a_stream_longer_than_one_batch_commits_all_of_it() {
        let conn = setup_conn();
        let total = BATCH_ITEMS + 5;
        let mut stream = String::new();
        for i in 0..total {
            stream.push_str(&node_line(&format!("n{i}")));
            stream.push('\n');
        }
        // Deliberately last, i.e. in the trailing partial batch, while its
        // endpoints were committed by the full batch before it.
        stream.push_str(&edge_line("e1", "n0", &format!("n{}", total - 1)));
        stream.push('\n');

        let summary = ingest_str(&stream, &conn).unwrap();

        assert_eq!(summary.nodes, total);
        assert_eq!(summary.edges, 1);
        assert_eq!(count(&conn, "nodes"), total as i64);
        assert_eq!(count(&conn, "edges"), 1);
    }

    #[test]
    fn an_empty_stream_is_a_no_op() {
        let conn = setup_conn();
        let summary = ingest_str("", &conn).unwrap();
        assert_eq!(summary, BulkIndexSummary::default());
        assert_eq!(count(&conn, "nodes"), 0);
    }

    /// Discovering two languages must spawn
    /// *both* plugins' one-shot `--bulk-index` mode and add their
    /// contributions together into one summary, not just walk the first (or
    /// only) one found. Each fake plugin (`daemon::test_plugin`) emits a
    /// fixed two nodes and one edge in bulk-index mode, so two languages
    /// summing to four nodes and two edges is only possible if both were
    /// actually spawned and both streams actually landed in the same index.
    #[test]
    fn discovering_two_languages_walks_both_and_sums_their_contributions() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        crate::daemon::test_plugin::install(plugins.path(), "alpha", &[".alpha-src"]);
        crate::daemon::test_plugin::install(plugins.path(), "beta", &[".beta-src"]);
        let discovered = crate::daemon::manifest::discover(&[plugins.path().to_path_buf()])
            .expect("the fixture plugins must discover cleanly");
        assert_eq!(discovered.manifests.len(), 2, "both fixture languages must have been discovered");

        let conn = setup_conn();

        let summary = run(project.path(), &conn, None, &discovered).expect("the multi-language walk failed");

        assert_eq!(summary.nodes, 4, "both languages' nodes must be counted, not just one's");
        assert_eq!(summary.edges, 2, "both languages' edges must be counted, not just one's");
        assert_eq!(summary.skipped_lines, 0);
        assert_eq!(count(&conn, "nodes"), 4, "both languages' nodes must have actually been committed");
        assert_eq!(count(&conn, "edges"), 2, "both languages' edges must have actually been committed");
    }

    /// A discovery naming only one language must still walk it: the loop over
    /// `discovered.manifests` must not accidentally require more than one
    /// entry to do anything.
    #[test]
    fn a_single_discovered_language_is_walked_same_as_before_the_registry_took_over() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        crate::daemon::test_plugin::install(plugins.path(), "solo", &[".solo-src"]);
        let discovered = crate::daemon::manifest::discover(&[plugins.path().to_path_buf()])
            .expect("the fixture plugin must discover cleanly");

        let conn = setup_conn();

        let summary = run(project.path(), &conn, None, &discovered).expect("the single-language walk failed");

        assert_eq!(summary.nodes, 2);
        assert_eq!(summary.edges, 1);
        assert_eq!(count(&conn, "nodes"), 2);
        assert_eq!(count(&conn, "edges"), 1);
    }

    /// An empty discovery (no plugins found at all) must not be an error -
    /// same "not fatal" convention `PluginRegistry` documents for the same
    /// case - it is simply a walk that finds nothing to spawn.
    #[test]
    fn an_empty_discovery_walks_nothing_and_is_not_an_error() {
        let project = tempfile::tempdir().unwrap();
        let conn = setup_conn();
        let discovered = DiscoveredPlugins::default();

        let summary =
            run(project.path(), &conn, None, &discovered).expect("an empty discovery must not fail");

        assert_eq!(summary, BulkIndexSummary::default());
        assert_eq!(count(&conn, "nodes"), 0);
    }
}
