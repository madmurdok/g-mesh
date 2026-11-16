//! The byte budget of an MCP response: how many serialized-JSON bytes an
//! answer may carry, and the cuts that keep a page, a tally and the whole
//! envelope inside it. The keyset SQL that produces the rows is
//! `graph::pagination`'s.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{
    encode_cursor, source_order_cursor, tally_edge_files_limited, EdgeRow, FileTally, Page, MAX_FILE_TALLY,
};
use crate::graph::Direction;
use crate::model::NodeRecord;

/// Ceiling on a single MCP tool response body, in serialized-JSON bytes of
/// the whole response: rows, tallies, hints and every other field.
/// `MAX_PAGE_SIZE` bounds *row count*, but the thing
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

/// Ceiling on one `get_file_outline` response, in the same unit as
/// [`MAX_RESPONSE_BYTES`] and applied whatever `limit` and `detail` are. An
/// outline is read whole far less often than it is re-read: every byte of it
/// is resent on each later turn of the conversation. Measured before this
/// bound (GM-523), `limit: 200` on `plugins/sdk/src/lsp/bridge.rs` was one
/// 54,975-byte answer and on excalidraw's `App.tsx` 59,754 bytes. 8,000 is
/// the size [`FILE_TALLY_MAX_BYTES`] already uses: about 59 compact rows of
/// `bridge.rs` per page, about 2k tokens at most.
pub const OUTLINE_MAX_RESPONSE_BYTES: usize = 8_000;

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
#[cfg(test)]
pub fn bound_page<T: Serialize>(
    rows: Vec<EdgeRow<T>>,
    has_more: bool,
    next_cursor: Option<String>,
) -> Page<T> {
    bound_page_within(rows, has_more, next_cursor, MAX_RESPONSE_BYTES)
}

/// Serialized JSON length of `value`, the unit every byte budget here is in.
pub fn wire_len<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len())
}

/// The largest byte budget for a response's one growable part (its rows, or
/// a tally that is itself the answer) at which the whole response fits
/// [`MAX_RESPONSE_BYTES`]: [`fit_budget_within`] at that ceiling.
pub fn fit_budget(measure: impl FnMut(usize) -> (usize, usize)) -> usize {
    fit_budget_within(MAX_RESPONSE_BYTES, measure)
}

/// The largest byte budget for a response's one growable part at which the
/// whole response fits `ceiling` bytes.
///
/// `measure(budget)` builds the response with that part cut to `budget`
/// bytes and returns `(part bytes, whole response bytes)`. The rest of the
/// response is measured, never estimated: it may change with the cut (a cut
/// page gains `total` and a cursor, a dropped row can return a file to a
/// tally), and each round lowers the budget by the overshoot just measured.
/// The budget only falls, and a part that stops shrinking is at its floor
/// (one row, or an empty tally), so the search ends; the response then
/// exceeds the ceiling only when that floor alone and the response's other
/// fields do, which their own byte caps rule out for any input but a single
/// row or anchor that is itself near the ceiling.
pub fn fit_budget_within(ceiling: usize, mut measure: impl FnMut(usize) -> (usize, usize)) -> usize {
    let mut budget = ceiling;
    let mut last_part = usize::MAX;
    loop {
        let (part, whole) = measure(budget);
        if whole <= ceiling || part >= last_part || budget == 0 {
            return budget;
        }
        last_part = part;
        budget = budget.min(part).saturating_sub(whole - ceiling);
    }
}

/// [`bound_page`] for a page that is one field of a larger response:
/// `response_len` serializes the response a candidate page would produce, and
/// the rows are cut until that whole response fits [`MAX_RESPONSE_BYTES`]
/// (see [`fit_budget`]). A page whose response already fits is returned
/// unchanged.
pub fn bound_page_in_response<T: Serialize + Clone>(
    rows: Vec<EdgeRow<T>>,
    has_more: bool,
    next_cursor: Option<String>,
    mut response_len: impl FnMut(&Page<T>) -> usize,
) -> Page<T> {
    let budget = fit_budget(|budget| {
        let page = bound_page_within(rows.clone(), has_more, next_cursor.clone(), budget);
        (wire_len(&page.results), response_len(&page))
    });
    bound_page_within(rows, has_more, next_cursor, budget)
}

/// A page's `total` as measured for a candidate page: present exactly when
/// the sent page's is (`has_more`), at the widest value a count can take, so
/// the page sent is never longer than the one measured.
pub fn widest_total(has_more: bool) -> Option<usize> {
    has_more.then_some(usize::MAX)
}

/// Cuts `items` to the longest prefix whose serialized array fits `max_bytes`,
/// possibly to nothing. True when anything was cut.
pub fn truncate_to_bytes<T: Serialize>(items: &mut Vec<T>, max_bytes: usize) -> bool {
    let Some(mut keep) = longest_prefix_fitting(items, max_bytes) else {
        return false;
    };
    if wire_len(&items[..keep]) > max_bytes {
        keep = 0;
    }
    items.truncate(keep);
    true
}

/// [`bound_page`] with the byte budget for the `results` array spelled out.
pub(crate) fn bound_page_within<T: Serialize>(
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
        let all_unresolved = rows.iter().all(|r| !r.rank.resolved);
        return Page {
            results: rows.into_iter().map(|r| r.item).collect(),
            has_more,
            next_cursor,
            all_unresolved,
        };
    };

    // Computed over the surviving prefix, not the pre-truncation `rows`: the
    // marker describes the page actually sent. In practice the two never
    // disagree - `paginate_edges` sorts `resolved: true` first, so a cut
    // point can only ever drop trailing unresolved rows, never a resolved
    // one - but the prefix is the honest source of truth if that ordering
    // rule ever changes.
    let all_unresolved = rows[..cut].iter().all(|r| !r.rank.resolved);
    let cursor = encode_cursor(&rows[cut - 1].rank);

    Page {
        results: rows.into_iter().take(cut).map(|r| r.item).collect(),
        has_more: true,
        next_cursor: Some(cursor),
        all_unresolved,
    }
}

/// Ceiling on a row page's `files` tally in serialized bytes, applied after
/// [`MAX_FILE_TALLY`] (see [`tally_edge_files_bounded`]). Together with
/// [`EXCLUDED_TALLY_MAX_BYTES`] and the other disclosures' caps it keeps a
/// response's non-row fields well inside [`MAX_RESPONSE_BYTES`], so rows
/// always have room.
pub const FILE_TALLY_MAX_BYTES: usize = 8_000;

/// Ceiling on how many files the excluded-references tally on
/// `find_callers`/`find_callees` names.
///
/// A quarter of [`MAX_FILE_TALLY`], because this tally is a *footnote to*
/// another answer rather than the answer: it rides on a response that may
/// already carry a full-sized `files` tally plus rows, and the set it
/// summarises is by construction the leftovers of a `CALLS` walk rather than a
/// symbol's whole fan-out. The widest such set the benchmark corpus produces
/// is excalidraw's `pointFrom` at 26 files, so 50 leaves headroom of the same
/// order [`MAX_FILE_TALLY`] leaves over its own worst case.
pub const MAX_EXCLUDED_FILE_TALLY: usize = 50;

/// Ceiling on the excluded-references tally's `files` in serialized bytes, a
/// quarter of [`FILE_TALLY_MAX_BYTES`] on the same rule as
/// [`MAX_EXCLUDED_FILE_TALLY`].
pub const EXCLUDED_TALLY_MAX_BYTES: usize = 2_000;

/// [`tally_edge_files`] for a row page's `files` field: capped at
/// [`MAX_FILE_TALLY`] entries and [`FILE_TALLY_MAX_BYTES`] bytes, with
/// whether either cap left files out.
pub fn tally_edge_files_bounded(
    conn: &Connection,
    anchor_node_id: &str,
    direction: Direction,
    edge_kinds: &[&str],
    file_paths: &[&str],
) -> Result<(Vec<FileTally>, bool)> {
    let mut files = tally_edge_files_limited(
        conn,
        anchor_node_id,
        direction,
        edge_kinds,
        file_paths,
        MAX_FILE_TALLY + 1,
    )?;
    let over_count = files.len() > MAX_FILE_TALLY;
    files.truncate(MAX_FILE_TALLY);
    let over_bytes = truncate_to_bytes(&mut files, FILE_TALLY_MAX_BYTES);
    Ok((files, over_count || over_bytes))
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

/// Cuts a [`paginate_defines`] page so the whole response it is sent in fits
/// `ceiling` bytes. `render` turns each node into its wire row;
/// `response_len(rows, has_more, next_cursor)` serializes the response a
/// candidate page would produce, so the envelope (`total`, the cursor) is
/// measured with the rows, never estimated ([`fit_budget_within`]).
///
/// A page whose response fits is returned with `has_more` and `next_cursor`
/// as the SQL page gave them. A cut page keeps at least one row, even one
/// that alone is over `ceiling` (an empty page with `has_more: true` would
/// never progress), and resumes right after the last row kept: `has_more`
/// becomes true and the cursor is that row's, so no row is skipped or sent
/// twice across pages.
pub fn bound_defines_page<T: Serialize>(
    page: Page<NodeRecord>,
    render: impl Fn(NodeRecord) -> T,
    mut response_len: impl FnMut(&[T], bool, Option<&str>) -> usize,
    ceiling: usize,
) -> Page<T> {
    let Page { results: nodes, has_more, next_cursor, all_unresolved } = page;
    let cursors: Vec<String> = nodes.iter().map(source_order_cursor).collect();
    let mut items: Vec<T> = nodes.into_iter().map(render).collect();

    let cut_at = |items: &[T], budget: usize| -> (usize, bool, Option<String>) {
        match longest_prefix_fitting(items, budget) {
            None => (items.len(), has_more, next_cursor.clone()),
            Some(cut) => (cut, true, Some(cursors[cut - 1].clone())),
        }
    };

    let budget = fit_budget_within(ceiling, |budget| {
        let (kept, more, cursor) = cut_at(&items, budget);
        (wire_len(&items[..kept]), response_len(&items[..kept], more, cursor.as_deref()))
    });
    let (kept, more, cursor) = cut_at(&items, budget);
    items.truncate(kept);
    Page { results: items, has_more: more, next_cursor: cursor, all_unresolved }
}
