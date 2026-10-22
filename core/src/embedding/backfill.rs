//! The embedding backfill pass: fills in `vectors` rows for every node the
//! graph is missing one for, as its own bounded scan rather than as part of
//! the walk that wrote those nodes in the first place.
//!
//! # Why this exists
//!
//! Before this, `daemon::bulk_index::commit` computed and stored a batch's
//! embeddings inline, as part of the cold-start walk - so a project's
//! structural graph and its embeddings became complete at the same instant,
//! and neither could be ready before the other. Measured on g-mesh's own
//! repo, that made a cold start take about 814s, when the walk that produces
//! the structural graph a `find_definition`/`find_references`/... caller
//! actually needs takes about 31s of it.
//!
//! `daemon::bulk_index::run` is structural-only, and this module is the
//! "finish the other half" step that runs once, afterward: every node the
//! walk (or an earlier interrupted attempt at this same pass, or an
//! incremental edit whose own embedding step failed) left without a
//! `vectors` row gets one - or is confirmed to have nothing worth embedding -
//! see `embedding::pipeline`'s "What gets embedded" section.
//!
//! `daemon::indexing_status::IndexingStatus`'s `Embedding` phase covers
//! exactly the time this pass runs: structural tools do not wait on it
//! (`Need::Structural` is already satisfied), `search_code` waits on it for
//! at most `mcp::SEARCH_EMBEDDING_WAIT` (`Need::Embeddings`), then answers
//! from the vectors stored so far.
//!
//! # Termination
//!
//! Keyset pagination over `nodes.id`, not "while any unembedded row
//! remains": a node whose inference keeps failing (a pathological input, a
//! persistent ONNX error), whose text trims to nothing, or that
//! `embedding::text::is_noise` filters out (`node_text` returning `None`, so
//! [`EmbeddingPipeline::compute`] skips it) would make a "does anything
//! remain" loop spin forever. A filtered node with no vector, or one from
//! another `embeddingVersion`, stays a candidate and is visited on every pass
//! (counted as [`BackfillSummary::filtered`], not as owed a vector); the first
//! visit deletes a vector it had. Paging strictly
//! past the last id a page saw, ordered by id, guarantees every candidate at
//! the moment this pass started is visited exactly once, however many of
//! them end up unembedded anyway.
//!
//! # No schema or indexer-version bump
//!
//! The graph itself is unchanged by this feature, so an index built before
//! GM-395 needs no reindex - "embeddings complete" is recorded nowhere on
//! disk; this pass's own `COUNT(*)` query *is* the check, and it settles,
//! quickly, at the count of candidates that get no vector (filtered, or
//! text that trims to nothing) once every embeddable node has a row. A side effect
//! worth having: a project whose model was not available at index time (or
//! whose weights simply had not been fetched yet) gets embedded on the next
//! restart that finds one, with no reindex required.
//!
//! # A vector from another model is owed too
//!
//! A node whose `vectors` row carries an `embeddingVersion` other than the
//! pipeline's is a candidate as well, the same rule
//! `storage::language_swap::plan_embeddings` applies to a reindex. That is
//! how a change of pinned weights (`embedding::model::embedding_version`)
//! reaches an existing index: every old row is
//! re-embedded by this pass on the next start, with no reindex and no schema
//! change, and `vectors::insert` overwrites it in place.
//!
//! # Vectors stored before the noise rule
//!
//! A node `embedding::text::is_noise` filters may still hold a vector from
//! before the rule existed, at the current `embeddingVersion` (the rule did
//! not change the text form's tag). Such a node is no candidate above, so the
//! page loop never reaches it. [`purge_noise_vectors`] does, once per index
//! and per rule generation (`embedding::text::NOISE_RULE_VERSION`, recorded
//! in the `embedding_noise_purge` table): it pages through every node that
//! has a vector, deletes the ones the rule filters, and records the
//! generation. It runs ahead of the model check, since deleting needs no
//! model; an interrupted purge records nothing and runs again next start.

use rusqlite::{Connection, Result as SqlResult};

use crate::daemon::indexing_status::IndexingStatus;
use crate::embedding::text::{is_noise, EmbedInput, NOISE_RULE_VERSION};
use crate::embedding::{EmbedStats, EmbeddingPipeline};
use crate::storage::index_store::{IndexStore, Unit};
use crate::storage::write::{Diff, NodeRecord};
use crate::storage::{schema, vectors};

/// Keyset page size. Small enough that one page's inference plus its commit
/// is a reasonable amount of work to redo if the daemon is killed mid-pass -
/// the next start's backfill simply resumes, since nothing here is
/// all-or-nothing the way one bulk-index batch's transaction is - and the
/// same order of magnitude as `daemon::bulk_index::BATCH_ITEMS`.
const PAGE_SIZE: i64 = 256;

/// While the file this names exists, [`run`] holds *before* its first batch -
/// independent of whether a model is actually available (see [`run`]'s own
/// doc comment on why the hold runs ahead of that check), so a phase-gating
/// test needs no real ONNX weights on disk. A no-op unless set, which is
/// every real run - same poll-until-deleted shape as
/// `daemon::bulk_index::WALK_HOLD_FILE_ENV`.
pub const HOLD_FILE_ENV: &str = "G_MESH_EMBED_PASS_HOLD_FILE";

/// While this env var is set to a non-empty value, [`run`] panics before its
/// first batch - test-only, for GM-395 slice 2b's guard that a panic in this
/// pass must not turn the whole activation into
/// [`Phase::Failed`](crate::daemon::indexing_status::Phase::Failed):
/// `daemon::activation::ActivationCtx::activate` wraps this call in
/// `catch_unwind` precisely so that structural tools keep answering even if
/// this pass blows up, and this knob is what lets a test make it blow up
/// without needing a real, reproducible ONNX failure. Checked right after the
/// hold above, before the availability check, for the same reason the hold
/// itself runs early: a test needs this to fire whether or not the machine
/// running it has ever fetched real weights.
pub const PANIC_ENV: &str = "G_MESH_EMBED_PASS_PANIC";

/// What one call to [`run`] actually did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    /// Candidate rows this pass owed the project when it started - not
    /// necessarily how many it ended up embedding (a node can fail inference,
    /// or be deleted mid-pass by a concurrent edit). Includes the
    /// [`filtered`](Self::filtered) ones.
    pub candidates: usize,
    /// Candidates `embedding::text::is_noise` filtered out: they get no
    /// vector by design, so they are not owed one.
    pub filtered: usize,
    /// Vectors the model computed: the embedding cache's misses, as
    /// [`EmbedStats::embedded`] counts them.
    pub embedded: usize,
    /// Vectors the embedding cache served instead. `embedded + cache_hits`
    /// is every vector this pass handed to the store.
    pub cache_hits: usize,
    /// Vectors [`purge_noise_vectors`] deleted before the pass.
    pub purged: usize,
}

impl BackfillSummary {
    /// The candidates owed a vector: [`candidates`](Self::candidates) less
    /// the [`filtered`](Self::filtered) ones.
    pub fn owed_a_vector(&self) -> usize {
        self.candidates.saturating_sub(self.filtered)
    }
}

/// Fills in every `vectors` row this project's graph is currently missing.
///
/// Holds on [`HOLD_FILE_ENV`] first, unconditionally, then runs
/// [`purge_noise_vectors`] (no model needed), then returns - before counting
/// a single candidate - if [`EmbeddingPipeline::is_available`] says there is
/// no model to run at all.
/// The hold has to come before that check, not after: a phase-gating test
/// needs to observe the daemon sitting in
/// [`Phase::Embedding`](crate::daemon::indexing_status::Phase::Embedding)
/// regardless of whether the machine running it has ever fetched real
/// weights, and an early return for "no model" would make the hold
/// unreachable on exactly the machines (CI, a fresh checkout) that need it
/// most.
pub fn run(store: &IndexStore, embedding: &EmbeddingPipeline, progress: &IndexingStatus) -> BackfillSummary {
    hold_before_first_batch_for_tests();
    panic_before_first_batch_for_tests();

    let purged = purge_noise_vectors(store);
    if !embedding.is_available() {
        return BackfillSummary { purged, ..Default::default() };
    }

    store.unit(Unit::Backfill, |store| {
        let version = embedding.embedding_version();
        let candidates = match store.step(|conn| count_candidates(conn, version)) {
            Ok(count) => count,
            Err(err) => {
                crate::log_line!(
                    "g-mesh daemon: failed to count nodes owed an embedding, skipping the backfill pass ({err:#})"
                );
                return BackfillSummary::default();
            }
        };
        progress.set_embed_total(candidates.max(0) as u64);

        let started = std::time::Instant::now();
        let mut stats = EmbedStats::default();
        let candidates = candidates.max(0) as usize;
        let mut after_id: Option<String> = None;
        let mut filtered = 0;

        loop {
            let page = match store.step(|conn| fetch_candidate_page(conn, version, after_id.as_deref(), PAGE_SIZE)) {
                Ok(page) => page,
                Err(err) => {
                    crate::log_line!(
                        "g-mesh daemon: failed to read a page of nodes owed an embedding, stopping the \
                         backfill pass early ({err:#})"
                    );
                    break;
                }
            };
            if page.is_empty() {
                break;
            }
            after_id = Some(page.last().expect("just checked non-empty").id.clone());
            let page_len = page.len() as u64;

            let diff =
                Diff { upsert_nodes: page.into_iter().map(to_node_record).collect(), ..Default::default() };
            // Inference runs between the unit's steps.
            let computed = embedding.compute(&diff, &mut stats);
            filtered += computed.dropped_len();
            store.store_vectors(embedding, &computed);
            progress.add_embed_done(page_len);
        }

        embedding.finish_unit("backfill", &stats, started.elapsed());
        BackfillSummary { candidates, filtered, embedded: stats.embedded, cache_hits: stats.hits, purged }
    })
}

/// Purge page size: rows are read and their noise deleted in one step, so a
/// page bounds how long the store lock is held.
const PURGE_PAGE_SIZE: i64 = 1024;

/// Deletes the vector of every node [`is_noise`] filters, unless this index
/// already recorded a purge under [`NOISE_RULE_VERSION`] - see this module's
/// "Vectors stored before the noise rule" section. Returns how many vectors
/// it deleted. Best-effort: a failure is logged, and leaves the generation
/// unrecorded, so the next start runs the purge again.
pub(crate) fn purge_noise_vectors(store: &IndexStore) -> usize {
    store.unit(Unit::Backfill, |store| {
        match store.step(|conn| schema::noise_purge_rule(conn)) {
            Ok(Some(rule)) if rule == NOISE_RULE_VERSION => return 0,
            Ok(_) => {}
            Err(err) => {
                crate::log_line!("g-mesh daemon: failed to read the embedding noise purge marker ({err:#})");
                return 0;
            }
        }
        let mut purged = 0;
        let mut after_id = String::new();
        loop {
            match store.step(|conn| purge_noise_page(conn, &after_id, PURGE_PAGE_SIZE)) {
                Ok((_, None)) => break,
                Ok((deleted, Some(last_id))) => {
                    purged += deleted;
                    after_id = last_id;
                }
                Err(err) => {
                    crate::log_line!(
                        "g-mesh daemon: failed to purge the vectors of filtered nodes, retrying next start ({err:#})"
                    );
                    return purged;
                }
            }
        }
        if let Err(err) = store.step(|conn| schema::record_noise_purge(conn, NOISE_RULE_VERSION)) {
            crate::log_line!("g-mesh daemon: {err:#} - the purge runs again next start");
        }
        if purged > 0 {
            crate::log_line!("g-mesh daemon: deleted {purged} vectors of name-only module and test nodes");
        }
        purged
    })
}

/// One keyset page of the purge: reads the nodes with a vector strictly
/// after `after_id`, ordered by id, and deletes the vectors of the ones
/// [`is_noise`] filters. Returns how many it deleted and the page's last id,
/// `None` once no node is left.
fn purge_noise_page(
    conn: &Connection,
    after_id: &str,
    limit: i64,
) -> anyhow::Result<(usize, Option<String>)> {
    type Row = (String, String, String, String, String, Option<String>, Option<String>);
    let rows: Vec<Row> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.kind, n.name, n.qualifiedName, n.filePath, n.docComment, n.signature \
             FROM vectors v JOIN nodes n ON n.id = v.nodeId \
             WHERE v.nodeId > ?1 ORDER BY v.nodeId LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![after_id, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
        })?;
        rows.collect::<SqlResult<_>>()?
    };
    let Some(last_id) = rows.last().map(|row| row.0.clone()) else { return Ok((0, None)) };
    let mut deleted = 0;
    for (id, kind, name, qualified_name, file_path, doc, signature) in &rows {
        let input = EmbedInput {
            kind,
            name,
            qualified_name,
            file_path,
            doc: doc.as_deref(),
            signature: signature.as_deref(),
        };
        if is_noise(&input) {
            vectors::delete(conn, id)?;
            deleted += 1;
        }
    }
    Ok((deleted, Some(last_id)))
}

/// One candidate row: a node's id plus the columns
/// [`EmbeddingPipeline::compute`] actually reads.
struct Candidate {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    doc_comment: Option<String>,
    signature: Option<String>,
}

/// Builds a [`NodeRecord`] carrying only what
/// [`EmbeddingPipeline::compute`] actually reads (`id`, `kind`, `name`,
/// `qualified_name`, `file_path`, `doc_comment`, `signature`) - every other
/// field is irrelevant here, because this `Diff` is never handed to
/// [`crate::storage::write::apply_diff`]; it only exists as `compute`'s
/// input shape. `EmbeddingPipeline::store` re-reads a node's *real*, current
/// row before writing anything, so a filler language here can never leak
/// into a stored answer.
fn to_node_record(candidate: Candidate) -> NodeRecord {
    let Candidate { id, kind, name, qualified_name, file_path, doc_comment, signature } = candidate;
    let mut node = NodeRecord::new(id, kind, name, qualified_name, file_path, "");
    node.doc_comment = doc_comment;
    node.signature = signature;
    node
}

/// Total rows this pass owes the project right now: every node with
/// embeddable text (a doc comment or a signature) and no `vectors` row yet,
/// or one made under an `embeddingVersion` other than `version`.
/// Mirrors [`fetch_candidate_page`]'s own `WHERE` clause exactly - this is
/// the unpaged count of the same set that query pages through.
fn count_candidates(conn: &Connection, version: &str) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id \
         WHERE (v.nodeId IS NULL OR v.embeddingVersion != ?1) \
         AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL)",
        [version],
        |row| row.get(0),
    )
}

/// One keyset page of candidates strictly after `after_id` (or from the
/// start, if `None`), ordered by `id` so paging never revisits or skips a
/// row regardless of how many pages have already run - see this module's own
/// "Termination" doc section.
fn fetch_candidate_page(
    conn: &Connection,
    version: &str,
    after_id: Option<&str>,
    limit: i64,
) -> SqlResult<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.kind, n.name, n.qualifiedName, n.filePath, n.docComment, n.signature \
         FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id \
         WHERE (v.nodeId IS NULL OR v.embeddingVersion != ?3) \
         AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL) \
         AND n.id > ?1 ORDER BY n.id LIMIT ?2",
    )?;
    // Every real node id is a non-empty string, so `id > ''` is true for all
    // of them - the same query serves both "from the very start" (`after_id`
    // is `None`) and "strictly after the last page's id" with one statement.
    let rows = stmt.query_map(rusqlite::params![after_id.unwrap_or(""), limit, version], |row| {
        Ok(Candidate {
            id: row.get(0)?,
            kind: row.get(1)?,
            name: row.get(2)?,
            qualified_name: row.get(3)?,
            file_path: row.get(4)?,
            doc_comment: row.get(5)?,
            signature: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Honors [`HOLD_FILE_ENV`]. A no-op unless it is set, which is every real
/// run - same shape as `daemon::bulk_index::hold_the_walk_open_for_tests`
/// (polled rather than watched: test-only scaffolding, a millisecond-scale
/// wait, bounded so a test that forgets to release it fails as a timeout
/// rather than wedging the daemon forever).
fn hold_before_first_batch_for_tests() {
    let Some(path) = std::env::var_os(HOLD_FILE_ENV).filter(|p| !p.is_empty()) else { return };
    let path = std::path::PathBuf::from(path);
    crate::log_line!(
        "g-mesh daemon: holding the embedding backfill pass until {} is removed ({HOLD_FILE_ENV})",
        path.display()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Honors [`PANIC_ENV`]. A no-op unless it is set, which is every real run -
/// same shape as [`hold_before_first_batch_for_tests`], but panicking instead
/// of holding: GM-395 slice 2b's test that a panic here does not turn a
/// project's structural tools into a tool error needs this pass to actually
/// panic, not merely pause.
fn panic_before_first_batch_for_tests() {
    if std::env::var_os(PANIC_ENV).filter(|p| !p.is_empty()).is_some() {
        panic!("g-mesh daemon: G_MESH_EMBED_PASS_PANIC test knob fired - this panic is expected");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
    use crate::storage::write::apply_diff;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn node(id: &str, doc_comment: Option<&str>, signature: Option<&str>) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, "src/lib.ts", "typescript");
        node.doc_comment = doc_comment.map(str::to_string);
        node.signature = signature.map(str::to_string);
        node
    }

    /// Candidate selection returns exactly the nodes with embeddable text and
    /// no vector row - not a node with neither field (nothing to embed), and
    /// not a node that already has one (already embedded).
    ///
    /// *Control:* drop the `LEFT JOIN vectors ... WHERE v.nodeId IS NULL`
    /// filter from `fetch_candidate_page`'s query, and `already_embedded`
    /// below is wrongly included in the result.
    #[test]
    fn candidate_selection_returns_exactly_the_nodes_owed_an_embedding() {
        let mut conn = open_conn();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    node("documented", Some("does a thing"), None),
                    node("signed_only", None, Some("fn signed_only()")),
                    node("nothing_to_embed", None, None),
                    node("already_embedded", Some("also does a thing"), None),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        crate::storage::vectors::insert(
            &conn,
            "already_embedded",
            &[0.0; crate::embedding::EMBEDDING_DIM],
            "v1",
        )
        .unwrap();

        let count = count_candidates(&conn, "v1").unwrap();
        assert_eq!(count, 2, "only `documented` and `signed_only` are owed an embedding");

        let page = fetch_candidate_page(&conn, "v1", None, 256).unwrap();
        let mut ids: Vec<&str> = page.iter().map(|c| c.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["documented", "signed_only"]);
    }

    /// A vector made under another `embeddingVersion` is owed a re-embed:
    /// this is how a change of pinned weights or of the embedded text form
    /// reaches an existing index. A row tagged with the bare model name (fp32
    /// weights) or with `<model>+int8` (the untrimmed text) is a candidate for
    /// the pipeline's version; a row already carrying it is not.
    ///
    /// *Control:* drop `OR v.embeddingVersion != ?` from either query, and
    /// neither old row is a candidate; drop `TEXT_FORM_TAG` from
    /// `embedding_version`, and `embedded_from_full_text` is not one.
    #[test]
    fn a_vector_from_another_embedding_version_is_owed_a_re_embed() {
        let mut conn = open_conn();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    node("embedded_by_fp32", Some("does a thing"), None),
                    node("embedded_by_int8", Some("does another thing"), None),
                    node("embedded_from_full_text", Some("does a third thing"), None),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let model = crate::config::EmbeddingConfig::default().model;
        let current = crate::embedding::model::embedding_version(&model);
        assert_ne!(current, model, "the default model's version must not be the fp32 weights' bare name");
        let zeros = [0.0; crate::embedding::EMBEDDING_DIM];
        crate::storage::vectors::insert(&conn, "embedded_by_fp32", &zeros, &model).unwrap();
        crate::storage::vectors::insert(&conn, "embedded_by_int8", &zeros, &current).unwrap();
        crate::storage::vectors::insert(&conn, "embedded_from_full_text", &zeros, &format!("{model}+int8"))
            .unwrap();

        assert_eq!(count_candidates(&conn, &current).unwrap(), 2);
        let page = fetch_candidate_page(&conn, &current, None, 256).unwrap();
        let ids: Vec<&str> = page.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["embedded_by_fp32", "embedded_from_full_text"]);
    }

    /// Keyset pagination visits each node exactly once across several pages,
    /// however small the page size - the shape a real pass runs at `PAGE_SIZE`
    /// (256), exercised here at 2 so a handful of rows already needs several
    /// pages.
    #[test]
    fn keyset_pagination_visits_each_candidate_exactly_once_across_pages() {
        let mut conn = open_conn();
        let expected: Vec<String> = (0..7).map(|i| format!("n{i}")).collect();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: expected.iter().map(|id| node(id, Some("doc"), None)).collect(),
                ..Default::default()
            },
        )
        .unwrap();

        let mut seen = Vec::new();
        let mut after_id: Option<String> = None;
        loop {
            let page = fetch_candidate_page(&conn, "v1", after_id.as_deref(), 2).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 2, "a page must never exceed the requested limit");
            after_id = Some(page.last().unwrap().id.clone());
            seen.extend(page.into_iter().map(|c| c.id));
        }

        seen.sort_unstable();
        let mut expected_sorted = expected;
        expected_sorted.sort_unstable();
        assert_eq!(seen, expected_sorted, "every candidate must be visited exactly once");
    }

    /// `embedded` counts what the model computed and `cache_hits` what the
    /// cache served, as `EmbedStats` does: a second project over a warm
    /// cache embeds nothing.
    ///
    /// *Control:* count `computed.len()` into `embedded` (hits included) and
    /// the warm pass reports 3 embedded.
    #[test]
    fn embedded_counts_the_models_vectors_and_cache_hits_the_caches() {
        use crate::embedding::pipeline::test_support::{cache_at, fake_model_dir, fake_pipeline, Counters};
        let scratch = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&scratch.path().join("model"), "weights v1");
        let pass = || {
            crate::storage::vectors::register_extension();
            let mut conn = open_conn();
            apply_diff(
                &mut conn,
                &Diff {
                    upsert_nodes: (0..3)
                        .map(|i| node(&format!("n{i}"), Some(&format!("does thing {i}")), None))
                        .collect(),
                    ..Default::default()
                },
            )
            .unwrap();
            let pipeline = fake_pipeline(&model_dir, Some(cache_at(scratch.path())), &Counters::default());
            run(&IndexStore::new(conn), &pipeline, &IndexingStatus::structural())
        };

        let cold = pass();
        let warm = pass();

        assert_eq!((cold.candidates, cold.embedded, cold.cache_hits), (3, 3, 0), "{cold:?}");
        assert_eq!((warm.candidates, warm.embedded, warm.cache_hits), (3, 0, 3), "{warm:?}");
    }

    /// `is_available() == false` makes the pass return before it ever runs
    /// `count_candidates` - proven with a connection that *does* have the
    /// schema and *does* owe one node an embedding, so an available path
    /// would count it (`candidates == 1`) and only the unavailable, no-query
    /// path reports `candidates == 0`.
    ///
    /// This has to observe `candidates`, not the whole `BackfillSummary`
    /// against `Connection::open_in_memory()` with no schema applied: `run`
    /// swallows `count_candidates`'s missing-table error and *also* returns
    /// `BackfillSummary::default()` on that path, so a schema-less connection
    /// cannot tell "skipped because unavailable" apart from "tried, hit a
    /// missing table, and gave up anyway" - both read as the same default
    /// summary.
    ///
    /// *Control:* in `EmbeddingPipeline::is_available`, drop the early
    /// `self.model.get()` check (or hardcode `true`). `run` then reaches
    /// `count_candidates` against this test's real schema, finds the one
    /// candidate node below, and `summary.candidates` reads `1`.
    #[test]
    fn an_unavailable_model_returns_before_counting_candidates() {
        let mut conn = open_conn();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![node("owed_an_embedding", Some("does a thing"), None)],
                ..Default::default()
            },
        )
        .unwrap();
        let conn = IndexStore::new(conn);
        let embedding = EmbeddingPipeline::disabled();
        let progress = IndexingStatus::structural();

        let summary = run(&conn, &embedding, &progress);

        assert_eq!(
            summary.candidates, 0,
            "an unavailable model must return before counting candidates, even though this project has \
             one node ({:?}) owed an embedding",
            summary
        );
    }

    /// An undocumented `pub mod {id}`: noise under the GM-437 rule.
    fn noise_module(id: &str) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Module", id, format!("crate::{id}"), "src/lib.rs", "rust");
        node.signature = Some(format!("pub mod {id}"));
        node
    }

    /// An index holding `nodes`, each with a vector made before the filter
    /// (at the version `v1`) when its id is in `with_vectors`.
    fn index_with(nodes: Vec<NodeRecord>, with_vectors: &[&str]) -> IndexStore {
        let mut conn = open_conn();
        apply_diff(&mut conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
        for id in with_vectors {
            insert_vector(&conn, id);
        }
        IndexStore::new(conn)
    }

    fn insert_vector(conn: &Connection, id: &str) {
        crate::storage::vectors::insert(conn, id, &[0.0; crate::embedding::EMBEDDING_DIM], "v1").unwrap();
    }

    fn vector_ids(store: &IndexStore) -> Vec<String> {
        store.with(|conn| {
            let mut stmt = conn.prepare("SELECT nodeId FROM vectors ORDER BY nodeId").unwrap();
            stmt.query_map([], |row| row.get(0)).unwrap().collect::<SqlResult<_>>().unwrap()
        })
    }

    fn marker(store: &IndexStore) -> Option<String> {
        store.with(|conn| schema::noise_purge_rule(conn).unwrap())
    }

    /// The design note's survivors of the purge: a documented module and a
    /// name-only fn that is no test.
    fn kept_nodes() -> Vec<NodeRecord> {
        let mut documented = noise_module("documented");
        documented.doc_comment = Some("Semantic search.".to_string());
        vec![documented, node("signed", None, Some("fn signed()"))]
    }

    /// A noise node is a backfill candidate that never reaches the model and
    /// gets no vector; it is counted as filtered, not as owed a vector.
    /// Control: `compute` pushing a noise node's `text_to_embed` text into
    /// `pending` embeds it.
    #[test]
    fn backfill_gives_a_noise_node_no_vector() {
        use crate::embedding::pipeline::test_support::{fake_model_dir, fake_pipeline, Counters};
        crate::storage::vectors::register_extension();
        let scratch = tempfile::tempdir().unwrap();
        let model_dir = fake_model_dir(&scratch.path().join("model"), "weights v1");
        let counters = Counters::default();
        let pipeline = fake_pipeline(&model_dir, None, &counters);
        let store =
            index_with(vec![noise_module("semantic"), node("read", Some("Reads a file."), None)], &[]);

        let summary = run(&store, &pipeline, &IndexingStatus::structural());

        assert_eq!(vector_ids(&store), vec!["read"]);
        assert_eq!(counters.received(), vec!["Reads a file.".to_string()], "only `read` reaches the model");
        assert_eq!((summary.candidates, summary.filtered, summary.embedded), (2, 1, 1), "{summary:?}");
        assert_eq!(summary.owed_a_vector(), 1, "{summary:?}");
    }

    /// The one-time purge deletes a vector a noise node got before the
    /// filter, with no model at all, keeps every other vector, and records
    /// the rule's generation.
    /// Control: running `purge_noise_vectors` after the `is_available`
    /// return leaves `semantic`'s vector; deleting every row in
    /// `purge_noise_page` (no `is_noise` check) loses the kept ones.
    #[test]
    fn the_purge_deletes_pre_filter_noise_vectors_without_a_model_and_keeps_the_rest() {
        let mut nodes = kept_nodes();
        nodes.push(noise_module("semantic"));
        let store = index_with(nodes, &["documented", "semantic", "signed"]);

        let summary = run(&store, &EmbeddingPipeline::disabled(), &IndexingStatus::structural());

        assert_eq!(summary.purged, 1, "{summary:?}");
        assert_eq!(vector_ids(&store), vec!["documented", "signed"]);
        assert_eq!(marker(&store).as_deref(), Some(NOISE_RULE_VERSION));
    }

    /// Once the marker holds the current generation the purge does not run
    /// again: a noise vector stored after it stays. A marker from another
    /// generation purges again and records the current one.
    /// Control: dropping the early return on a current marker purges on
    /// the second run.
    #[test]
    fn the_purge_runs_once_per_rule_version() {
        let store = index_with(vec![noise_module("semantic")], &["semantic"]);
        let pipeline = EmbeddingPipeline::disabled();
        let progress = IndexingStatus::structural();
        assert_eq!(run(&store, &pipeline, &progress).purged, 1);

        store.with(|conn| insert_vector(conn, "semantic"));
        assert_eq!(run(&store, &pipeline, &progress).purged, 0, "a current marker skips the purge");
        assert_eq!(vector_ids(&store), vec!["semantic"]);

        store.with(|conn| schema::record_noise_purge(conn, "0").unwrap());
        assert_eq!(run(&store, &pipeline, &progress).purged, 1, "an older generation purges again");
        assert!(vector_ids(&store).is_empty());
        assert_eq!(marker(&store).as_deref(), Some(NOISE_RULE_VERSION));
    }

    /// A purge that fails records no marker, so the next start runs it
    /// again and then records it.
    /// Control: `break` instead of `return purged` on a page error records
    /// the marker after the failure.
    #[test]
    fn a_failed_purge_records_no_marker_and_runs_again_next_start() {
        let store = index_with(vec![noise_module("semantic")], &["semantic"]);
        store.with(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER refuse_delete BEFORE DELETE ON vectors BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
            .unwrap()
        });

        assert_eq!(purge_noise_vectors(&store), 0);
        assert_eq!(marker(&store), None, "a failed purge must not record its generation");
        assert_eq!(vector_ids(&store), vec!["semantic"]);

        store.with(|conn| conn.execute_batch("DROP TRIGGER refuse_delete;").unwrap());
        assert_eq!(purge_noise_vectors(&store), 1, "the next start purges");
        assert_eq!(marker(&store).as_deref(), Some(NOISE_RULE_VERSION));
    }

    /// The count the start and reindex logs report leaves out filtered
    /// candidates; a pass whose candidates are all noise owes nothing (the
    /// daemon log stays silent on it).
    #[test]
    fn owed_a_vector_leaves_out_the_filtered_candidates() {
        let all_noise = BackfillSummary { candidates: 5, filtered: 5, ..Default::default() };
        assert_eq!(all_noise.owed_a_vector(), 0);
        let mixed = BackfillSummary { candidates: 12, filtered: 2, embedded: 10, ..Default::default() };
        assert_eq!(mixed.owed_a_vector(), 10);
    }
}
