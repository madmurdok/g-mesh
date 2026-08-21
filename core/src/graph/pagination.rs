use anyhow::{Context, Result};
use base64::prelude::*;
use rusqlite::{params, Connection, Row};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::storage::write::{EdgeRecord, NodeRecord};

/// Page size `find_references`/`find_callers`/`find_callees`/
/// `find_implementations` use when the caller doesn't supply `limit`.
pub const DEFAULT_PAGE_SIZE: usize = 20;

/// Ceiling on a caller-supplied `limit` - comfortably above every fan-out
/// case seen in practice (g-mesh-bench's benchmark corpus tops out around
/// 60), while still bounding worst-case single-response size.
pub const MAX_PAGE_SIZE: usize = 200;

/// Resolves a tool's optional `limit` param to an actual page size: the
/// default when the caller didn't ask for anything specific, otherwise
/// clamped into `[1, MAX_PAGE_SIZE]` rather than erroring on an
/// out-of-range value - a caller-supplied limit isn't a mistake worth
/// failing loudly over, just a suggestion to bound.
pub fn resolve_page_size(limit: Option<u32>) -> usize {
    limit.map(|l| l as usize).unwrap_or(DEFAULT_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE)
}

/// Ceiling on a single MCP tool response body, in serialized-JSON bytes of
/// its `results` array. `MAX_PAGE_SIZE` bounds *row count*, but the thing
/// that actually gets a call rejected is bytes: a `find_references` call
/// with `limit: 200` measured at 54,600 characters, and a `get_dependencies`
/// walk at its old defaults measured at 115,863, both came back rejected
/// outright by the MCP client's transport - not truncated, an error plus a
/// filesystem path to the dropped output (g-mesh-bench's v0.4.0 outlier
/// findings, `find_references`/`get_dependencies` sections). Neither of
/// g-mesh's own transports enforce anything remotely this small - the
/// core<->plugin control channel (`protocol::jsonrpc::MAX_BODY_BYTES`) and
/// the shim's stdio proxy (`protocol::ndjson_frame::MAX_LINE_BYTES`) both cap
/// frames at 64 MiB - so the rejection happens outside code this crate owns,
/// most likely a client-side cap on tool-result size measured in tokens, not
/// bytes. Without knowing that cap's exact value (and it isn't ours to
/// control even if we did), the only safe move is to stay a good deal under
/// the smallest size observed failing: 20,000 bytes is comfortably under
/// 54,600 while still well above what a default-sized page (20 rows) ever
/// produces, so normal calls never notice this exists.
pub const MAX_RESPONSE_BYTES: usize = 20_000;

/// The `kind` value a `File` node carries. A `File` node's `qualifiedName`
/// IS its own project-relative path by construction - see
/// `plugins/typescript/src/extract.ts`'s `run()`, which sets
/// `qualifiedName: this.filePath` for the node it emits for the file itself -
/// so it is byte-identical to that same row's `filePath` in every case,
/// never worth sending twice. Its `startLine`/`startCol` are likewise
/// always the file's own root syntax node position (`(0, 0)`), meaningless
/// as a "where in this file" answer. Neither redundancy holds for any other
/// kind (`Function`, `Type`, ...): a symbol's `qualifiedName` is genuinely
/// different information from its containing file's `filePath`, and its
/// `startLine`/`startCol` is a real, useful position. `find_references`,
/// `find_callers`/`find_callees`, `find_implementations`, and
/// `get_dependencies` (whose `DependencyNode` has no `startLine`/`startCol`
/// to begin with, so only `qualifiedName` applies there) all key off this
/// constant to omit the redundant fields for `File`-kind rows only.
pub const FILE_KIND: &str = "File";

/// One row a caller has already enriched from a [`ScoredEdge`] (typically by
/// resolving its other endpoint into a wire-shaped `T`), carrying back just
/// enough of the original edge - `resolved`, `locality`, and the edge's own
/// `id` - for [`bound_page`] to rebuild the exact cursor `paginate_edges`
/// would have produced had its SQL page ended right there.
pub struct EdgeRow<T> {
    pub item: T,
    pub resolved: bool,
    pub locality: i64,
    pub edge_id: String,
}

/// Longest prefix of `items` whose serialized JSON stays within `budget` bytes,
/// found by binary search on the cut point rather than a linear scan (a
/// handful of `serde_json::to_vec` calls on the whole candidate page instead
/// of one per row). Returns `None` when the whole slice already fits
/// (including the empty slice, which always fits) - the caller's signal that
/// nothing needed cutting. `Some(cut)` is never `Some(0)`, even when a single
/// item alone exceeds `budget`: a caller building a continuation cursor from
/// an empty kept prefix would produce "more available" with no progress
/// behind it - an infinite loop by another name. (This is why the result
/// can't just be a bare `usize` compared against `items.len()`: for a
/// single-item slice that alone busts the budget, the clamped cut and the
/// length are both 1, and only an explicit "did this actually get cut"
/// signal tells the two cases apart.)
///
/// Shared by [`bound_page`] and `mcp::get_dependencies::bound_walk`, which
/// cut different row shapes (`EdgeRow<T>`'s `item`, `DependencyNode`) against
/// the same [`MAX_RESPONSE_BYTES`] budget and then build different kinds of
/// continuation (a structural cursor vs. a resume token) from the cut point -
/// only the search itself was ever identical between them.
pub fn longest_prefix_fitting<T: Serialize>(items: &[T], budget: usize) -> Option<usize> {
    let fits = |n: usize| -> bool {
        serde_json::to_vec(&items[..n]).map(|v| v.len()).unwrap_or(usize::MAX) <= budget
    };

    if fits(items.len()) {
        return None;
    }

    let mut lo = 1usize;
    let mut hi = items.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

/// Truncates an already row-limited page further, if serializing it would
/// exceed [`MAX_RESPONSE_BYTES`], to the longest prefix that fits - handing
/// back `has_more: true` and a cursor resuming right after the cut, reusing
/// `paginate_edges`'s own cursor encoding rather than a byte-truncation-
/// specific scheme. A page that already fits under the budget is returned
/// completely unchanged, `has_more`/`next_cursor` included: this is pure
/// headroom for the rare oversized page, never a new default behavior for
/// the common one.
pub fn bound_page<T: Serialize>(rows: Vec<EdgeRow<T>>, has_more: bool, next_cursor: Option<String>) -> Page<T> {
    bound_page_within(rows, has_more, next_cursor, MAX_RESPONSE_BYTES)
}

/// [`bound_page`] with the byte budget spelled out, so a response that also
/// carries a `files` tally can hold part of [`MAX_RESPONSE_BYTES`] back for
/// it (see [`bound_page_reserving_tally`]) instead of the two sections
/// separately each believing they own the whole ceiling.
fn bound_page_within<T: Serialize>(
    rows: Vec<EdgeRow<T>>,
    has_more: bool,
    next_cursor: Option<String>,
    budget: usize,
) -> Page<T> {
    if rows.is_empty() {
        return Page { results: Vec::new(), has_more, next_cursor, all_unresolved: false };
    }

    let items: Vec<&T> = rows.iter().map(|r| &r.item).collect();
    let Some(cut) = longest_prefix_fitting(&items, budget) else {
        let all_unresolved = rows.iter().all(|r| !r.resolved);
        return Page { results: rows.into_iter().map(|r| r.item).collect(), has_more, next_cursor, all_unresolved };
    };

    // Computed over the surviving prefix, not the pre-truncation `rows`: the
    // marker describes the page actually sent. In practice the two never
    // disagree - `paginate_edges` sorts `resolved: true` first, so a cut
    // point can only ever drop trailing unresolved rows, never a resolved
    // one - but the prefix is the honest source of truth if that ordering
    // rule ever changes.
    let all_unresolved = rows[..cut].iter().all(|r| !r.resolved);
    let boundary = &rows[cut - 1];
    let cursor = encode_cursor(&StructuralCursor {
        resolved: boundary.resolved,
        locality: boundary.locality,
        id: boundary.edge_id.clone(),
    });

    Page {
        results: rows.into_iter().take(cut).map(|r| r.item).collect(),
        has_more: true,
        next_cursor: Some(cursor),
        all_unresolved,
    }
}

/// Opaque cursor-paginated batch, shared shape for every list-shaped MCP
/// tool response. Cursor instead of offset: background reindexing can
/// shift/duplicate rows mid-pagination if positions are counted by offset.
pub struct Page<T> {
    pub results: Vec<T>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    /// True when `results` is non-empty and *every* row came from a
    /// `resolved: false` edge - the shape that looks like an ordinary
    /// complete page (a plausible `results` array, `has_more: false`) but is
    /// actually built entirely from name-matched edges the linker couldn't
    /// confirm, distinguishable from a normal page today only by a caller
    /// scanning every row's own `resolved` bit. [`bound_page`] computes this
    /// from each [`EdgeRow`]'s `resolved` flag over whatever rows actually
    /// survive truncation, so it reflects the page as sent, not the
    /// pre-truncation candidate set. Always `false` for an empty page (there
    /// is nothing to be suspicious of) and for pages built outside the
    /// EdgeRow/bound_page path (`paginate_defines`, `paginate_by_score`, the
    /// raw `paginate_edges` result), which have no per-row resolved concept
    /// to report.
    pub all_unresolved: bool,
}

fn encode_cursor<T: Serialize>(value: &T) -> String {
    BASE64_STANDARD.encode(serde_json::to_vec(value).expect("cursor payload is always serializable"))
}

fn decode_cursor<T: DeserializeOwned>(raw: &str) -> Result<T> {
    let bytes = BASE64_STANDARD.decode(raw).context("invalid pagination cursor encoding")?;
    serde_json::from_slice(&bytes).context("invalid pagination cursor payload")
}

#[derive(Serialize, Deserialize)]
struct StructuralCursor {
    resolved: bool,
    locality: i64,
    id: String,
}

/// Which way to follow edges out of an anchor node.
// Doc comments here are user-facing: `JsonSchema` is derived so
// `get_dependencies`' MCP tool schema can name this exact enum rather than
// restating its variants, and schemars publishes the prose verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Direction {
    /// Edges going out of the anchor node (`fromId = anchor`).
    Outgoing,
    /// Edges coming into the anchor node (`toId = anchor`).
    Incoming,
}

/// One file holding at least one of the edges a [`tally_edge_files`] query
/// matched, and how many of them it holds.
///
/// Deliberately short key names (`path`/`refs`, not `filePath`/`references`
/// as the row shapes spell them): this array exists purely to be *cheaper*
/// than the rows it summarizes, and JSON object keys repeat once per entry -
/// on a 50-file tally the longer spelling costs ~600 bytes of pure key text
/// for no information. The values are unambiguous without it; a tally entry
/// can only ever be a path and a count.
#[derive(Serialize)]
pub struct FileTally {
    pub path: String,
    pub refs: i64,
}

/// Ceiling on how many entries [`tally_edge_files`] returns. Well above the
/// widest fan-out the benchmark corpus produces (excalidraw's `pointFrom`,
/// ~52 referencing files), and at ~40 bytes an entry a full 200 still stays
/// inside [`FILE_TALLY_RESERVE`].
const MAX_FILE_TALLY: usize = 200;

/// Byte budget [`bound_page_reserving_tally`] holds back from
/// [`MAX_RESPONSE_BYTES`] for a `files` tally, so a response carrying both a
/// tally and rows never grows past the same total ceiling a rows-only
/// response already respects. Sized for [`MAX_FILE_TALLY`] entries at typical
/// path lengths.
pub const FILE_TALLY_RESERVE: usize = 8_000;

/// Every distinct file that holds one of the edges [`paginate_edges`] would
/// match for the same anchor/direction/kind/scope, with a per-file count -
/// computed over the *whole* edge set, not one page of it.
///
/// That "whole set" is the point. A high-fan-out anchor's rows get cut by
/// `page_size` and again by [`MAX_RESPONSE_BYTES`], so the file-level answer
/// a rename or impact question actually wants ("which files do I have to
/// touch") is exactly the answer a paginated row list can't give: the caller
/// either walks every cursor page or hedges. One `GROUP BY` over the same
/// predicate answers it completely in a fraction of the bytes, because a
/// tally entry is ~40 bytes where a reference row is ~260.
///
/// Ordered by count descending, then path ascending - the highest-count file
/// is the one most worth reading first on an impact question, and the path
/// tiebreak keeps the order stable across identical counts.
pub fn tally_edge_files(
    conn: &Connection,
    anchor_node_id: &str,
    direction: Direction,
    edge_kinds: &[&str],
    file_paths: &[&str],
) -> Result<Vec<FileTally>> {
    let (other_endpoint, this_endpoint) = match direction {
        Direction::Outgoing => ("toId", "fromId"),
        Direction::Incoming => ("fromId", "toId"),
    };

    // `?1` is the anchor id and `?2` the row cap; the kind filter's
    // placeholders continue after them and the scope filter's after those,
    // same widening rule `paginate_edges` uses.
    let kind_filter = if edge_kinds.is_empty() {
        "1 = 1".to_string()
    } else {
        let placeholders: Vec<String> = (0..edge_kinds.len()).map(|i| format!("?{}", i + 3)).collect();
        format!("e.kind IN ({})", placeholders.join(", "))
    };
    let scope_filter = if file_paths.is_empty() {
        "1 = 1".to_string()
    } else {
        let base = 3 + edge_kinds.len();
        let placeholders: Vec<String> = (0..file_paths.len()).map(|i| format!("?{}", i + base)).collect();
        format!("n.filePath IN ({})", placeholders.join(", "))
    };
    let sql = format!(
        "SELECT n.filePath AS filePath, COUNT(*) AS refs \
         FROM edges e JOIN nodes n ON n.id = e.{other_endpoint} \
         WHERE e.{this_endpoint} = ?1 \
           AND {kind_filter} \
           AND {scope_filter} \
         GROUP BY n.filePath \
         ORDER BY refs DESC, filePath ASC \
         LIMIT ?2"
    );

    let cap = MAX_FILE_TALLY as i64;
    let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&anchor_node_id, &cap];
    sql_params.extend(edge_kinds.iter().map(|kind| kind as &dyn rusqlite::ToSql));
    sql_params.extend(file_paths.iter().map(|path| path as &dyn rusqlite::ToSql));

    let mut stmt = conn.prepare(&sql)?;
    let tally = stmt
        .query_map(sql_params.as_slice(), |row| Ok(FileTally { path: row.get("filePath")?, refs: row.get("refs")? }))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to tally edge files")?;
    Ok(tally)
}

/// Whether a `files` tally is worth sending alongside `results`.
///
/// Two cases, and only two. The page is incomplete (`has_more`), so the rows
/// genuinely cannot show every file and the tally is information the caller
/// has no other cheap way to get. Or the rows repeat files (`rows > files`),
/// so a caller after the file-level answer would otherwise dedupe them by
/// hand - the exact work that costs an agent its reasoning tokens on a
/// high-fan-out lookup.
///
/// Everything else - a complete page whose rows already sit one per file -
/// gets no tally at all, because there the tally would be a byte-for-byte
/// restatement of the `filePath` column. That is what keeps this free on the
/// narrow, exact lookups (a disambiguated symbol with three usages) where the
/// row list is already the whole answer.
pub fn tally_is_worth_sending(row_count: usize, tally: &[FileTally], has_more: bool) -> bool {
    has_more || row_count > tally.len()
}

/// [`bound_page`], but leaving [`FILE_TALLY_RESERVE`] bytes of the response
/// budget free for a `files` tally the caller is about to attach. Rows are
/// still what gets cut - a tally is bounded by [`MAX_FILE_TALLY`] and never
/// grows without bound, while rows do.
pub fn bound_page_reserving_tally<T: Serialize>(
    rows: Vec<EdgeRow<T>>,
    has_more: bool,
    next_cursor: Option<String>,
) -> Page<T> {
    bound_page_within(rows, has_more, next_cursor, MAX_RESPONSE_BYTES - FILE_TALLY_RESERVE)
}

/// An edge alongside the `locality` [`paginate_edges`] already computed for
/// its own ordering (`0` when the edge's other endpoint shares the anchor's
/// file, `1` otherwise) - returned rather than discarded so callers that
/// need it (to build an [`EdgeRow`] for [`bound_page`], say) don't have to
/// re-derive the identical rule by hand after separately resolving the other
/// endpoint's node.
pub struct ScoredEdge {
    pub edge: EdgeRecord,
    pub locality: i64,
}

/// Paginates the anchor node's incident edges, ordered per the structural
/// ordering rule: `resolved: true` before `resolved: false`, then locality
/// (an edge whose other endpoint shares `anchor_file_path` sorts first),
/// then `id` as a stable tiebreaker. Backs find_references/find_callers/
/// find_callees/find_implementations, which all differ only in `direction`
/// and `edge_kinds`.
///
/// `edge_kinds` is a set, not a single kind: the extractor records one usage
/// under exactly one kind (a call is a `CALLS` edge *instead of* a
/// `REFERENCES` one - see `addUsage` in plugins/typescript/src/extract.ts), so a
/// tool whose question spans several of those kinds has to ask for all of
/// them at once. An empty slice means "every kind".
///
/// `file_paths` narrows results to edges whose *other* endpoint (the
/// referencing/calling/implementing node - `n` in the join below, never the
/// anchor) lives in one of the given files. Exact string equality against
/// `nodes.filePath`, matching how every other tool's `file_path` parameter is
/// already stored and compared (project-relative, no leading `./`) - no
/// prefix or glob matching, since a membership test against a known file set
/// (the caller's own use case) never needs one and it would just be
/// unreviewed surface area. Same "empty means unfiltered" convention as
/// `edge_kinds`, for the same reason: omitting the parameter has to mean "no
/// scope, search the whole project" for existing callers to see no behavior
/// change, and an empty slice is the only spelling of "no scope" that doesn't
/// need a separate sentinel.
// Eight independently-meaningful parameters, each already documented above;
// grouping them into a struct would only move the same list one level of
// indirection away from its three call sites without shortening it.
#[allow(clippy::too_many_arguments)]
pub fn paginate_edges(
    conn: &Connection,
    anchor_node_id: &str,
    direction: Direction,
    edge_kinds: &[&str],
    file_paths: &[&str],
    anchor_file_path: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> Result<Page<ScoredEdge>> {
    let decoded: Option<StructuralCursor> = cursor.map(decode_cursor).transpose()?;

    let (other_endpoint, this_endpoint) = match direction {
        Direction::Outgoing => ("toId", "fromId"),
        Direction::Incoming => ("fromId", "toId"),
    };

    // The seven leading placeholders are fixed regardless of whether a cursor
    // is present (`?3 = 0` makes the keyset predicate vacuously true); the
    // kind filter's placeholders continue after them, and the scope filter's
    // continue after those - both vary in width per call.
    let kind_filter = if edge_kinds.is_empty() {
        "1 = 1".to_string()
    } else {
        let placeholders: Vec<String> = (0..edge_kinds.len()).map(|i| format!("?{}", i + 8)).collect();
        format!("e.kind IN ({})", placeholders.join(", "))
    };
    let scope_filter = if file_paths.is_empty() {
        "1 = 1".to_string()
    } else {
        let base = 8 + edge_kinds.len();
        let placeholders: Vec<String> = (0..file_paths.len()).map(|i| format!("?{}", i + base)).collect();
        format!("n.filePath IN ({})", placeholders.join(", "))
    };
    let sql = format!(
        "SELECT e.id AS id, e.fromId AS fromId, e.toId AS toId, e.kind AS kind, e.source AS source, e.resolved AS resolved, \
         e.toDeclaration AS toDeclaration, \
         CASE WHEN n.filePath = ?1 THEN 0 ELSE 1 END AS locality \
         FROM edges e JOIN nodes n ON n.id = e.{other_endpoint} \
         WHERE e.{this_endpoint} = ?2 \
           AND {kind_filter} \
           AND {scope_filter} \
           AND ( \
             ?3 = 0 \
             OR e.resolved < ?4 \
             OR (e.resolved = ?4 AND locality > ?5) \
             OR (e.resolved = ?4 AND locality = ?5 AND e.id > ?6) \
           ) \
         ORDER BY e.resolved DESC, locality ASC, e.id ASC \
         LIMIT ?7"
    );

    let (has_cursor, cursor_resolved, cursor_locality, cursor_id): (i64, i64, i64, String) = match &decoded {
        Some(c) => (1, c.resolved as i64, c.locality, c.id.clone()),
        None => (0, 0, 0, String::new()),
    };
    let limit = (page_size + 1) as i64;

    let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![
        &anchor_file_path,
        &anchor_node_id,
        &has_cursor,
        &cursor_resolved,
        &cursor_locality,
        &cursor_id,
        &limit,
    ];
    sql_params.extend(edge_kinds.iter().map(|kind| kind as &dyn rusqlite::ToSql));
    sql_params.extend(file_paths.iter().map(|path| path as &dyn rusqlite::ToSql));

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<(EdgeRecord, i64)> = stmt
        .query_map(
            sql_params.as_slice(),
            |row| {
                let locality: i64 = row.get("locality")?;
                Ok((
                    EdgeRecord {
                        id: row.get("id")?,
                        from_id: row.get("fromId")?,
                        to_id: row.get("toId")?,
                        kind: row.get("kind")?,
                        source: row.get("source")?,
                        resolved: row.get("resolved")?,
                        to_declaration: row.get("toDeclaration")?,
                    },
                    locality,
                ))
            },
        )?
        .collect::<rusqlite::Result<_>>()
        .context("failed to paginate edges")?;

    let has_more = rows.len() > page_size;
    rows.truncate(page_size);

    let next_cursor = has_more.then(|| {
        let (edge, locality) = rows.last().expect("has_more implies at least one row");
        encode_cursor(&StructuralCursor {
            resolved: edge.resolved,
            locality: *locality,
            id: edge.id.clone(),
        })
    });

    Ok(Page {
        results: rows.into_iter().map(|(edge, locality)| ScoredEdge { edge, locality }).collect(),
        has_more,
        next_cursor,
        // Intermediate page: `bound_page` computes the real marker once
        // callers have wrapped these rows in `EdgeRow`. Nothing here reads
        // this field before that happens.
        all_unresolved: false,
    })
}

#[derive(Serialize, Deserialize)]
struct SourceOrderCursor {
    start_line: i64,
    start_col: i64,
    id: String,
}

/// Paginates the symbols a `File` node's `DEFINES` edges reach, ordered by
/// source position (`startLine` then `startCol` ascending, `id` as a stable
/// tiebreaker for same-position nodes) rather than `paginate_edges`'
/// resolved/locality rule - `get_file_outline`'s whole point is to read back
/// "as the file reads", not ranked by confidence. Joins straight to `nodes`
/// instead of returning `EdgeRecord`s to resolve one at a time, since every
/// caller wants the full node here and there's no ambiguity about which end
/// of the edge that is.
pub fn paginate_defines(
    conn: &Connection,
    file_node_id: &str,
    page_size: usize,
    cursor: Option<&str>,
) -> Result<Page<NodeRecord>> {
    let decoded: Option<SourceOrderCursor> = cursor.map(decode_cursor).transpose()?;

    let sql = "SELECT n.* FROM edges e JOIN nodes n ON n.id = e.toId \
               WHERE e.fromId = ?1 AND e.kind = 'DEFINES' \
                 AND ( \
                   ?2 = 0 \
                   OR n.startLine > ?3 \
                   OR (n.startLine = ?3 AND n.startCol > ?4) \
                   OR (n.startLine = ?3 AND n.startCol = ?4 AND n.id > ?5) \
                 ) \
               ORDER BY n.startLine ASC, n.startCol ASC, n.id ASC \
               LIMIT ?6";

    let (has_cursor, cursor_line, cursor_col, cursor_id): (i64, i64, i64, String) = match &decoded {
        Some(c) => (1, c.start_line, c.start_col, c.id.clone()),
        None => (0, 0, 0, String::new()),
    };
    let limit = (page_size + 1) as i64;

    let mut stmt = conn.prepare(sql)?;
    let mut rows: Vec<NodeRecord> = stmt
        .query_map(params![file_node_id, has_cursor, cursor_line, cursor_col, cursor_id, limit], crate::graph::queries::map_node_row)?
        .collect::<rusqlite::Result<_>>()
        .context("failed to paginate DEFINES edges")?;

    let has_more = rows.len() > page_size;
    rows.truncate(page_size);

    let next_cursor = has_more.then(|| {
        let last = rows.last().expect("has_more implies at least one row");
        encode_cursor(&SourceOrderCursor { start_line: last.start_line, start_col: last.start_col, id: last.id.clone() })
    });

    // No per-row resolved concept here - `DEFINES` rows are a file's own
    // declarations, read back in source order, not name-matched edges.
    Ok(Page { results: rows, has_more, next_cursor, all_unresolved: false })
}

#[derive(Serialize, Deserialize)]
struct ScoreCursor {
    score: f64,
    id: String,
}

/// Generic keyset pagination for `search_code`-shaped results, ordered by
/// similarity score (descending) then `id` as a tiebreaker. `base_sql` must
/// project `score` (REAL) and `id` (unique) columns; `map_row` reads
/// whatever columns the caller needs, plus `score`/`id` for cursor state.
/// Not yet called by any tool (search_code lands with the Embeddings epic)
/// but exercised directly in tests so the ordering rule is proven now.
pub fn paginate_by_score<T>(
    conn: &Connection,
    base_sql: &str,
    params: &[&dyn rusqlite::ToSql],
    page_size: usize,
    cursor: Option<&str>,
    map_row: impl Fn(&Row) -> rusqlite::Result<(T, f64, String)>,
) -> Result<Page<T>> {
    let decoded: Option<ScoreCursor> = cursor.map(decode_cursor).transpose()?;

    let n = params.len();
    let (has_cursor_idx, score_idx, id_idx, limit_idx) = (n + 1, n + 2, n + 3, n + 4);
    let sql = format!(
        "SELECT * FROM ({base_sql}) AS page \
         WHERE ?{has_cursor_idx} = 0 \
            OR score < ?{score_idx} \
            OR (score = ?{score_idx} AND id > ?{id_idx}) \
         ORDER BY score DESC, id ASC \
         LIMIT ?{limit_idx}"
    );

    let (has_cursor, cursor_score, cursor_id): (i64, f64, String) = match &decoded {
        Some(c) => (1, c.score, c.id.clone()),
        None => (0, 0.0, String::new()),
    };
    let limit = (page_size + 1) as i64;

    let mut all_params: Vec<&dyn rusqlite::ToSql> = params.to_vec();
    all_params.push(&has_cursor);
    all_params.push(&cursor_score);
    all_params.push(&cursor_id);
    all_params.push(&limit);

    let mut stmt = conn.prepare(&sql)?;
    let mut rows: Vec<(T, f64, String)> = stmt
        .query_map(all_params.as_slice(), &map_row)?
        .collect::<rusqlite::Result<_>>()
        .context("failed to paginate by score")?;

    let has_more = rows.len() > page_size;
    rows.truncate(page_size);

    let next_cursor = has_more.then(|| {
        let (_, score, id) = rows.last().expect("has_more implies at least one row");
        encode_cursor(&ScoreCursor { score: *score, id: id.clone() })
    });

    Ok(Page {
        results: rows.into_iter().map(|(item, _, _)| item).collect(),
        has_more,
        next_cursor,
        // No per-row resolved concept here - ranked by similarity score, not
        // linker confidence.
        all_unresolved: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn make_node(conn: &Connection, id: &str, file_path: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, ?2, 0, 0, 0, 0, 'rust')",
            params![id, file_path],
        )
        .unwrap();
    }

    fn make_edge(conn: &Connection, id: &str, from: &str, to: &str, resolved: bool) {
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved) VALUES (?1, ?2, ?3, 'CALLS', 'tree-sitter', ?4)",
            params![id, from, to, resolved],
        )
        .unwrap();
    }

    #[test]
    fn resolve_page_size_defaults_when_no_limit_given() {
        assert_eq!(resolve_page_size(None), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn resolve_page_size_clamps_an_oversized_limit_down_to_the_ceiling() {
        assert_eq!(resolve_page_size(Some(10_000)), MAX_PAGE_SIZE);
    }

    #[test]
    fn resolve_page_size_clamps_a_zero_limit_up_to_one() {
        assert_eq!(resolve_page_size(Some(0)), 1);
    }

    #[test]
    fn resolve_page_size_passes_an_in_range_limit_through_unchanged() {
        assert_eq!(resolve_page_size(Some(50)), 50);
    }

    #[test]
    fn resolved_true_sorts_before_resolved_false_at_equal_locality() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "n1", "a.rs");
        make_node(&conn, "n2", "a.rs");
        make_edge(&conn, "e_unresolved", "root", "n1", false);
        make_edge(&conn, "e_resolved", "root", "n2", true);

        let page = paginate_edges(&conn, "root", Direction::Outgoing, &[], &[], "a.rs", 10, None).unwrap();
        assert_eq!(page.results.len(), 2);
        assert!(page.results[0].edge.resolved, "resolved edge must sort first at equal locality");
        assert!(!page.results[1].edge.resolved);
        assert!(!page.has_more);
    }

    #[test]
    fn locality_breaks_ties_after_resolved() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "far", "b.rs");
        make_node(&conn, "near", "a.rs");
        make_edge(&conn, "e_far", "root", "far", true);
        make_edge(&conn, "e_near", "root", "near", true);

        let page = paginate_edges(&conn, "root", Direction::Outgoing, &[], &[], "a.rs", 10, None).unwrap();
        assert_eq!(page.results[0].edge.id, "e_near", "same-file target must sort before a distant one");
        assert_eq!(page.results[1].edge.id, "e_far");
    }

    #[test]
    fn incoming_direction_paginates_edges_pointing_at_the_anchor() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "caller1", "a.rs");
        make_node(&conn, "caller2", "b.rs");
        make_edge(&conn, "e1", "caller1", "root", true);
        make_edge(&conn, "e2", "caller2", "root", true);

        let page = paginate_edges(&conn, "root", Direction::Incoming, &[], &[], "a.rs", 10, None).unwrap();
        let ids: Vec<&str> = page.results.iter().map(|e| e.edge.id.as_str()).collect();
        assert_eq!(ids, vec!["e1", "e2"], "same-file caller must sort before the distant one");
    }

    #[test]
    fn pagination_returns_every_row_once_even_with_inserts_between_calls() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        for i in 0..5 {
            let id = format!("n{i}");
            make_node(&conn, &id, "a.rs");
            make_edge(&conn, &format!("e{i}"), "root", &id, i % 2 == 0);
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = paginate_edges(&conn, "root", Direction::Outgoing, &[], &[], "a.rs", 2, cursor.as_deref()).unwrap();
            seen.extend(page.results.iter().map(|e| e.edge.id.clone()));

            // Simulate a background reindex inserting a new low-priority edge
            // (unresolved, distant file) after the first page - it must not
            // disturb the pages already served or duplicate/skip the
            // original five.
            if seen.len() == 2 {
                make_node(&conn, "intruder", "z.rs");
                make_edge(&conn, "e_intruder", "root", "intruder", false);
            }

            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        let original: Vec<String> = (0..5).map(|i| format!("e{i}")).collect();
        for id in &original {
            assert_eq!(seen.iter().filter(|s| *s == id).count(), 1, "row {id} must appear exactly once");
        }
        assert!(seen.contains(&"e_intruder".to_string()), "the new lowest-priority row lands on the final page");
        assert_eq!(seen.len(), 6);
    }

    #[test]
    fn file_paths_scope_excludes_rows_from_files_outside_the_given_set() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "in_scope", "b.rs");
        make_node(&conn, "out_of_scope", "c.rs");
        make_edge(&conn, "e_in", "root", "in_scope", true);
        make_edge(&conn, "e_out", "root", "out_of_scope", true);

        let page = paginate_edges(&conn, "root", Direction::Outgoing, &[], &["b.rs"], "a.rs", 10, None).unwrap();
        let ids: Vec<&str> = page.results.iter().map(|e| e.edge.id.as_str()).collect();
        assert_eq!(ids, vec!["e_in"], "only the row whose other endpoint is in the scoped file set must come back");
    }

    #[test]
    fn file_paths_scope_matches_against_multiple_files_at_once() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "in_one", "b.rs");
        make_node(&conn, "in_two", "c.rs");
        make_node(&conn, "out", "d.rs");
        make_edge(&conn, "e_one", "root", "in_one", true);
        make_edge(&conn, "e_two", "root", "in_two", true);
        make_edge(&conn, "e_out", "root", "out", true);

        let page = paginate_edges(&conn, "root", Direction::Outgoing, &[], &["b.rs", "c.rs"], "a.rs", 10, None).unwrap();
        let mut ids: Vec<&str> = page.results.iter().map(|e| e.edge.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["e_one", "e_two"], "every file in the scope set must contribute its rows");
    }

    #[test]
    fn an_empty_file_paths_slice_behaves_exactly_like_an_omitted_scope() {
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        make_node(&conn, "far", "b.rs");
        make_node(&conn, "near", "a.rs");
        make_edge(&conn, "e_far", "root", "far", true);
        make_edge(&conn, "e_near", "root", "near", true);

        let scoped = paginate_edges(&conn, "root", Direction::Outgoing, &[], &[], "a.rs", 10, None).unwrap();
        let unscoped = paginate_edges(&conn, "root", Direction::Outgoing, &[], &[], "a.rs", 10, None).unwrap();
        let scoped_ids: Vec<&str> = scoped.results.iter().map(|e| e.edge.id.as_str()).collect();
        let unscoped_ids: Vec<&str> = unscoped.results.iter().map(|e| e.edge.id.as_str()).collect();
        assert_eq!(scoped_ids, unscoped_ids, "an empty file_paths slice must return the exact same rows as omitting it");
        assert_eq!(scoped_ids, vec!["e_near", "e_far"]);
    }

    #[test]
    fn a_scope_narrowing_a_huge_result_set_down_to_one_page_still_paginates_correctly_across_the_boundary() {
        // Reproduces the benchmark's shape: many rows exist project-wide, but
        // scoping to a handful of known files should page through exactly
        // those rows, with a cursor that resumes correctly - the scope filter
        // must not corrupt cursor state derived from resolved/locality/id.
        let conn = setup();
        make_node(&conn, "root", "a.rs");
        for i in 0..20 {
            let id = format!("noise_{i}");
            make_node(&conn, &id, "noise.rs");
            make_edge(&conn, &format!("e_noise_{i}"), "root", &id, true);
        }
        for i in 0..3 {
            let id = format!("scoped_{i}");
            make_node(&conn, &id, "scoped.rs");
            make_edge(&conn, &format!("e_scoped_{i}"), "root", &id, true);
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = paginate_edges(&conn, "root", Direction::Outgoing, &[], &["scoped.rs"], "a.rs", 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one scoped row per page");
            seen.extend(page.results.iter().map(|e| e.edge.id.clone()));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(
            seen,
            vec!["e_scoped_0", "e_scoped_1", "e_scoped_2"],
            "only the scoped rows must be seen, each exactly once, none of the 20 noise rows leaking in"
        );
    }

    fn make_node_at(conn: &Connection, id: &str, file_path: &str, start_line: i64, start_col: i64) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, ?2, ?3, ?4, ?3, ?4, 'rust')",
            params![id, file_path, start_line, start_col],
        )
        .unwrap();
    }

    fn make_defines_edge(conn: &Connection, id: &str, file_id: &str, symbol_id: &str) {
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved) VALUES (?1, ?2, ?3, 'DEFINES', 'tree-sitter', false)",
            params![id, file_id, symbol_id],
        )
        .unwrap();
    }

    #[test]
    fn paginate_defines_orders_by_source_position_not_insertion_order() {
        let conn = setup();
        make_node(&conn, "file", "a.rs");
        make_node_at(&conn, "third", "a.rs", 30, 0);
        make_node_at(&conn, "first", "a.rs", 5, 0);
        make_node_at(&conn, "second", "a.rs", 5, 4);
        make_defines_edge(&conn, "e_third", "file", "third");
        make_defines_edge(&conn, "e_first", "file", "first");
        make_defines_edge(&conn, "e_second", "file", "second");

        let page = paginate_defines(&conn, "file", 10, None).unwrap();
        let ids: Vec<&str> = page.results.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["first", "second", "third"], "must come back in source order, not insertion order");
        assert!(!page.has_more);
    }

    #[test]
    fn paginate_defines_paginates_across_cursor_continuation() {
        let conn = setup();
        make_node(&conn, "file", "a.rs");
        for i in 0..5 {
            let id = format!("n{i}");
            make_node_at(&conn, &id, "a.rs", i, 0);
            make_defines_edge(&conn, &format!("e{i}"), "file", &id);
        }

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = paginate_defines(&conn, "file", 2, cursor.as_deref()).unwrap();
            seen.extend(page.results.into_iter().map(|n| n.id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        assert_eq!(seen, vec!["n0", "n1", "n2", "n3", "n4"], "must return every symbol exactly once, in source order");
    }

    #[test]
    fn score_pagination_orders_by_score_descending() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE scored (id TEXT PRIMARY KEY, score REAL, label TEXT)").unwrap();
        conn.execute("INSERT INTO scored VALUES ('a', 0.5, 'low'), ('b', 0.9, 'high'), ('c', 0.7, 'mid')", [])
            .unwrap();

        let page = paginate_by_score::<String>(
            &conn,
            "SELECT id, score, label FROM scored",
            &[],
            10,
            None,
            |row| Ok((row.get::<_, String>("label")?, row.get("score")?, row.get("id")?)),
        )
        .unwrap();

        assert_eq!(page.results, vec!["high", "mid", "low"]);
        assert!(!page.has_more);
    }

    #[derive(Serialize)]
    struct Item {
        id: String,
        blob: String,
    }

    fn edge_row(id: &str, blob_len: usize) -> EdgeRow<Item> {
        EdgeRow { item: Item { id: id.to_string(), blob: "x".repeat(blob_len) }, resolved: true, locality: 0, edge_id: format!("e_{id}") }
    }

    fn edge_row_resolved(id: &str, resolved: bool) -> EdgeRow<Item> {
        EdgeRow { item: Item { id: id.to_string(), blob: String::new() }, resolved, locality: 0, edge_id: format!("e_{id}") }
    }

    #[test]
    fn a_page_that_already_fits_the_byte_budget_is_returned_completely_unchanged() {
        let rows = vec![edge_row("a", 10), edge_row("b", 10)];
        let page = bound_page(rows, true, Some("upstream-cursor".to_string()));

        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].id, "a");
        assert_eq!(page.results[1].id, "b");
        assert!(page.has_more, "has_more must pass through untouched when nothing needed truncating");
        assert_eq!(
            page.next_cursor.as_deref(),
            Some("upstream-cursor"),
            "an upstream cursor must not be replaced just because bound_page ran"
        );
    }

    #[test]
    fn an_oversized_page_truncates_to_the_longest_prefix_that_fits_the_budget() {
        // Each row's blob is ~1000 bytes: comfortably under budget alone, but
        // 30 of them together blow past MAX_RESPONSE_BYTES (20,000).
        let rows: Vec<EdgeRow<Item>> = (0..30).map(|i| edge_row(&format!("n{i:02}"), 1000)).collect();
        let page = bound_page(rows, false, None);

        assert!(page.results.len() < 30, "the full 30 rows must not fit in one page: {}", page.results.len());
        assert!(!page.results.is_empty());
        assert!(page.has_more, "a byte-truncated page must always report more");
        let cursor = page.next_cursor.expect("a byte-truncated page must carry a resumable cursor");

        let raw = serde_json::to_vec(&page.results).unwrap();
        assert!(raw.len() <= MAX_RESPONSE_BYTES, "the truncated page itself must respect the budget: {}", raw.len());

        // The cursor must resume right after the last row actually returned,
        // not an arbitrary or off-by-one boundary.
        let last_included = page.results.last().unwrap().id.clone();
        let decoded: StructuralCursor = decode_cursor(&cursor).unwrap();
        assert_eq!(decoded.id, format!("e_{last_included}"));
    }

    #[test]
    fn a_single_row_that_alone_exceeds_the_budget_is_still_returned_rather_than_an_empty_page() {
        let rows = vec![edge_row("huge", MAX_RESPONSE_BYTES + 1000)];
        let page = bound_page(rows, false, None);

        assert_eq!(page.results.len(), 1, "at least one row must survive even if it alone busts the budget");
        assert!(page.has_more, "an oversized single row is still a truncation, not a complete page");
        assert!(page.next_cursor.is_some());
    }

    #[test]
    fn an_empty_page_is_returned_unchanged_regardless_of_has_more() {
        let page: Page<Item> = bound_page(Vec::new(), false, None);
        assert!(page.results.is_empty());
        assert!(!page.has_more);
        assert!(page.next_cursor.is_none());
        assert!(!page.all_unresolved, "an empty page has nothing to be suspicious of");
    }

    #[test]
    fn a_page_where_every_row_is_unresolved_is_flagged_all_unresolved() {
        let rows = vec![edge_row_resolved("a", false), edge_row_resolved("b", false)];
        let page = bound_page(rows, false, None);
        assert!(page.all_unresolved, "every row unresolved must set the marker");
    }

    #[test]
    fn a_page_with_at_least_one_resolved_row_is_not_flagged_even_if_most_rows_are_unresolved() {
        let rows = vec![
            edge_row_resolved("resolved", true),
            edge_row_resolved("b", false),
            edge_row_resolved("c", false),
            edge_row_resolved("d", false),
        ];
        let page = bound_page(rows, false, None);
        assert!(!page.all_unresolved, "a single resolved row must be enough to clear the marker");
    }

    #[test]
    fn an_empty_result_set_is_never_flagged_all_unresolved() {
        let page: Page<Item> = bound_page(Vec::new(), false, None);
        assert!(!page.all_unresolved, "nothing to be suspicious of when there are no rows at all");
    }

    #[test]
    fn all_unresolved_reflects_the_truncated_prefix_actually_returned() {
        // Every row is unresolved and, individually, blows the byte budget -
        // forcing bound_page down its truncation path. The marker must still
        // come out true: truncation cuts the tail, but resolved:true rows
        // always sort first, so a cut prefix can only ever contain a subset
        // of an already-all-unresolved set.
        let rows: Vec<EdgeRow<Item>> = (0..5)
            .map(|i| EdgeRow {
                item: Item { id: format!("n{i}"), blob: "x".repeat(MAX_RESPONSE_BYTES / 3) },
                resolved: false,
                locality: 0,
                edge_id: format!("e_n{i}"),
            })
            .collect();
        let page = bound_page(rows, false, None);
        assert!(page.results.len() < 5, "must actually have truncated for this test to prove anything");
        assert!(page.all_unresolved);
    }
}
