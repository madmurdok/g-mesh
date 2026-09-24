//! Cold-start bulk index: the one full walk of a project that gives a
//! never-indexed codebase a populated graph before its daemon answers
//! anything.
//!
//! The file watcher can only report changes that happen while it is running -
//! `notify` synthesizes nothing for a tree that already exists - so without
//! this, a freshly cloned project stays invisible to every tool until each of
//! its files happens to be edited.
//!
//! Each discovered language's plugin is spawned a *second* time for this, in
//! its one-shot `--bulk-index` mode, rather than asked over the long-lived
//! control-plane pipe (`daemon::plugin::PluginProcess`/`daemon::registry
//! ::PluginRegistry`). A whole-project walk is an open-ended stream of nodes
//! and edges, not one request/response frame: given a process of its own, its
//! stdout is precisely the self-contained, EOF-terminated NDJSON stream
//! `protocol::ndjson::NdjsonReader` was written to consume - nothing
//! interleaved with `FileChanged` traffic, and no end-of-bulk marker to
//! invent. A one-shot process per language, not one process asked to walk
//! every language, because that is exactly what each manifest's `command`/
//! `args` already spawn on the interactive path - see `daemon::manifest`'s
//! `PluginManifest` - and a language with zero files in the project still
//! costs a spawned one-shot process, same as it does today for the bundled
//! JS/TS plugin: teaching core to pre-scan file extensions before walking so
//! it could skip an absent language is out of scope here.
//!
//! [`run`] takes [`DiscoveredPlugins`] rather than a live
//! `daemon::registry::PluginRegistry`: this walk needs only the resolved
//! `command`/`args` discovery already produced, never a long-lived,
//! lazily-spawned supervisor - the registry's whole reason to exist. Every
//! language is walked unconditionally and in one pass, not spawned on first
//! touch, so threading the registry's lazy-spawn machinery through here would
//! add a concept this code has no use for.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use crate::daemon::indexing_status::IndexingStatus;
use crate::daemon::manifest::{DiscoveredPlugins, PluginManifest};
use crate::daemon::plugin;
use crate::embedding::EmbeddingPipeline;
use crate::graph::{imports, symbol_links};
use crate::protocol::ndjson::{BulkItem, NdjsonReader};
use crate::storage::schema;
use crate::storage::write::{apply_diff, Diff};
use crate::watcher::apply::{to_edge_record, to_node_record};

/// Puts the plugin in one-shot bulk-index mode; must stay in sync with
/// `BULK_INDEX_FLAG` in plugins/typescript/src/index.ts.
pub(crate) const BULK_INDEX_FLAG: &str = "--bulk-index";

/// Nodes plus edges accumulated before a batch is committed. One `Diff` for
/// the whole project would mean holding a large repo's entire graph in memory
/// before a single row is written; one per item would mean a transaction per
/// row. A few thousand keeps both bounded without tuning.
const BATCH_ITEMS: usize = 2_000;

/// Holds a finished walk open for this many milliseconds before [`run`]
/// returns, so the daemon has not yet recorded the walk as complete.
///
/// Real installs never set it. It exists so the test suite can observe the
/// window in which the socket is bound and the index is not yet complete
/// (task 105) without needing a repository large enough to take seconds to
/// walk, and without a test that has to burn ten real seconds outlasting
/// `shim::BOOTSTRAP_TIMEOUT`. Same rationale as
/// [`plugin::PLUGIN_PATH_ENV`](crate::daemon::plugin::PLUGIN_PATH_ENV): an
/// explicit, documented override beats a test that has to fake the whole
/// subsystem to control one property of it.
///
/// Deliberately applied *after* everything is committed rather than before,
/// which is what makes the window useful rather than merely long: a test can
/// wait for the row it is about to ask for to appear in the database and only
/// then ask, so a "still indexing" answer proves the flag is what gates the
/// response - not an empty table, which would have produced a refusal-shaped
/// answer (`no symbol named ... found`) all by itself.
pub const WALK_DELAY_ENV: &str = "G_MESH_BULK_INDEX_DELAY_MS";

/// Path whose *deletion* releases the finished walk, for the tests that need
/// the moment of completion to be an event they cause rather than a duration
/// they hope for.
///
/// [`WALK_DELAY_ENV`] turns "the walk is about to finish" into a knob, and for
/// most tests that is enough. It was not enough for the grace-window
/// assertions GM-245 fixed and GM-394 later removed entirely (`mcp::mod::
/// GMeshMcpServer::still_indexing` no longer gives a call a bounded wait
/// before refusing - it waits, unconditionally, for the walk to actually
/// finish - see that method's own doc comment), because the knob is a `sleep`
/// in the *daemon*: a loaded machine can stretch a 400ms hold to 700, which
/// only mattered when a test's whole point was landing inside a window
/// narrower than that slop. It is kept for the property that outlives that
/// history: a test can create the file, let the walk reach it, dispatch its
/// own call, and only then delete the file - so completion happens *after*
/// the call is already in flight by construction, not by arithmetic on two
/// sleeps. See [`HOLD_LOCK_FILE_ENV`] for the sibling knob GM-394 added to
/// hold the batch-commit *lock* itself open the same way, for assertions
/// about that lock specifically rather than about the walk's completion.
pub const WALK_HOLD_FILE_ENV: &str = "G_MESH_BULK_INDEX_HOLD_FILE";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BulkIndexSummary {
    pub nodes: usize,
    pub edges: usize,
    /// Lines that parsed as neither a node nor an edge. Skipped rather than
    /// fatal, matching `NdjsonReader`'s contract: one unreadable line costs
    /// one symbol, while refusing the whole walk over it costs the project
    /// its entire index.
    pub skipped_lines: usize,
    /// `IMPORTS` edges the post-walk linking pass repointed from a module
    /// placeholder onto the real file it names (`graph::imports`).
    pub linked_imports: usize,
    /// `CALLS`/`REFERENCES`/`SUPERTYPE_OF` edges the post-walk linking pass
    /// repointed from a pending-symbol placeholder onto the symbol another
    /// file exports (`graph::symbol_links`).
    pub linked_symbols: usize,
}

/// Walks `project_root` through every plugin `discovered` names - one
/// one-shot `--bulk-index` process per language, run one after another - and
/// commits everything each of them emits, returning only once every child has
/// exited and the last batch is durable. `daemon::run` turns that return into
/// the moment its `IndexingStatus` flips and tools start answering for real,
/// so "returned" has to mean "complete", not "nearly".
///
/// Batching is safe to cut anywhere in one language's stream even though
/// edges are foreign keys onto nodes: a plugin emits a file's nodes before
/// that same file's edges, and never an edge between files (see the
/// dangling-edge guard in extract.ts), so an edge's endpoints are always
/// committed by an earlier batch or its own. Every cross-file edge appears
/// only afterwards, once every language's ingest loop is done, when
/// `graph::imports` links the walk's resolved module placeholders and
/// `graph::symbol_links` its pending-symbol ones - by then every node from
/// every language either exists or never will, which is precisely why those
/// steps cannot be folded into any one language's stream, and why they run
/// exactly once, project-wide, rather than once per language.
pub fn run(
    project_root: &Path,
    conn: &Mutex<Connection>,
    embedding: Option<&EmbeddingPipeline>,
    discovered: &DiscoveredPlugins,
) -> Result<BulkIndexSummary> {
    run_with_progress(project_root, conn, embedding, discovered, None)
}

/// [`run`], also reporting how far it has got through `progress`'s walk
/// counters - languages done out of total, the language being walked, and
/// nodes plus edges ingested so far (GM-395 D6). Only the daemon's
/// activation passes `Some`: a waiting tool call renders those counters into
/// its progress notifications. The CLI's in-process walks have nobody to
/// report to and call [`run`].
pub fn run_with_progress(
    project_root: &Path,
    conn: &Mutex<Connection>,
    embedding: Option<&EmbeddingPipeline>,
    discovered: &DiscoveredPlugins,
    progress: Option<&IndexingStatus>,
) -> Result<BulkIndexSummary> {
    let mut summary = BulkIndexSummary::default();

    // Sorted so a run over N languages walks them - and, if one fails, names
    // which - in a deterministic order. Ingestion order does not change the
    // result (see the doc comment above), but a nondeterministic spawn order
    // would make a real failure impossible to reproduce from one run to the
    // next.
    let mut manifests: Vec<&PluginManifest> = discovered.manifests.values().collect();
    manifests.sort_by(|a, b| a.language.cmp(&b.language));

    // Decided, not inherited: one language's `walk_one_language` failure
    // (`?`, not a collected-and-continued error) fails this whole walk,
    // even for a project that contains not one file of that language -
    // every *discovered* plugin is walked unconditionally regardless of
    // what the project contains (see this module's own doc comment above
    // [`run`]), so a checkout with an unbuilt Python plugin cannot index a
    // pure-Go project either. GM-316 traced the failure this produces
    // (`missing_plugin_binary_hint` fixes the message; this comment is
    // about whether the failure itself is right) and chose to keep it,
    // for the same reason `daemon::mod::run` already gives for treating
    // this whole call as fatal: an index that silently skipped a language
    // and kept going would look exactly like a complete one to every
    // caller downstream - `find_references`, `find_definition`, every MCP
    // tool - which has no way to tell "this symbol truly does not exist"
    // from "the plugin that would have found it never got to run". That is
    // the shape GM-292 cost a whole release to notice: a failure that kept
    // answering, wrongly, is worse than one that stops and says why,
    // because a wrong answer is trusted right up until someone happens to
    // check it by hand, and a missing one is not trusted by construction.
    // Downgrading this to "skip the language that failed, index the rest"
    // would need `bulk_index` to pre-scan the project's file extensions
    // before walking so a language genuinely absent from the project could
    // be told apart from one merely unbuilt - explicitly out of scope per
    // this module's own doc comment - and even then would still let a
    // project that *does* contain Python serve a graph missing it while
    // reporting itself indexed. Neither risk is worth the partial index.
    //
    // What this does mean for a user with no Python in their project: the
    // cost is a one-time `cargo build --workspace`, not an ongoing tax for
    // not using Python. Discovery finds every bundled plugin unconditionally
    // (`daemon::manifest::discover`), so a dev checkout has always needed
    // every bundled plugin's toolchain available before the first index -
    // Go on `$PATH`, `npm ci && npm run build` for TypeScript - the same
    // requirement this repo's own build docs list as a one-time setup step,
    // never as a per-project opt-in. Python and Rust joining the cargo
    // workspace (GM-303) only moved their share of that one-time cost onto
    // `cargo build --workspace`, which a contributor already runs to get
    // `g-mesh` itself in the common case (`cargo build` with no `-p` at the
    // workspace root builds every member; there is no default-member
    // override in this workspace's `Cargo.toml`). The gap this task actually
    // found is narrower than "no Python": it is a *scoped* build
    // (`cargo build -p g-mesh`, or `cargo test -p g-mesh --test <name>`,
    // exactly the shape a contributor reaches for while iterating on `core`
    // alone) that built the daemon without its sibling workspace binaries.
    // The fixed message names the exact command that closes that gap; this
    // paragraph is the argument for why the daemon still refuses to start
    // in the meantime rather than starting without Python. And a released
    // binary never sees this at all - `plugins/python/plugin.toml`'s own
    // header notes its `command` is a dev-checkout path; an installed
    // archive gets a manifest whose `command` names a staged binary that
    // shipped with it (`scripts/bundle-rust-plugin.sh` and its
    // not-yet-written Python counterpart), so a real end user with no
    // interest in Python never has a `target/debug/` path to be missing.
    if let Some(progress) = progress {
        progress.start_walk_progress(u32::try_from(manifests.len()).unwrap_or(u32::MAX));
    }
    for manifest in manifests {
        if let Some(progress) = progress {
            progress.mark_language_started(&manifest.language);
        }
        walk_one_language(project_root, manifest, conn, &mut summary, embedding, progress)?;
        if let Some(progress) = progress {
            progress.mark_language_done();
        }
    }

    // Only now, with every language's stream over: an import can only be
    // linked to a file that is already a node, and a cross-file symbol usage
    // can only be linked once every language that might define it has had its
    // own chance to run - so this runs once, project-wide, after every
    // language's ingest loop, not per language.
    {
        let mut conn = conn.lock().unwrap();
        summary.linked_imports =
            imports::link_all(&mut conn).context("failed to link the walk's resolved imports")?.linked_edges;
        summary.linked_symbols = symbol_links::link_all(&mut conn)
            .context("failed to link the walk's cross-file symbol usages")?
            .linked_edges;
    }

    hold_the_walk_open_for_tests();
    Ok(summary)
}

/// Spawns `manifest`'s plugin in its one-shot `--bulk-index` mode for
/// `project_root` and folds everything it emits into `summary`, returning
/// only once the child has exited and every batch it produced is durable.
/// Split out of [`run`] so each language's spawn/ingest/wait cycle is
/// independently readable, and so a failure partway through one language's
/// walk (an unreadable line, a spawn failure, a nonzero exit) can name that
/// language directly.
///
/// `pub(crate)` rather than private since GM-272: `daemon::workspace_reindex`
/// reuses this exact function, unchanged, to re-walk a *single* language
/// after its rows were deleted - the same one-shot `--bulk-index` process
/// this module already spawns for the cold-start walk, just invoked for one
/// manifest instead of iterated over every discovered one. Nothing about the
/// batching/commit contract above changes for that caller: a per-language
/// reindex is still safe to cut anywhere, for the same reason a cold-start
/// walk is (see this module's own doc comment above [`run`]).
pub(crate) fn walk_one_language(
    project_root: &Path,
    manifest: &PluginManifest,
    conn: &Mutex<Connection>,
    summary: &mut BulkIndexSummary,
    embedding: Option<&EmbeddingPipeline>,
    progress: Option<&IndexingStatus>,
) -> Result<()> {
    // Same check `daemon::plugin::PluginState::spawn` makes before spawning
    // the interactive process - see `plugin::missing_plugin_binary_hint`'s
    // doc comment. Without it, a missing `target/debug/g-mesh-plugin-*`
    // binary (an unbuilt cargo-workspace plugin - python, rust) fails
    // `Command::spawn` below with a bare `No such file or directory (os
    // error 2)`, wrapped only in "failed to spawn the {language} plugin's
    // bulk index ({command})" - naming neither cargo nor the fact that this
    // is a build output at all. That is the exact message traced while
    // verifying GM-301 (GM-316): the daemon's cold-start walk failing this
    // way names every test waiting on the plugin it starves, not the plugin
    // that was never built.
    if let Some(hint) = plugin::missing_plugin_binary_hint(&manifest.command, &manifest.args) {
        bail!("failed to spawn the {} plugin's bulk index: {hint}", manifest.language);
    }

    let mut child = Command::new(&manifest.command)
        .args(&manifest.args)
        .arg(BULK_INDEX_FLAG)
        .arg(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Same reasoning as PluginProcess::spawn: plugin logs are diagnostic
        // only, so they go wherever the daemon's own stderr goes.
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn the {} plugin's bulk index ({})",
                manifest.language,
                manifest.command.display()
            )
        })?;

    let stdout = child.stdout.take().context("bulk-index plugin process has no stdout")?;

    if let Err(err) = ingest(BufReader::new(stdout), conn, summary, embedding, progress) {
        // Nobody is going to read the rest of this walk: a plugin left
        // writing into a pipe no one drains would otherwise outlive a failure
        // it knows nothing about.
        let _ = child.kill();
        let _ = child.wait();
        return Err(err);
    }

    let status = child.wait().context("failed to wait for the bulk-index plugin process")?;
    if !status.success() {
        bail!("the {} plugin's bulk index exited with {status}", manifest.language);
    }

    // This language's own half of `record_bulk_index`'s project-wide roll-up
    // (`storage::schema`'s own doc comment on the two functions has the full
    // reasoning): `walk_one_language` is the one place that reliably knows
    // *which* language just finished its walk, so it records that language's
    // `language_state.bulkIndexedAt` itself, right here, rather than leaving
    // it to `run`'s caller - which only ever asks for the roll-up as a whole,
    // once, after every language in this loop is done.
    //
    // `plugin::fingerprint(manifest)` is "readily available at the write
    // site" in exactly the sense this task scopes populating
    // `pluginFingerprint` to: `manifest` is already in hand here, and
    // `daemon::registry::indexer_version` already computes the very same
    // digest over every discovered plugin at daemon startup - so recomputing
    // it for this one language costs nothing this walk was not already going
    // to pay for elsewhere in spirit, and it is the one write site this
    // column has today (see `language_state`'s own DDL comment on who else,
    // if anyone, would fill it).
    schema::record_language_bulk_indexed(
        &conn.lock().unwrap(),
        &manifest.language,
        Some(&plugin::fingerprint(manifest)),
    )
    .with_context(|| format!("failed to record that {} was bulk-indexed", manifest.language))?;

    Ok(())
}

/// Honors [`WALK_DELAY_ENV`]. A no-op unless it is set to a number, which is
/// every real run.
fn hold_the_walk_open_for_tests() {
    // The file gate first: a test that uses it wants the release to be its own
    // action, and a stray delay on top would only blur that.
    if let Some(path) = std::env::var_os(WALK_HOLD_FILE_ENV).filter(|p| !p.is_empty()) {
        let path = std::path::PathBuf::from(path);
        eprintln!(
            "g-mesh daemon: holding the finished bulk walk until {} is removed ({WALK_HOLD_FILE_ENV})",
            path.display()
        );
        // Polled rather than watched: this is test-only scaffolding, the wait
        // is milliseconds, and a filesystem watcher here would be a second
        // mechanism to get wrong. Bounded so a test that forgets to release it
        // fails as a test timeout rather than wedging the daemon forever.
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

/// Reads one language's whole NDJSON bulk stream to EOF, committing it in
/// batches. Split out from [`walk_one_language`] so every way of failing part
/// way through has one place to clean up after the child process - and so the
/// ingestion rules can be tested without a plugin on the other end.
///
/// Adds only to `nodes`/`edges`/`skipped_lines` - never `linked_imports`/
/// `linked_symbols`, which [`run`] computes once, project-wide, after every
/// language's ingest loop like this one has run, not per language (see
/// [`run`]'s doc comment for why).
pub(crate) fn ingest<R: BufRead>(
    reader: R,
    conn: &Mutex<Connection>,
    summary: &mut BulkIndexSummary,
    embedding: Option<&EmbeddingPipeline>,
    progress: Option<&IndexingStatus>,
) -> Result<()> {
    let mut batch = Diff::default();
    let mut batched = 0usize;

    for item in NdjsonReader::new(reader) {
        match item {
            Ok(BulkItem::Node(node)) => {
                batch.upsert_nodes.push(to_node_record(*node));
                summary.nodes += 1;
            }
            Ok(BulkItem::Edge(edge)) => {
                batch.upsert_edges.push(to_edge_record(edge));
                summary.edges += 1;
            }
            Err(err) => {
                // A read failure (a broken pipe, say) would be reported again
                // on the very next iteration, so shrugging it off the way a
                // malformed line is shrugged off would spin forever - the
                // stream is over, whatever the line count says.
                if err.downcast_ref::<std::io::Error>().is_some() {
                    return Err(err).context("failed to read the plugin's bulk-index stream");
                }
                eprintln!("g-mesh daemon: skipping malformed bulk-index line: {err:#}");
                summary.skipped_lines += 1;
                continue;
            }
        }

        if let Some(progress) = progress {
            progress.add_items_ingested(1);
        }
        batched += 1;
        if batched >= BATCH_ITEMS {
            commit(conn, &mut batch, embedding)?;
            batched = 0;
        }
    }

    commit(conn, &mut batch, embedding)?;
    Ok(())
}

/// Commits one batch and empties it, holding the connection only for as long
/// as the transaction (and the embedding rows it stores) take - the walk
/// itself must not keep other readers out.
///
/// # GM-394: embedding inference runs before the lock is taken
///
/// This used to call `EmbeddingPipeline::apply` - inference and storage in
/// one step - while still holding `conn`'s guard, which meant a batch's whole
/// embedding step (`EmbeddingModel::embed`, an ONNX forward pass per
/// embeddable node, plus a one-time synchronous model load on its very first
/// call - see `embedding::pipeline`'s "Where the model lives" section) ran
/// with every other connection locked out of the daemon's one SQLite handle.
/// `mcp::mod::GMeshMcpServer::get_info` is one of them: it is called during
/// MCP `initialize`, so a client's handshake blocked for as long as that
/// inference took - minutes, on a project big enough to matter, which is
/// exactly the hang GM-394 traced.
///
/// `EmbeddingPipeline::compute` touches no database at all, so it runs here
/// first, with no lock held - the fix's contained half; `get_info` no longer
/// taking this lock while indexing at all (`daemon::indexing_status`'s own
/// "GM-394" doc section) is the other, and the one that actually closes the
/// bug regardless of how long this function's own lock-free window turns out
/// to be. The lock is then taken once, for `apply_diff` and
/// `EmbeddingPipeline::store` together - both are ordinary SQLite writes, not
/// inference, so there is nothing left inside it that scales with batch size
/// the way inference did.
///
/// # GM-395: `embedding: None` for the cold-start walk
///
/// Since GM-395's slice 1, the cold-start walk (`run`'s only caller through
/// `daemon::mod::run`, `cli::init`, `cli::reindex`) passes `None` here: the
/// walk is structural-only now, and embedding moved out to its own pass
/// (`embedding::backfill::run`), run once, project-wide, after every
/// language's walk is linked - see that module's own doc comment. `None`
/// skips both `compute` and `store` outright, which is strictly cheaper than
/// calling them against a disabled pipeline (no model lookup, no per-node
/// loop over an empty `Vec`). `daemon::workspace_reindex`'s per-language
/// re-walk is the one caller that still passes `Some` - it is a full re-walk
/// of one language, not the initial cold start, so its own embeddings still
/// belong inline with it rather than waiting for the next backfill pass.
fn commit(conn: &Mutex<Connection>, batch: &mut Diff, embedding: Option<&EmbeddingPipeline>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let computed = embedding.map(|embedding| embedding.compute(batch)).unwrap_or_default();
    let mut conn = conn.lock().unwrap();
    apply_diff(&mut conn, batch).context("failed to commit a bulk-index batch")?;
    hold_the_lock_open_for_tests();
    // Best-effort, like every other embedding call - see
    // `watcher::apply::round_trip`'s identical handling for why a failure
    // here must not undo (or fail) a batch that is already durable.
    if let Some(embedding) = embedding {
        embedding.store(&conn, &computed);
    }
    *batch = Diff::default();
    Ok(())
}

/// Path whose *deletion* releases a batch commit that is holding `conn`'s
/// lock open, for tests that need to prove something behaves correctly while
/// that lock is actually held - not merely while `daemon::indexing_status::
/// IndexingStatus` reads as indexing, which [`WALK_HOLD_FILE_ENV`] already
/// controls without ever touching the lock at all (that hold runs after
/// every batch has committed and released it - see [`run`]'s call to
/// [`hold_the_walk_open_for_tests`]).
///
/// GM-394's own regression needs exactly this distinction. The bug it found
/// was never "the walk takes a while" - `IndexingStatus` already told every
/// caller that, honestly, since task 105 - it was "a batch commit holds the
/// mutex every MCP handler shares for as long as its embedding inference
/// takes". Reproducing that deterministically, on a machine that has not
/// necessarily fetched the real ONNX weights `EmbeddingPipeline` would
/// otherwise need, means holding the *lock* open on purpose, independent of
/// whatever this build's embedding pipeline does - which is what this knob
/// is for: a no-op unless set, and when set, held from directly inside the
/// locked section of [`commit`] until the named file is removed.
pub const HOLD_LOCK_FILE_ENV: &str = "G_MESH_BULK_INDEX_HOLD_LOCK_FILE";

/// Honors [`HOLD_LOCK_FILE_ENV`]. A no-op unless it is set, which is every
/// real run - same shape as [`hold_the_walk_open_for_tests`], polled rather
/// than watched for the identical reason (test-only scaffolding, a
/// millisecond-scale wait, bounded so a test that forgets to release it fails
/// as a timeout rather than wedging the daemon forever).
fn hold_the_lock_open_for_tests() {
    let Some(path) = std::env::var_os(HOLD_LOCK_FILE_ENV).filter(|p| !p.is_empty()) else { return };
    let path = std::path::PathBuf::from(path);
    eprintln!(
        "g-mesh daemon: holding a bulk-index batch's lock open until {} is removed ({HOLD_LOCK_FILE_ENV})",
        path.display()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{
        EdgeKind, NodeKind, Position, Range, SourceTier, Visibility, WireEdge, WireNode,
    };
    use crate::storage::schema;
    use std::io::Cursor;

    fn setup_conn() -> Mutex<Connection> {
        let conn = Connection::open_in_memory().unwrap();
        // Foreign keys on, unlike the daemon's own connection: the point of
        // several of these tests is that batching never presents SQLite with
        // an edge whose endpoints aren't in yet, and that only bites when the
        // constraint is actually enforced.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        Mutex::new(conn)
    }

    fn count(conn: &Mutex<Connection>, table: &str) -> i64 {
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

    fn ingest_str(stream: &str, conn: &Mutex<Connection>) -> Result<BulkIndexSummary> {
        let mut summary = BulkIndexSummary::default();
        ingest(Cursor::new(stream.as_bytes().to_vec()), conn, &mut summary, None, None)?;
        Ok(summary)
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

    /// Task 156's acceptance criterion: discovering two languages must spawn
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

    /// A discovery naming only one language - today's real-world shape,
    /// before any second plugin exists - must still walk it: the loop over
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
