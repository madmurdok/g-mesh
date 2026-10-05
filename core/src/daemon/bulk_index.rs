//! Cold-start bulk index: the one full walk of a project that gives a
//! never-indexed codebase a populated graph before its daemon answers
//! anything. The file watcher cannot do this: `notify` reports only changes
//! made while it runs.
//!
//! Each discovered language's plugin is spawned in its one-shot
//! `--bulk-index` mode, one process per language, from the manifest's own
//! `command`/`args`. Its stdout is the EOF-terminated NDJSON stream
//! `protocol::ndjson::NdjsonReader` consumes. Why a second process rather
//! than the control-plane pipe: [ADR 0002](../../../docs/adr/0002-bulk-walk.md).
//! What one failed language costs: [ADR 0021](../../../docs/adr/0021-per-language-bulk-outcome.md).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

use crate::daemon::indexing_status::IndexingStatus;
use crate::daemon::manifest::{DiscoveredPlugins, PluginManifest};
use crate::daemon::plugin;
use crate::embedding::{EmbedStats, EmbeddingPipeline};
use crate::languages::{self, LanguageOutcome};
use crate::protocol::ndjson::{BulkItem, NdjsonReader};
use crate::storage::file_rows::FileScope;
use crate::storage::index_store::{IndexStore, Unit, Writer};
use crate::storage::schema;
use crate::storage::write::Diff;
use crate::watcher::apply::{to_edge_record, to_node_record, PathWarnings};
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

/// Test-only: set to any non-empty value to switch off the count of files
/// belonging to absent plugins ([`languages::count_absent_files`]), so a
/// measurement can compare the walk with and without it. With it set, no
/// language gets a `PluginAbsent` outcome. Real installs never set it.
pub const ABSENT_COUNT_OFF_ENV: &str = "G_MESH_BULK_INDEX_NO_ABSENT_COUNT";

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
    /// What the walk did for each discovered language, keyed by language.
    /// The counts above cover only the languages that ended `Indexed`.
    pub outcomes: BTreeMap<String, LanguageOutcome>,
}

/// Everything one walk carries from language to language and batch to batch:
/// the store it writes, what it reports to, and what it accumulates.
pub(crate) struct WalkContext<'a> {
    pub(crate) store: &'a IndexStore,
    /// `None` for the structural-only cold-start walk (`embedding::backfill`
    /// fills the vectors afterwards); `Some` for
    /// `daemon::workspace_reindex`'s per-language re-walk.
    pub(crate) embedding: Option<&'a EmbeddingPipeline>,
    /// What `embedding` did across every batch of this walk, for the caller
    /// to report with `EmbeddingPipeline::finish_unit`.
    pub(crate) embed_stats: EmbedStats,
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
            embed_stats: EmbedStats::default(),
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
    let walk_clock = std::time::Instant::now();
    let mut ctx =
        WalkContext { embedding, progress, walked_files: Some(BTreeSet::new()), ..WalkContext::new(conn) };
    let mut failed: BTreeMap<String, String> = BTreeMap::new();
    // The absent plugins' file count walks the tree on its own thread, from
    // before the first plugin is spawned until after the last one finished,
    // so it costs time only when it outlasts every plugin's walk (ADR 0021).
    // It touches no database. Nothing to count when every catalogue plugin is
    // installed.
    let count_absent = !languages::missing(discovered).is_empty() && !absent_count_switched_off();
    let absent_files = std::thread::scope(|scope| -> Result<BTreeMap<&'static str, usize>> {
        let counting =
            count_absent.then(|| scope.spawn(|| languages::count_absent_files(project_root, discovered)));
        for manifest in &manifests {
            if let Some(progress) = progress {
                progress.mark_language_started(&manifest.language);
            }
            // A language is wholly in the index or not at all, and the walk fails
            // only when every discovered language failed (ADR 0021, which answers
            // ADR 0002's partial-index objection). A failed language's rows are
            // purged here, before `link_all`, so no cross-file edge reaches them.
            let counts_before = (ctx.summary.nodes, ctx.summary.edges, ctx.summary.skipped_lines);
            let walked_before = ctx.walked_files.clone();
            if let Err(err) = walk_one_language(project_root, manifest, &mut ctx) {
                purge_language(conn, &manifest.language).with_context(|| {
                    format!("failed to remove the partly walked {} rows after: {err:#}", manifest.language)
                })?;
                (ctx.summary.nodes, ctx.summary.edges, ctx.summary.skipped_lines) = counts_before;
                ctx.walked_files = walked_before;
                failed.insert(manifest.language.clone(), languages::failed_error(&err));
            }
            if let Some(progress) = progress {
                progress.mark_language_done();
            }
        }
        Ok(match counting.map(|handle| handle.join()) {
            None => BTreeMap::new(),
            Some(Ok(counts)) => counts,
            // A count is a courtesy: losing it costs the PluginAbsent lines,
            // never the walk.
            Some(Err(_)) => {
                eprintln!(
                    "g-mesh: counting the files of languages with no plugin installed panicked - skipped"
                );
                BTreeMap::new()
            }
        })
    })?;
    let WalkContext { mut summary, walked_files, embed_stats, .. } = ctx;
    let walked_files = walked_files.unwrap_or_default();
    if let Some(embedding) = embedding {
        embedding.finish_unit("bulk-walk", &embed_stats, walk_clock.elapsed());
    }

    let file_counts = conn.with(file_counts_by_language)?;
    summary.outcomes = discovered
        .manifests
        .values()
        .map(|manifest| {
            let language = manifest.language.clone();
            let outcome = match failed.remove(&language) {
                Some(error) => LanguageOutcome::Failed { error },
                None => LanguageOutcome::Indexed { files: file_counts.get(&language).copied().unwrap_or(0) },
            };
            (language, outcome)
        })
        .collect();
    for (language, files) in absent_files {
        if files > 0 {
            summary
                .outcomes
                .insert(language.to_string(), LanguageOutcome::PluginAbsent { files: Some(files) });
        }
    }
    conn.with(|conn| schema::record_language_outcomes(conn, &summary.outcomes))
        .context("failed to record the per-language outcomes of the walk")?;
    // Over the discovered languages only: an absent plugin is not a failure.
    let all_failed = !discovered.manifests.is_empty()
        && discovered
            .manifests
            .keys()
            .all(|language| matches!(summary.outcomes.get(language), Some(LanguageOutcome::Failed { .. })));
    if all_failed {
        let mut message = String::from("every discovered language failed to index:");
        for (language, outcome) in &summary.outcomes {
            if let LanguageOutcome::Failed { error } = outcome {
                message.push_str(&format!("\n  {language}: {}", languages::error_on_one_line(error)));
            }
        }
        bail!(message);
    }

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

/// Removes every row `language` has in the index, through the watcher's own
/// path for a deleted file, one file at a time. Containers (`filePath = ''`)
/// are not files: they go with their last member.
fn purge_language(conn: &IndexStore, language: &str) -> Result<()> {
    let paths: Vec<String> = conn.with(|conn| -> Result<Vec<String>> {
        let mut stmt = conn
            .prepare("SELECT DISTINCT filePath FROM nodes WHERE language = ?1 AND filePath <> '' ORDER BY filePath")
            .context("failed to prepare the language's file query")?;
        let rows = stmt
            .query_map([language], |row| row.get(0))
            .context("failed to query the language's files")?
            .collect::<rusqlite::Result<_>>()
            .context("failed to read the language's files")?;
        Ok(rows)
    })?;
    conn.unit(Unit::BulkWalk, |store| {
        for path in &paths {
            let mut diff = Diff::default();
            store.apply_file_diff_linked(&mut diff, path, FileScope::Gone, "purge a failed language")?;
        }
        Ok(())
    })
}

/// Each language's `File`-node count.
fn file_counts_by_language(conn: &rusqlite::Connection) -> Result<BTreeMap<String, usize>> {
    let mut stmt = conn
        .prepare("SELECT language, COUNT(*) FROM nodes WHERE kind = ?1 GROUP BY language")
        .context("failed to prepare the per-language file count")?;
    let rows = stmt
        .query_map([FILE_NODE_KIND], |row| {
            Ok((row.get::<_, String>(0)?, usize::try_from(row.get::<_, i64>(1)?).unwrap_or(0)))
        })
        .context("failed to count each language's files")?;
    rows.collect::<rusqlite::Result<_>>().context("failed to read each language's file count")
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
    if let Some(hint) = plugin::missing_plugin_binary_hint(&manifest.command) {
        // The hint is the innermost cause, so the instructions show it rather
        // than the step (ADR 0022).
        return Err(
            anyhow!(hint).context(format!("failed to spawn the {} plugin's bulk index", manifest.language))
        );
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

/// Whether [`ABSENT_COUNT_OFF_ENV`] is set to a non-empty value.
fn absent_count_switched_off() -> bool {
    std::env::var_os(ABSENT_COUNT_OFF_ENV).is_some_and(|value| !value.is_empty())
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
    let mut path_warnings = PathWarnings::default();

    for item in NdjsonReader::new(reader) {
        match item {
            Ok(BulkItem::Node(node)) => {
                let record = to_node_record(*node, &mut path_warnings);
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
fn commit(store: &mut Writer<'_>, batch: &mut Diff, ctx: &mut WalkContext<'_>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let embedding = ctx.embedding;
    let computed =
        embedding.map(|embedding| embedding.compute(batch, &mut ctx.embed_stats)).unwrap_or_default();
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
            alias_paths: Vec::new(),
            untyped_calls: Vec::new(),
            qualified_path: None,
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
            BulkIndexSummary {
                nodes: 2,
                edges: 1,
                skipped_lines: 0,
                linked_imports: 0,
                linked_symbols: 0,
                ..BulkIndexSummary::default()
            }
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
            BulkIndexSummary {
                nodes: 2,
                edges: 0,
                skipped_lines: 1,
                linked_imports: 0,
                linked_symbols: 0,
                ..BulkIndexSummary::default()
            }
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

    // -----------------------------------------------------------------
    // Per-language outcome (ADR 0021): one failed language costs only
    // itself, and the walk fails only when every discovered one failed.
    // -----------------------------------------------------------------

    use crate::daemon::test_plugin;
    use crate::languages::LanguageOutcome;

    /// One NDJSON node line in the fake plugins' wire shape.
    fn node_of(id: &str, kind: &str, file_path: &str, language: &str) -> String {
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

    /// One NDJSON `CALLS` edge line.
    fn calls_edge(id: &str, from: &str, to: &str) -> String {
        serde_json::json!({
            "id": id,
            "fromId": from,
            "toId": to,
            "kind": "CALLS",
            "source": "syntactic",
            "engine": "tree-sitter",
            "resolved": true,
        })
        .to_string()
    }

    /// A good language's stream: two files, a function in each, and a call
    /// from the first file's function to the second's.
    fn two_file_stream(language: &str, ext: &str) -> Vec<String> {
        let a = format!("src/a{ext}");
        let b = format!("src/b{ext}");
        vec![
            node_of(&format!("file:{a}"), "File", &a, language),
            node_of(&format!("{language}-f1"), "Function", &a, language),
            node_of(&format!("file:{b}"), "File", &b, language),
            node_of(&format!("{language}-f2"), "Function", &b, language),
            calls_edge(&format!("{language}-call"), &format!("{language}-f1"), &format!("{language}-f2")),
        ]
    }

    /// Writes `rel` under `root` with an mtime an hour back, so the walk's
    /// staleness baseline (which skips files modified just before the walk)
    /// records it and `indexed_files` shows which files the walk kept.
    fn write_old_file(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "x").unwrap();
        let an_hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(an_hour_ago).unwrap();
    }

    fn scalar(conn: &IndexStore, sql: &str) -> i64 {
        conn.lock().unwrap().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn recorded_outcomes(conn: &IndexStore) -> Vec<(String, LanguageOutcome)> {
        schema::language_outcomes(&conn.lock().unwrap()).unwrap()
    }

    /// Removes a fake plugin's entry point: the "binary" of a node-launched
    /// plugin, so its spawn fails the way a never-built plugin's does.
    fn remove_entry_point(plugin_dir: &Path) -> std::path::PathBuf {
        let entry = plugin_dir.join("plugin.js");
        std::fs::remove_file(&entry).unwrap();
        entry
    }

    fn discover_root(plugins: &Path) -> DiscoveredPlugins {
        crate::daemon::manifest::discover(&[plugins.to_path_buf()])
            .expect("the fixture plugins must discover")
    }

    /// The acceptance criterion's core: with one of two plugins' binaries
    /// gone, the walk is `Ok`, the good language is indexed and queryable,
    /// and the other is `Failed` with an error naming what is missing.
    ///
    /// Control: restore the abort-on-first-failure loop in
    /// `run_with_progress` (`walk_one_language(..)?` with no purge/record) -
    /// the walk returns `Err` and the `expect` below fails. Walking `beta`
    /// before `alpha` is not needed: the old loop aborted on `beta` whichever
    /// order, and `alpha`'s rows alone could not make it `Ok`.
    #[test]
    fn a_missing_plugin_binary_costs_only_its_own_language() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        test_plugin::install(plugins.path(), "alpha", &[".alpha-src"]);
        let beta = test_plugin::install(plugins.path(), "beta", &[".beta-src"]);
        let missing = remove_entry_point(&beta);
        test_plugin::set_bulk_stream(project.path(), "alpha", &two_file_stream("alpha", ".alpha-src"), 0);
        let discovered = discover_root(plugins.path());
        let conn = setup_conn();

        let summary = run(project.path(), &conn, None, &discovered)
            .expect("one failed language must not fail the whole walk");

        assert_eq!(summary.outcomes.get("alpha"), Some(&LanguageOutcome::Indexed { files: 2 }));
        match summary.outcomes.get("beta") {
            // The manifest's `./plugin.js` is joined onto its directory as
            // written, so the path is matched as its directory plus file name.
            Some(LanguageOutcome::Failed { error }) => assert!(
                error.contains(&beta.display().to_string())
                    && error.contains("plugin.js")
                    && error.contains("does not exist"),
                "beta's error must name the missing entry point {}: {error}",
                missing.display()
            ),
            other => panic!("beta must be Failed, got {other:?}"),
        }
        assert_eq!(summary.outcomes.len(), 2, "only discovered languages, no absent catalogue ones");
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM nodes WHERE language = 'alpha' AND kind = 'Function'"),
            2,
            "the good language's symbols must be in the index"
        );
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM edges WHERE id = 'alpha-call'"), 1);
        assert_eq!(
            recorded_outcomes(&conn),
            vec![
                ("alpha".to_string(), LanguageOutcome::Indexed { files: 2 }),
                ("beta".to_string(), summary.outcomes["beta"].clone()),
            ],
            "the outcome must be persisted for a later session"
        );
    }

    /// Every discovered plugin missing: `Err` naming each language, and the
    /// outcome rows are still written, so a `Phase::Failed` session can say
    /// why.
    ///
    /// Controls: compute `all_failed` as `false` (or drop the bail) - the
    /// walk returns `Ok` and `expect_err` fails; move the
    /// `record_language_outcomes` call after the bail - no rows, and the
    /// row assertion fails.
    #[test]
    fn every_discovered_plugin_missing_fails_the_walk_and_still_records_each_failure() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        let alpha = test_plugin::install(plugins.path(), "alpha", &[".alpha-src"]);
        let beta = test_plugin::install(plugins.path(), "beta", &[".beta-src"]);
        remove_entry_point(&alpha);
        remove_entry_point(&beta);
        let discovered = discover_root(plugins.path());
        let conn = setup_conn();

        let err = run(project.path(), &conn, None, &discovered)
            .expect_err("a walk where every discovered language failed is an error");
        let message = format!("{err:#}");
        for language in ["alpha", "beta"] {
            assert!(message.contains(language), "the error must name {language}: {message}");
        }

        let rows = recorded_outcomes(&conn);
        assert_eq!(rows.len(), 2, "both failures must be recorded: {rows:?}");
        for (_, outcome) in &rows {
            assert!(matches!(outcome, LanguageOutcome::Failed { .. }), "{rows:?}");
        }
    }

    /// The all-failed error names each language on one line of its own,
    /// with that language's whole stored chain (one cause per line in the
    /// store) joined by ": " - never the stored newlines (GM-330, ADR 0021
    /// section 2).
    ///
    /// Control: push the stored error raw (no `error_on_one_line`) - the
    /// message gains a line per extra cause and the line-count and per-line
    /// assertions fail.
    #[test]
    fn the_all_failed_error_shows_each_whole_chain_on_one_line() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        // A plugin whose spawn command is gone: the OS refuses the spawn, and
        // the stored error is the context plus the OS cause (a missing
        // `plugin.js` entry point is caught earlier, as a single cause).
        for language in ["alpha", "beta"] {
            let dir = plugins.path().join(language);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("plugin.toml"),
                format!(
                    "[plugin]\nlanguage = \"{language}\"\nprotocol_version = {}\n\
                     plugin_version = \"0.0.0-test\"\n\n[plugin.spawn]\ncommand = \"./g-mesh-plugin-{language}\"\n\n\
                     [plugin.languages]\nextensions = [\".{language}-src\"]\n",
                    crate::protocol::types::CURRENT_PROTOCOL_VERSION
                ),
            )
            .unwrap();
        }
        let discovered = discover_root(plugins.path());
        let conn = setup_conn();

        let message = run(project.path(), &conn, None, &discovered)
            .expect_err("a walk where every discovered language failed is an error")
            .to_string();

        let rows = recorded_outcomes(&conn);
        let lines: Vec<&str> = message.lines().collect();
        assert_eq!(lines.len(), 1 + rows.len(), "a header and one line per language: {message}");
        for ((language, outcome), line) in rows.iter().zip(&lines[1..]) {
            let LanguageOutcome::Failed { error } = outcome else { panic!("{rows:?}") };
            let causes: Vec<&str> = error.lines().collect();
            assert!(causes.len() >= 2, "the fixture must store a multi-line chain: {error}");
            assert_eq!(*line, format!("  {language}: {}", causes.join(": ")), "{message}");
        }
    }

    /// An absent catalogue language never rescues an all-failed walk: the
    /// all-failed test is over discovered languages only.
    ///
    /// Control: compute `all_failed` over every outcome
    /// (`summary.outcomes.values().all(Failed)`) - python's `PluginAbsent`
    /// makes it false and the walk returns `Ok`.
    #[test]
    fn an_absent_language_with_files_does_not_prevent_the_all_failed_error() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        let alpha = test_plugin::install(plugins.path(), "alpha", &[".alpha-src"]);
        remove_entry_point(&alpha);
        write_old_file(project.path(), "app.py");
        let discovered = discover_root(plugins.path());
        let conn = setup_conn();

        run(project.path(), &conn, None, &discovered)
            .expect_err("every discovered language failed; python having no plugin changes nothing");
        let rows = recorded_outcomes(&conn);
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert!(
            matches!(&rows[0], (language, LanguageOutcome::Failed { .. }) if language == "alpha"),
            "{rows:?}"
        );
        assert_eq!(rows[1], ("python".to_string(), LanguageOutcome::PluginAbsent { files: Some(1) }));
    }

    /// No plugin discovered at all, `.py` files present: `Ok`, an empty
    /// graph, and Python's file count as a `PluginAbsent` outcome, persisted.
    ///
    /// Control: skip the count (`count_absent = false`) - no outcome, and the
    /// equality fails.
    #[test]
    fn zero_discovered_plugins_with_python_files_is_ok_with_python_absent_and_counted() {
        let project = tempfile::tempdir().unwrap();
        write_old_file(project.path(), "app.py");
        write_old_file(project.path(), "pkg/mod.pyi");
        let conn = setup_conn();

        let summary = run(project.path(), &conn, None, &DiscoveredPlugins::default())
            .expect("zero discovered plugins is not an error");

        assert_eq!(count(&conn, "nodes"), 0);
        let expected =
            BTreeMap::from([("python".to_string(), LanguageOutcome::PluginAbsent { files: Some(2) })]);
        assert_eq!(summary.outcomes, expected);
        assert_eq!(recorded_outcomes(&conn), expected.into_iter().collect::<Vec<_>>());
    }

    /// No plugin discovered and no file any catalogue language claims: no
    /// outcome at all - a catalogue language with no files is not
    /// `PluginAbsent`.
    ///
    /// Control: insert `PluginAbsent { files: None }` for every
    /// `languages::missing` entry rather than only counted ones - four rows
    /// appear.
    #[test]
    fn zero_discovered_plugins_and_no_catalogue_files_records_no_outcome() {
        let project = tempfile::tempdir().unwrap();
        write_old_file(project.path(), "README.md");
        let conn = setup_conn();

        let summary = run(project.path(), &conn, None, &DiscoveredPlugins::default()).unwrap();

        assert!(summary.outcomes.is_empty(), "{:?}", summary.outcomes);
        assert!(recorded_outcomes(&conn).is_empty());
    }

    /// A plugin that streams part of its language and then exits non-zero
    /// leaves nothing of that language behind: no node, no `indexed_files`
    /// baseline, not counted in the summary; the walk completes, the
    /// project-wide `bulkIndexedAt` roll-up fires (the failed language is
    /// not "present"), and the good language's cross-file call is intact.
    ///
    /// Controls: drop the `purge_language` call - beta's nodes remain and
    /// the roll-up stays unset (beta is present without its own
    /// `bulkIndexedAt`); drop `ctx.walked_files = walked_before` - beta's
    /// files get `indexed_files` rows; drop the summary-count restore -
    /// `summary.nodes` includes beta's.
    #[test]
    fn a_plugin_that_fails_mid_stream_has_its_committed_rows_purged() {
        let project = tempfile::tempdir().unwrap();
        let plugins = tempfile::tempdir().unwrap();
        test_plugin::install(plugins.path(), "alpha", &[".alpha-src"]);
        test_plugin::install(plugins.path(), "beta", &[".beta-src"]);
        for rel in ["src/a.alpha-src", "src/b.alpha-src", "src/a.beta-src", "src/b.beta-src"] {
            write_old_file(project.path(), rel);
        }
        test_plugin::set_bulk_stream(project.path(), "alpha", &two_file_stream("alpha", ".alpha-src"), 0);
        // Enough lines for beta to have committed rows before it fails: more
        // than one batch, so the purge has real rows to remove.
        let mut beta_stream = two_file_stream("beta", ".beta-src");
        for n in 0..(BATCH_ITEMS + 10) {
            beta_stream.push(node_of(&format!("beta-extra-{n}"), "Function", "src/b.beta-src", "beta"));
        }
        test_plugin::set_bulk_stream(project.path(), "beta", &beta_stream, 3);
        let discovered = discover_root(plugins.path());
        let conn = setup_conn();
        schema::ensure_current(&conn.lock().unwrap(), "test-generation").unwrap();

        let summary =
            run(project.path(), &conn, None, &discovered).expect("one failed language is not fatal");

        match summary.outcomes.get("beta") {
            Some(LanguageOutcome::Failed { error }) => {
                assert!(error.contains("exited"), "beta's error must say it exited non-zero: {error}")
            }
            other => panic!("beta must be Failed, got {other:?}"),
        }
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM nodes WHERE language = 'beta'"),
            0,
            "no beta node may remain"
        );
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM nodes WHERE filePath LIKE '%.beta-src'"), 0);
        assert_eq!(scalar(&conn, "SELECT COUNT(*) FROM indexed_files WHERE filePath LIKE '%.beta-src'"), 0);
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM indexed_files WHERE filePath LIKE '%.alpha-src'"),
            2,
            "the good language's files keep their baselines (else the check above proves nothing)"
        );
        assert_eq!(summary.nodes, 4, "the summary counts only the indexed language's nodes");
        assert_eq!(summary.edges, 1);
        assert_eq!(
            scalar(
                &conn,
                "SELECT COUNT(*) FROM edges e JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
                 WHERE e.id = 'alpha-call' AND f.filePath = 'src/a.alpha-src' AND t.filePath = 'src/b.alpha-src'"
            ),
            1,
            "the good language's cross-file call must survive the purge"
        );
        assert_eq!(
            scalar(&conn, "SELECT COUNT(*) FROM edges WHERE fromId NOT IN (SELECT id FROM nodes) OR toId NOT IN (SELECT id FROM nodes)"),
            0,
            "no edge may outlive an endpoint"
        );

        let guard = conn.lock().unwrap();
        schema::record_bulk_index(&guard).unwrap();
        assert!(
            schema::bulk_index_completed(&guard).unwrap(),
            "the walk is complete for every language it could index, so bulkIndexedAt is set"
        );
    }
}
