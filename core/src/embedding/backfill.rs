//! The embedding backfill pass: fills in `vectors` rows for every node the
//! graph is missing one for, as its own bounded scan rather than as part of
//! the walk that wrote those nodes in the first place.
//!
//! # Why this exists (GM-395, slice 1)
//!
//! Before this, `daemon::bulk_index::commit` computed and stored a batch's
//! embeddings inline, as part of the cold-start walk - so a project's
//! structural graph and its embeddings became complete at the same instant,
//! and neither could be ready before the other. Measured on g-mesh's own
//! repo, that made a cold start take about 814s, when the walk that produces
//! the structural graph a `find_definition`/`find_references`/... caller
//! actually needs takes about 31s of it.
//!
//! `daemon::bulk_index::run` now takes `embedding: None` for the cold-start
//! walk (see its own doc comment), which makes the walk structural-only, and
//! this module is the "finish the other half" step that runs once,
//! afterward: every node the walk (or an earlier interrupted attempt at this
//! same pass, or an incremental edit whose own embedding step failed) left
//! without a `vectors` row gets one - or is confirmed to have nothing worth
//! embedding - see `embedding::pipeline`'s "What gets embedded" section.
//!
//! `daemon::indexing_status::IndexingStatus`'s `Embedding` phase covers
//! exactly the time this pass runs: structural tools do not wait on it
//! (`Need::Structural` is already satisfied), `search_code`
//! (`Need::Embeddings`) does.
//!
//! # Termination
//!
//! Keyset pagination over `nodes.id`, not "while any unembedded row
//! remains": a node whose inference keeps failing (a pathological input, a
//! persistent ONNX error) or whose text trims to nothing (`text_to_embed`
//! returning `None`, so [`EmbeddingPipeline::compute`] silently skips it)
//! would make a "does anything remain" loop spin forever. Paging strictly
//! past the last id a page saw, ordered by id, guarantees every candidate at
//! the moment this pass started is visited exactly once, however many of
//! them end up unembedded anyway.
//!
//! # No schema or indexer-version bump
//!
//! The graph itself is unchanged by this feature, so an index built before
//! GM-395 needs no reindex - "embeddings complete" is recorded nowhere on
//! disk; this pass's own `COUNT(*)` query *is* the check, and it is `0`,
//! quickly, once every embeddable node already has a row. A side effect
//! worth having: a project whose model was not available at index time (or
//! whose weights simply had not been fetched yet) gets embedded on the next
//! restart that finds one, with no reindex required.

use std::sync::Mutex;

use rusqlite::{Connection, Result as SqlResult};

use crate::daemon::indexing_status::IndexingStatus;
use crate::embedding::EmbeddingPipeline;
use crate::storage::write::{Diff, NodeRecord};

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

/// What one call to [`run`] actually did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackfillSummary {
    /// Candidate rows this pass owed the project when it started - not
    /// necessarily how many it ended up embedding (a node can fail inference,
    /// or be deleted mid-pass by a concurrent edit).
    pub candidates: usize,
    /// Nodes an embedding was actually computed and stored for.
    pub embedded: usize,
}

/// Fills in every `vectors` row this project's graph is currently missing.
///
/// Holds on [`HOLD_FILE_ENV`] first, unconditionally, then returns
/// immediately - before running a single query - if
/// [`EmbeddingPipeline::is_available`] says there is no model to run at all.
/// The hold has to come before that check, not after: a phase-gating test
/// needs to observe the daemon sitting in
/// [`Phase::Embedding`](crate::daemon::indexing_status::Phase::Embedding)
/// regardless of whether the machine running it has ever fetched real
/// weights, and an early return for "no model" would make the hold
/// unreachable on exactly the machines (CI, a fresh checkout) that need it
/// most.
pub fn run(
    conn: &Mutex<Connection>,
    embedding: &EmbeddingPipeline,
    progress: &IndexingStatus,
) -> BackfillSummary {
    hold_before_first_batch_for_tests();

    if !embedding.is_available() {
        return BackfillSummary::default();
    }

    let candidates = match count_candidates(&conn.lock().unwrap()) {
        Ok(count) => count,
        Err(err) => {
            eprintln!(
                "g-mesh daemon: failed to count nodes owed an embedding, skipping the backfill pass ({err:#})"
            );
            return BackfillSummary::default();
        }
    };
    progress.set_embed_total(candidates.max(0) as u64);

    let mut summary = BackfillSummary { candidates: candidates.max(0) as usize, embedded: 0 };
    let mut after_id: Option<String> = None;

    loop {
        let page = {
            let guard = conn.lock().unwrap();
            match fetch_candidate_page(&guard, after_id.as_deref(), PAGE_SIZE) {
                Ok(page) => page,
                Err(err) => {
                    eprintln!(
                        "g-mesh daemon: failed to read a page of nodes owed an embedding, stopping the \
                         backfill pass early ({err:#})"
                    );
                    break;
                }
            }
        };
        if page.is_empty() {
            break;
        }
        after_id = Some(page.last().expect("just checked non-empty").0.clone());
        let page_len = page.len() as u64;

        let diff =
            Diff { upsert_nodes: page.into_iter().map(to_node_record).collect(), ..Default::default() };
        let computed = embedding.compute(&diff);
        summary.embedded += computed.len();
        {
            let guard = conn.lock().unwrap();
            embedding.store(&guard, &computed);
        }
        progress.add_embed_done(page_len);
    }

    summary
}

/// One candidate row: a node's id plus the two columns
/// [`EmbeddingPipeline::compute`] actually reads.
type Candidate = (String, Option<String>, Option<String>);

/// Builds a filler [`NodeRecord`] carrying only what
/// [`EmbeddingPipeline::compute`] actually reads (`id`, `doc_comment`,
/// `signature`) - every other field is irrelevant here, because this
/// `Diff` is never handed to [`crate::storage::write::apply_diff`]; it only
/// exists as `compute`'s input shape. `EmbeddingPipeline::store` re-reads a
/// node's *real*, current row before writing anything (GM-396's staleness
/// check), so a filler kind/name/qualifiedName/filePath/language here can
/// never leak into a stored answer.
fn to_node_record((id, doc_comment, signature): Candidate) -> NodeRecord {
    let mut node = NodeRecord::new(id, "", "", "", "", "");
    node.doc_comment = doc_comment;
    node.signature = signature;
    node
}

/// Total rows this pass owes the project right now: every node with
/// embeddable text (a doc comment or a signature) and no `vectors` row yet.
/// Mirrors [`fetch_candidate_page`]'s own `WHERE` clause exactly - this is
/// the unpaged count of the same set that query pages through.
fn count_candidates(conn: &Connection) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id \
         WHERE v.nodeId IS NULL AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL)",
        [],
        |row| row.get(0),
    )
}

/// One keyset page of candidates strictly after `after_id` (or from the
/// start, if `None`), ordered by `id` so paging never revisits or skips a
/// row regardless of how many pages have already run - see this module's own
/// "Termination" doc section.
fn fetch_candidate_page(conn: &Connection, after_id: Option<&str>, limit: i64) -> SqlResult<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.docComment, n.signature FROM nodes n LEFT JOIN vectors v ON v.nodeId = n.id \
         WHERE v.nodeId IS NULL AND (n.docComment IS NOT NULL OR n.signature IS NOT NULL) \
         AND n.id > ?1 ORDER BY n.id LIMIT ?2",
    )?;
    // Every real node id is a non-empty string, so `id > ''` is true for all
    // of them - the same query serves both "from the very start" (`after_id`
    // is `None`) and "strictly after the last page's id" with one statement.
    let rows = stmt.query_map(rusqlite::params![after_id.unwrap_or(""), limit], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
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
    eprintln!(
        "g-mesh daemon: holding the embedding backfill pass until {} is removed ({HOLD_FILE_ENV})",
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

        let count = count_candidates(&conn).unwrap();
        assert_eq!(count, 2, "only `documented` and `signed_only` are owed an embedding");

        let page = fetch_candidate_page(&conn, None, 256).unwrap();
        let mut ids: Vec<&str> = page.iter().map(|(id, _, _)| id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["documented", "signed_only"]);
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
            let page = fetch_candidate_page(&conn, after_id.as_deref(), 2).unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 2, "a page must never exceed the requested limit");
            after_id = Some(page.last().unwrap().0.clone());
            seen.extend(page.into_iter().map(|(id, _, _)| id));
        }

        seen.sort_unstable();
        let mut expected_sorted = expected;
        expected_sorted.sort_unstable();
        assert_eq!(seen, expected_sorted, "every candidate must be visited exactly once");
    }

    /// `is_available() == false` makes the pass return without ever running a
    /// query - proven by handing it a connection with no schema applied at
    /// all, which any real query against `nodes`/`vectors` would fail
    /// against.
    ///
    /// *Control:* in `EmbeddingPipeline::is_available`, drop the early
    /// `self.model.get()` check (or hardcode `true`), and this test panics
    /// on the missing-table error instead of returning a default summary.
    #[test]
    fn an_unavailable_model_returns_without_running_a_query() {
        let conn = Connection::open_in_memory().unwrap(); // deliberately no schema::apply
        let conn = Mutex::new(conn);
        let embedding = EmbeddingPipeline::disabled();
        let progress = IndexingStatus::structural();

        let summary = run(&conn, &embedding, &progress);

        assert_eq!(summary, BackfillSummary::default());
    }
}
