//! Real logic behind the `find_callers`/`find_callees` MCP tools. Same shape
//! as `find_references` - anchor lookup, then `paginate_edges` over one edge
//! kind - except the edge kind is `CALLS` and the direction to walk flips
//! depending on which tool is asking: callers are the `Incoming` `CALLS`
//! edges (who calls the anchor), callees are the `Outgoing` ones (what the
//! anchor calls). Both are single-hop by design; the transitive walk lives in
//! `get_dependencies`, not here.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::daemon::manifest::Capabilities;
use crate::daemon::registry::PathCoverage;
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination::{self, Direction};
use crate::graph::queries;
use crate::storage::index_store::IndexStore;
use crate::storage::write::NodeRecord;

use super::not_indexed::{self, NotIndexed};
use super::query_shapes::QueryShapes;
use super::session_hints::{self, HintKey, SessionHints};
use super::tool_result::{internal_error, success};
use super::unlinked::{self, UnlinkedUsages};
use super::untyped::{self, UntypedReceiverCalls};
use super::{anchor, answer, find_definition, provenance, Answer, SymbolQueryParams};

/// One "other end of a CALLS edge" record, plus whether that edge is
/// `resolved`. Direction-agnostic on purpose: `list_calls` doesn't know
/// whether it's resolving a caller or a callee, only which node id sits at
/// the non-anchor end of each edge. The two `handle_*` functions attach the
/// role-specific field name (`callerSymbolId` vs `calleeSymbolId`) when they
/// serialize.
struct CallSite {
    node: NodeRecord,
    resolved: bool,
    /// The edge's position in `paginate_edges`' order. Carried through so
    /// `handle_callers`/`handle_callees` can rebuild a resumable cursor if
    /// the enriched page needs further truncation to fit
    /// `pagination::MAX_RESPONSE_BYTES`.
    rank: pagination::EdgeRank,
}

/// Paginates the `CALLS` edges incident to `anchor_id` in `direction` and
/// resolves each to the node on the other end. Split out from the `handle_*`
/// functions, like `find_references::list_references`, so tests can drive it
/// with a small `page_size` directly.
fn list_calls(
    conn: &Connection,
    anchor_id: &str,
    anchor_file_path: &str,
    file_paths: &[&str],
    direction: Direction,
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<CallSite>> {
    let page = pagination::paginate_edges(
        conn,
        anchor_id,
        direction,
        &["CALLS"],
        file_paths,
        anchor_file_path,
        // One row per call site, not per calling symbol - see `Distinctness`.
        pagination::Distinctness::Edges,
        page_size,
        cursor,
    )
    .context("failed to paginate CALLS edges")?;

    let mut results = Vec::with_capacity(page.results.len());
    for pagination::ScoredEdge { edge, rank } in page.results {
        // Outgoing: anchor is fromId, the callee sits at toId. Incoming: anchor
        // is toId, the caller sits at fromId.
        let other_id = match direction {
            Direction::Outgoing => &edge.to_id,
            Direction::Incoming => &edge.from_id,
        };
        let node = queries::get_node(conn, other_id)
            .context("failed to resolve call-edge endpoint")?
            .with_context(|| format!("edge {} points at missing node {other_id}", edge.id))?;
        results.push(CallSite { node, resolved: edge.resolved, rank });
    }

    // Intermediate page, ahead of the EdgeRow/bound_page step each handle_*
    // does next - that step computes the real `all_unresolved` marker from
    // each row's `resolved` bit, so this one is a placeholder nothing reads.
    Ok(pagination::Page {
        results,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        all_unresolved: false,
    })
}

/// Wire shape for `find_callers`: the calling function on the other end of
/// each inbound `CALLS` edge.
///
/// No `name` field: it never carries information `qualifiedName` doesn't
/// already have. `qualifiedName`/`startLine`/`startCol` are `None` - omitted
/// from the wire JSON entirely, never emitted as `null` or `0` - exactly
/// when `kind` is [`pagination::FILE_KIND`]; see that constant's doc comment
/// for why both are pure redundancy on a `File`-kind row.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerSite {
    caller_symbol_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    kind: String,
    file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_col: Option<i64>,
    resolved: bool,
}

impl From<CallSite> for CallerSite {
    fn from(site: CallSite) -> Self {
        let is_file = site.node.kind == pagination::FILE_KIND;
        CallerSite {
            caller_symbol_id: site.node.id,
            qualified_name: (!is_file).then_some(site.node.qualified_name),
            kind: site.node.kind,
            file_path: site.node.file_path,
            start_line: (!is_file).then_some(site.node.start_line),
            start_col: (!is_file).then_some(site.node.start_col),
            resolved: site.resolved,
        }
    }
}

/// Wire shape for `find_callees`: the called function on the other end of
/// each outbound `CALLS` edge. Same `name`/`qualifiedName`/`startLine`/
/// `startCol` rules as [`CallerSite`].
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleeSite {
    callee_symbol_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    kind: String,
    file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_col: Option<i64>,
    resolved: bool,
}

impl From<CallSite> for CalleeSite {
    fn from(site: CallSite) -> Self {
        let is_file = site.node.kind == pagination::FILE_KIND;
        CalleeSite {
            callee_symbol_id: site.node.id,
            qualified_name: (!is_file).then_some(site.node.qualified_name),
            kind: site.node.kind,
            file_path: site.node.file_path,
            start_line: (!is_file).then_some(site.node.start_line),
            start_col: (!is_file).then_some(site.node.start_col),
            resolved: site.resolved,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerPage<'a> {
    /// See `anchor::AnchorInfo` - what `symbol_id`/`symbol_name` resolved to,
    /// so a caller asking about usages "elsewhere" doesn't need a separate
    /// `find_definition` call just to learn the anchor's own file/line.
    anchor: &'a anchor::AnchorInfo,
    results: &'a [CallerSite],
    /// Every file holding a call to the anchor, with a per-file count, over
    /// the whole edge set rather than this page - see
    /// `pagination::tally_edge_files`, and `find_references`' identically
    /// gated field for when it appears and when it stays off the wire.
    ///
    /// `find_callees` has no counterpart on purpose. "Which files does this
    /// function reach" is not a question anybody asks before an edit; the
    /// impact questions that want a file-level answer - a rename, a signature
    /// change, a removal - all run *inbound*, so a callee-side tally would be
    /// payload on every response for a question the tool never gets.
    #[serde(skip_serializing_if = "Option::is_none")]
    files: Option<&'a [pagination::FileTally]>,
    /// Present only when `files` leaves files out, cut by its entry or byte
    /// cap (`pagination::tally_edge_files_bounded`); absent, not `false`,
    /// otherwise.
    #[serde(skip_serializing_if = "answer::is_false")]
    files_truncated: bool,
    /// Exact number of callers over the whole set. Present only when
    /// `has_more`: on a complete page it is `results.len()`.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
    has_more: bool,
    next_cursor: Option<&'a str>,
    /// See `Page::all_unresolved` - true when every caller in `results` came
    /// from an edge the linker couldn't confirm.
    all_unresolved: bool,
    /// `anchor::file_anchor_hint`, then the `super::session_hints` sentences
    /// this page's fields trigger; absent (not `null`) when none applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    /// See [`ExcludedReferences`] - absent, not zero, when the walk left
    /// nothing behind.
    #[serde(skip_serializing_if = "Option::is_none")]
    excluded_references: Option<ExcludedReferences>,
    /// See [`UnlinkedUsages`] - calls that may target the anchor but that the
    /// linker left on a placeholder. Absent when there is no candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    unlinked_usages: Option<&'a UnlinkedUsages>,
    /// See [`UntypedReceiverCalls`] - functions calling a method of the
    /// anchor's name through a receiver whose type was not inferred, with no
    /// edge to the anchor yet. Absent when there is none.
    #[serde(skip_serializing_if = "Option::is_none")]
    untyped_receiver_calls: Option<&'a UntypedReceiverCalls>,
    /// See `super::provenance` - present only when the anchor's language
    /// declares a semantic tier that has not completed for this project, so
    /// this answer came from its structural tier alone. Absent (not `null`,
    /// not an "everything is fine" object) on every healthy response, which
    /// is nearly all of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<provenance::Provenance>,
    /// See `super::not_indexed::group` - the `file_paths` entries in a
    /// language with no indexed files (plugin absent or failed), one entry
    /// per language. Absent when the filter is omitted or fully covered.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    not_indexed: &'a [NotIndexed],
}

/// What a `CALLS` walk left behind, disclosed at the response level.
///
/// `find_callers`/`find_callees` walk `CALLS` edges only, and a `CALLS` edge
/// exists only where the call site sits lexically inside a named, tracked
/// function. A call written at a file's top level, or inside an anonymous
/// callback that is not itself a tracked symbol - `it("...", () => { f() })` in
/// a test file is the canonical shape - produces a `REFERENCES` edge instead,
/// which this walk never sees.
///
/// That is by design and documented, but the documentation lives in the
/// caller's prompt while the *answer* says `hasMore: false` and nothing else.
/// A page that is complete for the question it answers still reads as complete
/// for the question that was asked, and a benchmark run caught exactly that:
/// a `find_callers` page on `releaseTask` looked whole while omitting three
/// test files, and the agent went and grepped them up itself.
///
/// Rows never travel; a *file tally* does. Sending the rows would turn this
/// tool into `find_references` and double its payload for a question it was
/// not asked - the original argument, and it still holds. But a bare count
/// turned out to buy the caller nothing, because what it does with the count
/// is go and ask which files (GM-258): across 200 measured benchmark runs
/// `find_callers` was called 44 times, 28 of those responses carried this
/// disclosure, and seven of the follow-ups were a whole extra turn spent
/// re-asking the same anchor through `find_references` purely to turn "2
/// excluded" into two paths. From one such trace, verbatim, between the two
/// calls: *"it excludes 2 non-CALLS edges … Let me check find_references for
/// full coverage"* - and the answer it then wrote was a file list.
///
/// So `files` carries the names, at ~40 bytes an entry against a reference
/// row's ~260, capped by `pagination::MAX_EXCLUDED_FILE_TALLY` entries and
/// `pagination::EXCLUDED_TALLY_MAX_BYTES` bytes. `count` stays
/// exact and uncapped even when the tally is cut, since understating the gap
/// is the one thing this field must never do; `files_truncated` says so
/// explicitly rather than leaving the caller to notice that the tally sums to
/// less than the count.
///
/// The field is absent, not zero, when nothing was excluded: the narrow lookup
/// that is most of the traffic must not grow bytes to announce that nothing
/// was hidden, the same rule `files` follows.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExcludedReferences {
    count: usize,
    /// The files holding one of the excluded usages that the rest of the
    /// response does not already name, highest count first, capped at
    /// `pagination::MAX_EXCLUDED_FILE_TALLY` before that filtering and at
    /// `pagination::EXCLUDED_TALLY_MAX_BYTES` after it. Every file holding an
    /// excluded usage is named once somewhere in the response, up to those
    /// caps. Absent when empty, and always on `answer: "count"`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    files: Vec<pagination::FileTally>,
    /// Present only when a cap cut the tally - absent, not `false`, in the
    /// ordinary case, so a disclosure on a small anchor stays small.
    #[serde(skip_serializing_if = "answer::is_false")]
    files_truncated: bool,
    hint: &'static str,
}

const EXCLUDED_REFERENCES_HINT: &str =
    "Usages that are not CALLS edges were excluded - a call at file top level or inside an \
     anonymous callback (e.g. a test's it(...) body) is one of these. This response names every \
     file holding one (`files` here lists those not named elsewhere), so a file-level answer is \
     already complete; call find_references only if you need the calling symbol or its line.";

const EXCLUDED_COUNT_HINT: &str =
    "Usages that are not CALLS edges (a call at file top level or inside an anonymous callback) \
     are not in `total`; `count` here is how many.";

/// Counts the `REFERENCES`-kind edges this `CALLS` walk excluded, or `None`
/// when there were none, and names their files (none with `answer` set to
/// [`Answer::Count`]); [`ExcludedReferences::naming_only_new`] then drops the
/// files the response already names elsewhere. Errors are swallowed to
/// `None` on purpose: this is a disclosure attached to an answer that already
/// succeeded, and failing the whole call because the footnote could not be
/// computed would trade a good answer for no answer.
fn excluded_references(
    conn: &Connection,
    anchor_id: &str,
    direction: Direction,
    file_paths: &[&str],
    answer: Answer,
) -> Option<ExcludedReferences> {
    let count = pagination::count_edges(conn, anchor_id, direction, &["REFERENCES"], file_paths).ok()?;
    if count == 0 {
        return None;
    }
    if answer == Answer::Count {
        return Some(ExcludedReferences {
            count,
            files: Vec::new(),
            files_truncated: false,
            hint: EXCLUDED_COUNT_HINT,
        });
    }
    // A second query on the same predicate rather than deriving the count from
    // the tally's own sum: the tally is capped and the count must not be, and
    // reconstructing an uncapped total from a capped `GROUP BY` is exactly the
    // understatement this disclosure exists to prevent.
    let files = pagination::tally_edge_files_limited(
        conn,
        anchor_id,
        direction,
        &["REFERENCES"],
        file_paths,
        pagination::MAX_EXCLUDED_FILE_TALLY,
    )
    .unwrap_or_default();
    let files_truncated = files.len() >= pagination::MAX_EXCLUDED_FILE_TALLY;
    Some(ExcludedReferences { count, files, files_truncated, hint: EXCLUDED_REFERENCES_HINT })
}

impl ExcludedReferences {
    /// This disclosure for a response that already names `named`: those
    /// files dropped from `files`, and the rest capped at
    /// `pagination::EXCLUDED_TALLY_MAX_BYTES`.
    fn naming_only_new(&self, named: &HashSet<&str>) -> ExcludedReferences {
        let mut files: Vec<pagination::FileTally> =
            self.files.iter().filter(|tally| !named.contains(tally.path.as_str())).cloned().collect();
        let cut = pagination::truncate_to_bytes(&mut files, pagination::EXCLUDED_TALLY_MAX_BYTES);
        ExcludedReferences { files, files_truncated: self.files_truncated || cut, ..self.clone() }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleePage<'a> {
    /// See `anchor::AnchorInfo`, same rationale as `CallerPage`'s own field.
    anchor: &'a anchor::AnchorInfo,
    results: &'a [CalleeSite],
    /// Exact number of callees over the whole set. Present only when
    /// `has_more`: on a complete page it is `results.len()`.
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
    has_more: bool,
    next_cursor: Option<&'a str>,
    /// See `Page::all_unresolved` - true when every callee in `results` came
    /// from an edge the linker couldn't confirm.
    all_unresolved: bool,
    /// `anchor::file_anchor_hint`, then `session_hints::ALL_UNRESOLVED` when
    /// `all_unresolved`, then the once-per-session sentences its rows and
    /// `provenance` trigger; absent (not `null`) when none applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
    /// See [`ExcludedReferences`] - absent, not zero, when the walk left
    /// nothing behind.
    #[serde(skip_serializing_if = "Option::is_none")]
    excluded_references: Option<ExcludedReferences>,
    /// See `super::provenance` - present only when the anchor's language
    /// declares a semantic tier that has not completed for this project, so
    /// this answer came from its structural tier alone. Absent (not `null`,
    /// not an "everything is fine" object) on every healthy response, which
    /// is nearly all of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<provenance::Provenance>,
    /// See `super::not_indexed::group` - the `file_paths` entries in a
    /// language with no indexed files (plugin absent or failed), one entry
    /// per language. Absent when the filter is omitted or fully covered.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    not_indexed: &'a [NotIndexed],
}

/// The response-level disclosures of [`CallerPage`], carried unchanged by the
/// non-row answers (`answer::Summary`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerDisclosures<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    excluded_references: Option<ExcludedReferences>,
    #[serde(skip_serializing_if = "Option::is_none")]
    unlinked_usages: Option<&'a UnlinkedUsages>,
    #[serde(skip_serializing_if = "Option::is_none")]
    untyped_receiver_calls: Option<&'a UntypedReceiverCalls>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<provenance::Provenance>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    not_indexed: &'a [NotIndexed],
}

/// The response-level disclosures of [`CalleePage`], as [`CallerDisclosures`].
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleeDisclosures<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    excluded_references: Option<ExcludedReferences>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<provenance::Provenance>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    not_indexed: &'a [NotIndexed],
}

/// Every path an excluded-references tally names, for `touched` sets.
fn excluded_paths(excluded: &Option<ExcludedReferences>) -> impl Iterator<Item = &str> {
    excluded.iter().flat_map(|excluded| excluded.files.iter().map(|tally| tally.path.as_str()))
}

pub(crate) fn handle_callers(
    store: &Arc<IndexStore>,
    embedding: &EmbeddingPipeline,
    shapes: &QueryShapes,
    capabilities: &HashMap<String, Capabilities>,
    hints: &SessionHints,
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    find_definition::resolve_lazily(embedding, shapes, |semantic| {
        handle_callers_in(store, semantic, capabilities, hints, params.clone())
    })
}

/// Everything a caller page carries besides its rows, fetched once so that
/// candidate pages can be measured whole before one is sent.
struct CallerParts<'a> {
    conn: &'a Connection,
    anchor: &'a NodeRecord,
    anchor_info: anchor::AnchorInfo,
    anchor_hint: Option<&'static str>,
    tally: Vec<pagination::FileTally>,
    tally_truncated: bool,
    excluded: Option<ExcludedReferences>,
    unlinked: Option<UnlinkedUsages>,
    untyped: Option<UntypedReceiverCalls>,
    tier: provenance::Resolved,
    hints: &'a SessionHints,
    not_indexed: &'a [NotIndexed],
}

impl CallerParts<'_> {
    /// The response for `page`. `send` spends once-per-session hints; without
    /// it they are only peeked, for measuring.
    fn response<'p>(
        &'p self,
        page: &'p pagination::Page<CallerSite>,
        total: Option<usize>,
        send: bool,
    ) -> CallerPage<'p> {
        let files = pagination::tally_is_worth_sending(page.results.len(), &self.tally, page.has_more)
            .then_some(self.tally.as_slice());
        let named: HashSet<&str> = page
            .results
            .iter()
            .map(|row| row.file_path.as_str())
            .chain(files.into_iter().flatten().map(|tally| tally.path.as_str()))
            .collect();
        let excluded = self.excluded.as_ref().map(|excluded| excluded.naming_only_new(&named));

        // Every file this response names: rows, the tally, the excluded,
        // unlinked and untyped tallies.
        let touched = named
            .iter()
            .copied()
            .chain(excluded_paths(&excluded))
            .chain(self.unlinked.iter().flat_map(UnlinkedUsages::file_paths))
            .chain(self.untyped.iter().flat_map(UntypedReceiverCalls::file_paths));
        let provenance = self.tier.clone().disclose(
            self.conn,
            &self.anchor.language,
            Some(&self.anchor.file_path),
            touched,
        );
        let has_file_row = page.results.iter().any(|row| row.kind == pagination::FILE_KIND);
        let once = |trigger, key, sentence| self.hints.offer(send, trigger, key, sentence);
        let hint = session_hints::join([
            self.anchor_hint,
            page.all_unresolved.then_some(session_hints::ALL_UNRESOLVED),
            once(has_file_row, HintKey::FileRow, session_hints::FILE_ROW),
            once(files.is_some(), HintKey::FilesTally, session_hints::FILES_TALLY),
            once(
                !page.all_unresolved && page.results.iter().any(|row| !row.resolved),
                HintKey::UnresolvedRow,
                session_hints::UNRESOLVED_ROW,
            ),
            once(provenance.is_some(), HintKey::SemanticTier, session_hints::PROVENANCE),
        ]);

        CallerPage {
            anchor: &self.anchor_info,
            results: &page.results,
            files,
            files_truncated: files.is_some() && self.tally_truncated,
            total,
            has_more: page.has_more,
            next_cursor: page.next_cursor.as_deref(),
            all_unresolved: page.all_unresolved,
            hint,
            excluded_references: excluded,
            unlinked_usages: self.unlinked.as_ref(),
            untyped_receiver_calls: self.untyped.as_ref(),
            provenance,
            not_indexed: self.not_indexed,
        }
    }
}

/// One pass of [`handle_callers`] - see [`find_definition::SemanticRung`].
pub(crate) fn handle_callers_in(
    store: &Arc<IndexStore>,
    semantic: &find_definition::SemanticRung<'_>,
    capabilities: &HashMap<String, Capabilities>,
    hints: &SessionHints,
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    handle_callers_in_covered(store, semantic, capabilities, hints, &[], params)
}

/// [`handle_callers_in`], with `uncovered` the `file_paths` entries whose language is
/// not indexed (`GMeshMcpServer::filter_coverage`): the answer names them in
/// `notIndexed` (`not_indexed::group`).
pub(crate) fn handle_callers_in_covered(
    store: &Arc<IndexStore>,
    semantic: &find_definition::SemanticRung<'_>,
    capabilities: &HashMap<String, Capabilities>,
    hints: &SessionHints,
    uncovered: &[(String, PathCoverage)],
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = store.read();

    let resolved = match anchor::resolve(&conn, semantic, &params)? {
        Ok(resolved) => resolved,
        Err(finished) => return Ok(finished),
    };
    // Destructured here so everything below still reads the node directly,
    // while `resolved.by` stays available for the response's `resolvedBy`.
    let resolved_by = resolved.by;
    let queried_as = resolved.queried_as;
    let anchor = resolved.node;
    let hint = anchor::file_anchor_hint(&anchor);
    let anchor_info = anchor::AnchorInfo::with_rung(&anchor, resolved_by, queried_as);

    let page_size = pagination::resolve_page_size(params.limit);
    let file_paths: Vec<&str> = params.file_paths.iter().flatten().map(String::as_str).collect();
    let tier = provenance::resolve(&conn, capabilities, &anchor.language);
    let unlinked = unlinked::probe(&conn, &anchor, &["CALLS"], &file_paths);
    let untyped = untyped::probe(&conn, &anchor, &["CALLS"], &file_paths);
    let answer = params.answer.unwrap_or_default();
    let excluded = excluded_references(&conn, &anchor.id, Direction::Incoming, &file_paths, answer);
    let not_indexed = not_indexed::group(&conn, uncovered)?;

    let counted = answer::count(&conn, answer, &anchor.id, Direction::Incoming, &["CALLS"], &file_paths)
        .map_err(|e| internal_error("failed to count callers", e))?;
    if let Some(counted) = counted {
        return answer::respond(&anchor_info, &counted, |files, send| {
            let named: HashSet<&str> = files.into_iter().flatten().map(|tally| tally.path.as_str()).collect();
            let excluded = excluded.as_ref().map(|excluded| excluded.naming_only_new(&named));
            let touched = named
                .iter()
                .copied()
                .chain(excluded_paths(&excluded))
                .chain(unlinked.iter().flat_map(UnlinkedUsages::file_paths))
                .chain(untyped.iter().flat_map(UntypedReceiverCalls::file_paths));
            let provenance = tier.clone().disclose(&conn, &anchor.language, Some(&anchor.file_path), touched);
            let hint = session_hints::join([
                hint,
                hints.offer(send, provenance.is_some(), HintKey::SemanticTier, session_hints::PROVENANCE),
            ]);
            let disclosures = CallerDisclosures {
                excluded_references: excluded,
                unlinked_usages: unlinked.as_ref(),
                untyped_receiver_calls: untyped.as_ref(),
                provenance,
                not_indexed: &not_indexed,
            };
            (hint, disclosures)
        });
    }

    let page = list_calls(
        &conn,
        &anchor.id,
        &anchor.file_path,
        &file_paths,
        Direction::Incoming,
        page_size,
        params.cursor.as_deref(),
    )
    .map_err(|e| internal_error("failed to find callers", e))?;
    let (tally, tally_truncated) =
        pagination::tally_edge_files_bounded(&conn, &anchor.id, Direction::Incoming, &["CALLS"], &file_paths)
            .map_err(|e| internal_error("failed to tally calling files", e))?;
    let parts = CallerParts {
        conn: &conn,
        anchor: &anchor,
        anchor_info,
        anchor_hint: hint,
        tally,
        tally_truncated,
        excluded,
        unlinked,
        untyped,
        tier,
        hints,
        not_indexed: &not_indexed,
    };

    let rows = page
        .results
        .into_iter()
        .map(|site| {
            let rank = site.rank.clone();
            pagination::EdgeRow { item: CallerSite::from(site), rank }
        })
        .collect();
    let bounded = pagination::bound_page_in_response(rows, page.has_more, page.next_cursor, |candidate| {
        pagination::wire_len(&parts.response(candidate, pagination::widest_total(candidate.has_more), false))
    });
    let total = bounded
        .has_more
        .then(|| pagination::count_edges(&conn, &anchor.id, Direction::Incoming, &["CALLS"], &file_paths))
        .transpose()
        .map_err(|e| internal_error("failed to count callers", e))?;

    success(&parts.response(&bounded, total, true))
}

#[cfg(test)]
pub(crate) fn handle_callees(
    store: &Arc<IndexStore>,
    embedding: &EmbeddingPipeline,
    shapes: &QueryShapes,
    capabilities: &HashMap<String, Capabilities>,
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    let hints = SessionHints::default();
    find_definition::resolve_lazily(embedding, shapes, |semantic| {
        handle_callees_in(store, semantic, capabilities, &hints, params.clone())
    })
}

/// Everything a callee page carries besides its rows; see [`CallerParts`].
struct CalleeParts<'a> {
    conn: &'a Connection,
    anchor: &'a NodeRecord,
    anchor_info: anchor::AnchorInfo,
    anchor_hint: Option<&'static str>,
    excluded: Option<ExcludedReferences>,
    tier: provenance::Resolved,
    hints: &'a SessionHints,
    not_indexed: &'a [NotIndexed],
}

impl CalleeParts<'_> {
    /// The response for `page`, as [`CallerParts::response`].
    fn response<'p>(
        &'p self,
        page: &'p pagination::Page<CalleeSite>,
        total: Option<usize>,
        send: bool,
    ) -> CalleePage<'p> {
        let named: HashSet<&str> = page.results.iter().map(|row| row.file_path.as_str()).collect();
        let excluded = self.excluded.as_ref().map(|excluded| excluded.naming_only_new(&named));

        // Every file this response names: rows and the excluded tally.
        let touched = named.iter().copied().chain(excluded_paths(&excluded));
        let provenance = self.tier.clone().disclose(
            self.conn,
            &self.anchor.language,
            Some(&self.anchor.file_path),
            touched,
        );
        let once = |trigger, key, sentence| self.hints.offer(send, trigger, key, sentence);
        let hint = session_hints::join([
            self.anchor_hint,
            page.all_unresolved.then_some(session_hints::ALL_UNRESOLVED),
            once(
                !page.all_unresolved && page.results.iter().any(|row| !row.resolved),
                HintKey::UnresolvedRow,
                session_hints::UNRESOLVED_ROW,
            ),
            once(provenance.is_some(), HintKey::SemanticTier, session_hints::PROVENANCE),
        ]);

        CalleePage {
            anchor: &self.anchor_info,
            results: &page.results,
            total,
            has_more: page.has_more,
            next_cursor: page.next_cursor.as_deref(),
            all_unresolved: page.all_unresolved,
            hint,
            excluded_references: excluded,
            provenance,
            not_indexed: self.not_indexed,
        }
    }
}

/// One pass of [`handle_callees`] - see [`find_definition::SemanticRung`].
/// Tests only: the server calls [`handle_callees_in_covered`].
#[cfg(test)]
pub(crate) fn handle_callees_in(
    store: &Arc<IndexStore>,
    semantic: &find_definition::SemanticRung<'_>,
    capabilities: &HashMap<String, Capabilities>,
    hints: &SessionHints,
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    handle_callees_in_covered(store, semantic, capabilities, hints, &[], params)
}

/// `handle_callees_in`, with `uncovered` the `file_paths` entries whose language is
/// not indexed (`GMeshMcpServer::filter_coverage`): the answer names them in
/// `notIndexed` (`not_indexed::group`).
pub(crate) fn handle_callees_in_covered(
    store: &Arc<IndexStore>,
    semantic: &find_definition::SemanticRung<'_>,
    capabilities: &HashMap<String, Capabilities>,
    hints: &SessionHints,
    uncovered: &[(String, PathCoverage)],
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = store.read();

    let resolved = match anchor::resolve(&conn, semantic, &params)? {
        Ok(resolved) => resolved,
        Err(finished) => return Ok(finished),
    };
    // Destructured here so everything below still reads the node directly,
    // while `resolved.by` stays available for the response's `resolvedBy`.
    let resolved_by = resolved.by;
    let queried_as = resolved.queried_as;
    let anchor = resolved.node;
    let hint = anchor::file_anchor_hint(&anchor);
    let anchor_info = anchor::AnchorInfo::with_rung(&anchor, resolved_by, queried_as);

    let page_size = pagination::resolve_page_size(params.limit);
    let file_paths: Vec<&str> = params.file_paths.iter().flatten().map(String::as_str).collect();
    let tier = provenance::resolve(&conn, capabilities, &anchor.language);
    let answer = params.answer.unwrap_or_default();
    let excluded = excluded_references(&conn, &anchor.id, Direction::Outgoing, &file_paths, answer);
    let not_indexed = not_indexed::group(&conn, uncovered)?;

    let counted = answer::count(&conn, answer, &anchor.id, Direction::Outgoing, &["CALLS"], &file_paths)
        .map_err(|e| internal_error("failed to count callees", e))?;
    if let Some(counted) = counted {
        return answer::respond(&anchor_info, &counted, |files, send| {
            let named: HashSet<&str> = files.into_iter().flatten().map(|tally| tally.path.as_str()).collect();
            let excluded = excluded.as_ref().map(|excluded| excluded.naming_only_new(&named));
            let touched = named.iter().copied().chain(excluded_paths(&excluded));
            let provenance = tier.clone().disclose(&conn, &anchor.language, Some(&anchor.file_path), touched);
            let hint = session_hints::join([
                hint,
                hints.offer(send, provenance.is_some(), HintKey::SemanticTier, session_hints::PROVENANCE),
            ]);
            (hint, CalleeDisclosures { excluded_references: excluded, provenance, not_indexed: &not_indexed })
        });
    }

    let page = list_calls(
        &conn,
        &anchor.id,
        &anchor.file_path,
        &file_paths,
        Direction::Outgoing,
        page_size,
        params.cursor.as_deref(),
    )
    .map_err(|e| internal_error("failed to find callees", e))?;
    let parts = CalleeParts {
        conn: &conn,
        anchor: &anchor,
        anchor_info,
        anchor_hint: hint,
        excluded,
        tier,
        hints,
        not_indexed: &not_indexed,
    };

    let rows = page
        .results
        .into_iter()
        .map(|site| {
            let rank = site.rank.clone();
            pagination::EdgeRow { item: CalleeSite::from(site), rank }
        })
        .collect();
    let bounded = pagination::bound_page_in_response(rows, page.has_more, page.next_cursor, |candidate| {
        pagination::wire_len(&parts.response(candidate, pagination::widest_total(candidate.has_more), false))
    });
    let total = bounded
        .has_more
        .then(|| pagination::count_edges(&conn, &anchor.id, Direction::Outgoing, &["CALLS"], &file_paths))
        .transpose()
        .map_err(|e| internal_error("failed to count callees", e))?;

    success(&parts.response(&bounded, total, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::EdgeRecord;

    /// The capability map every test in this module passes: empty, so
    /// `provenance::resolve` reads "no plugin here declares a semantic
    /// tier" and these responses stay byte-for-byte what they were before
    /// GM-382 added the field. The tests that are *about* the field build
    /// their own map; see `mcp::provenance`'s own tests for the predicate
    /// itself.
    fn no_capabilities() -> HashMap<String, Capabilities> {
        HashMap::new()
    }

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn json_body(result: &CallToolResult) -> serde_json::Value {
        assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
            other => panic!("expected text/json content, got {other:?}"),
        }
    }

    fn error_text(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected an error result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// Sets up the acceptance criteria's chain: A calls B, B calls C.
    fn setup_chain() -> Connection {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_ab", "a", "b", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_bc", "b", "c", "CALLS", "tree-sitter", true)).unwrap();
        conn
    }

    /// GM-382, the arm this whole task exists for, at the wire level rather
    /// than at `provenance::resolve`'s: a language that declares a semantic
    /// tier, an index in which that tier has not run, and the real
    /// `find_callers` handler - the response must say so.
    ///
    /// The index here is deliberately a *healthy-looking* one: one resolved
    /// `CALLS` edge, `hasMore: false`, `allUnresolved: false`. That is the
    /// exact shape the shipped guidance tells a caller to trust without
    /// re-checking, and before this field existed it was also the exact
    /// shape a Rust page produced with no `rust-analyzer` installed. The
    /// only thing distinguishing the two is the block asserted below.
    #[test]
    fn a_declared_semantic_tier_that_never_ran_is_disclosed_on_the_wire() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };

        let result = handle_callers(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &rust_with_a_semantic_tier(),
            &SessionHints::default(),
            params,
        )
        .unwrap();

        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 1, "the page still answers: {body}");
        assert_eq!(body["hasMore"], false, "and still looks complete: {body}");
        assert_eq!(
            body["provenance"],
            serde_json::json!({ "language": "rust", "semanticTier": "absent" }),
            "the page must disclose which plugin answered and that its semantic tier did not: {body}"
        );
    }

    /// The control for the test above, and the only difference between them
    /// is the one variable: the same fixture, the same query, the same
    /// capability map, with `rust`'s whole-project semantic pass recorded.
    /// The disclosure has to disappear.
    ///
    /// Without this pair the test above proves only that a field can be
    /// emitted, not that it *discriminates* - a block hard-coded onto every
    /// Rust response would pass it and would be worthless.
    #[test]
    fn a_completed_semantic_pass_leaves_the_wire_shape_untouched() {
        let conn = setup_chain();
        schema::record_language_semantic_pass(&conn, "rust").unwrap();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };

        let result = handle_callers(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &rust_with_a_semantic_tier(),
            &SessionHints::default(),
            params,
        )
        .unwrap();

        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 1, "the same answer: {body}");
        assert!(
            body.get("provenance").is_none(),
            "a healthy page must carry no provenance key at all - not `null`, not an \
             everything-is-fine object: {body}"
        );
    }

    /// `find_callees` is the same response envelope and must not have been
    /// wired up differently by hand - the four edge-walking tools are
    /// exactly the set that carries this, and a tool missing it would be a
    /// silent hole in precisely the direction GM-382 is about.
    #[test]
    fn find_callees_discloses_the_absent_tier_too() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };

        let result = handle_callees(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &rust_with_a_semantic_tier(),
            params,
        )
        .unwrap();

        assert_eq!(
            json_body(&result)["provenance"],
            serde_json::json!({ "language": "rust", "semanticTier": "absent" })
        );
    }

    /// A capability map declaring rust's real shipped shape: it has a
    /// semantic tier, and that tier is what resolves receiver calls. The
    /// fixture's nodes are all `"rust"` (see `setup_chain`), so this is the
    /// language every anchor in this module resolves to.
    fn rust_with_a_semantic_tier() -> HashMap<String, Capabilities> {
        HashMap::from([(
            "rust".to_string(),
            Capabilities {
                semantic_pass: true,
                semantic_sweep: false,
                semantic_prepare: false,
                files_created: false,
                receiver_calls: crate::daemon::manifest::ReceiverCallResolution::Resolved,
                receiver_calls_structural: crate::daemon::manifest::ReceiverCallResolution::Unresolved,
            },
        )])
    }

    #[test]
    fn find_callers_of_b_returns_exactly_a_not_c_not_itself() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };
        let result = handle_callers(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            &SessionHints::default(),
            params,
        )
        .unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "B has exactly one caller, not the transitive chain");
        assert_eq!(results[0]["callerSymbolId"], "a");
    }

    #[test]
    fn find_callees_of_b_returns_exactly_c_not_a() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };
        let result = handle_callees(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            params,
        )
        .unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "B has exactly one callee, not the transitive chain");
        assert_eq!(results[0]["calleeSymbolId"], "c");
    }

    /// A top-level call with no enclosing function attributes to the `File`
    /// node itself - the shape that motivated trimming `name` and omitting
    /// `qualifiedName`/`startLine`/`startCol` from `File`-kind rows: they
    /// duplicate `filePath` (`qualifiedName`) or are meaningless
    /// (`startLine`/`startCol`, always the file's own root position). Both
    /// must be entirely absent from the JSON. Symbol-kind rows keep both,
    /// unaffected.
    #[test]
    fn a_file_kind_caller_row_omits_qualified_name_and_position_but_keeps_them_for_symbol_kind_rows() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(
            &mut conn,
            NodeRecord::new("file", "File", "main.rs", "src/main.rs", "src/main.rs", "rust"),
        )
        .unwrap();
        upsert_node(
            &mut conn,
            NodeRecord::new("caller", "Function", "caller", "pkg::caller", "caller.rs", "rust"),
        )
        .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_file", "file", "target", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_caller", "caller", "target", "CALLS", "tree-sitter", true))
            .unwrap();
        let conn = Arc::new(IndexStore::new(conn));

        let body = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);

        for row in results {
            assert!(row.get("name").is_none(), "the name field must never be present on any row: {row}");
        }

        let file_row =
            results.iter().find(|r| r["kind"] == "File").expect("the File-kind row must be present");
        assert!(
            file_row.get("qualifiedName").is_none(),
            "qualifiedName duplicates filePath for a File-kind row: {file_row}"
        );
        assert!(
            file_row.get("startLine").is_none(),
            "startLine is meaningless for a File-kind row: {file_row}"
        );
        assert!(
            file_row.get("startCol").is_none(),
            "startCol is meaningless for a File-kind row: {file_row}"
        );
        assert_eq!(file_row["filePath"], "src/main.rs");

        let symbol_row =
            results.iter().find(|r| r["kind"] == "Function").expect("the Function-kind row must be present");
        assert_eq!(
            symbol_row["qualifiedName"], "pkg::caller",
            "a symbol-kind row must still carry its qualifiedName"
        );
        assert_eq!(symbol_row["startLine"], 0, "a symbol-kind row must still carry its startLine");
        assert_eq!(symbol_row["startCol"], 0, "a symbol-kind row must still carry its startCol");
    }

    #[test]
    fn callers_of_a_root_is_an_empty_page_not_an_error() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("a".to_string()), ..Default::default() };
        let result = handle_callers(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            &SessionHints::default(),
            params,
        )
        .unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
        assert_eq!(body["allUnresolved"], false, "an empty page has nothing to be suspicious of");
    }

    /// Mirrors `find_references`'s benchmark-shaped repro: a caller list that
    /// looks like a complete answer (non-empty, `hasMore: false`) but is
    /// built entirely from edges the linker couldn't confirm must say so at
    /// the response level, not leave it to be inferred by scanning every
    /// row's own `resolved` bit.
    #[test]
    fn a_page_where_every_caller_is_unresolved_is_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", false))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", false))
            .unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callers(
                &Arc::new(IndexStore::new(conn)),
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(body["allUnresolved"], true, "every caller unresolved must set the response-level marker");
    }

    /// The benchmark shape, from g-mesh-bench GMB-142: `releaseTask` is called
    /// from three test files, each call sitting inside an `it("...", () => {})`
    /// body that is not itself a tracked symbol. Those produce `REFERENCES`
    /// edges, so the `CALLS` walk cannot see them - and before this field the
    /// page said `hasMore: false` and left the caller to discover the gap by
    /// grepping, which is exactly what the measured agent did.
    #[test]
    fn a_caller_page_that_excluded_references_discloses_how_many() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("spec", "File", "spec", "spec", "a.test.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_call", "caller", "target", "CALLS", "tree-sitter", true))
            .unwrap();
        // The call inside the anonymous callback: a REFERENCES edge the walk skips.
        upsert_edge(&mut conn, EdgeRecord::new("e_ref", "spec", "target", "REFERENCES", "tree-sitter", true))
            .unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callers(
                &Arc::new(IndexStore::new(conn)),
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );

        assert_eq!(
            body["results"].as_array().unwrap().len(),
            1,
            "the CALLS walk still returns only the call"
        );
        assert_eq!(body["hasMore"], false);
        assert_eq!(body["excludedReferences"]["count"], 1, "the omitted REFERENCES edge must be disclosed");
        assert_eq!(
            body["excludedReferences"]["files"],
            serde_json::json!([{ "path": "a.test.rs", "refs": 1 }]),
            "GM-258: the file holding the excluded usage travels, since naming it is what the \
             caller otherwise spends a second call to learn",
        );
        assert!(
            body["excludedReferences"]["filesTruncated"].is_null(),
            "an untruncated tally must not pay bytes to say so",
        );
        assert!(
            body["excludedReferences"]["results"].is_null(),
            "rows still never travel - sending them would make this find_references",
        );
    }

    /// GM-258's own bound. `count` is uncapped by design (understating the gap
    /// is the one thing this field must never do), so past the tally cap the
    /// two disagree - and the response has to say which of them was cut rather
    /// than leave the caller to infer it from the arithmetic.
    #[test]
    fn a_tally_cut_by_the_cap_says_so_and_leaves_the_count_exact() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        let over = pagination::MAX_EXCLUDED_FILE_TALLY + 3;
        for i in 0..over {
            let id = format!("spec{i}");
            let path = format!("spec{i}.test.rs");
            upsert_node(&mut conn, NodeRecord::new(&id, "File", &id, &id, &path, "rust")).unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e_ref{i}"), &id, "target", "REFERENCES", "tree-sitter", true),
            )
            .unwrap();
        }

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callers(
                &Arc::new(IndexStore::new(conn)),
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );

        assert_eq!(body["excludedReferences"]["count"], over, "the count stays whole");
        assert_eq!(
            body["excludedReferences"]["files"].as_array().unwrap().len(),
            pagination::MAX_EXCLUDED_FILE_TALLY,
            "the tally, and only the tally, is what the cap cuts",
        );
        assert_eq!(body["excludedReferences"]["filesTruncated"], true);
    }

    /// The callee side carries the identical field and had no `files` tally of
    /// its own to model it on, so its wiring is worth pinning separately.
    #[test]
    fn a_callee_page_names_the_files_it_excluded_too() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("helper", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_call", "target", "callee", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new("e_ref", "target", "helper", "REFERENCES", "tree-sitter", true),
        )
        .unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callees(
                &Arc::new(IndexStore::new(conn)),
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                params,
            )
            .unwrap(),
        );

        assert_eq!(body["excludedReferences"]["count"], 1);
        assert_eq!(body["excludedReferences"]["files"], serde_json::json!([{ "path": "c.rs", "refs": 1 }]));
    }

    /// The narrow lookup that is most of the traffic must not grow bytes to
    /// announce that nothing was hidden. Absent, not zero - the same rule
    /// `files` follows.
    #[test]
    fn a_caller_page_with_nothing_excluded_omits_the_field_entirely() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_call", "caller", "target", "CALLS", "tree-sitter", true))
            .unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let result = handle_callers(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            &SessionHints::default(),
            params,
        )
        .unwrap();
        let raw = json_body(&result).to_string();
        let body = json_body(&result);

        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert!(
            body.get("excludedReferences").is_none(),
            "the field must be absent, not null or zero, when the walk excluded nothing",
        );
        assert!(!raw.contains("excludedReferences"), "and must cost the narrow lookup no bytes at all");
    }

    #[test]
    fn a_page_with_at_least_one_resolved_caller_is_not_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", false))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "CALLS", "tree-sitter", false))
            .unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callers(
                &Arc::new(IndexStore::new(conn)),
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        assert_eq!(body["results"].as_array().unwrap().len(), 3);
        assert_eq!(
            body["allUnresolved"], false,
            "one resolved row among several unresolved ones must clear the marker"
        );
    }

    #[test]
    fn callees_of_a_leaf_is_an_empty_page_not_an_error() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("c".to_string()), ..Default::default() };
        let result = handle_callees(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            params,
        )
        .unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    /// Task #190: both `find_callers` and `find_callees` echo the resolved
    /// anchor in their own response, not just `find_references`'s - each
    /// builds its own response struct, so this has to be proven per tool.
    #[test]
    fn both_directions_echo_the_resolved_anchor() {
        let conn = Arc::new(IndexStore::new(setup_chain()));

        let callers = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(callers["anchor"]["id"], "b");
        assert_eq!(callers["anchor"]["qualifiedName"], "pkg::b");
        assert_eq!(callers["anchor"]["kind"], "Function");
        assert_eq!(callers["anchor"]["filePath"], "b.rs");

        let callees = json_body(
            &handle_callees(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(callees["anchor"]["id"], "b");
        assert_eq!(callees["anchor"]["qualifiedName"], "pkg::b");
    }

    /// Both directions must accept the name form: the anchor lookup is
    /// shared, but the two handlers call it separately.
    #[test]
    fn an_unambiguous_symbol_name_anchors_both_directions_without_a_symbol_id() {
        let conn = Arc::new(IndexStore::new(setup_chain()));

        let callers = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_name: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(callers["results"].as_array().unwrap().len(), 1);
        assert_eq!(callers["results"][0]["callerSymbolId"], "a");

        let callees = json_body(
            &handle_callees(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                SymbolQueryParams { symbol_name: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(callees["results"].as_array().unwrap().len(), 1);
        assert_eq!(callees["results"][0]["calleeSymbolId"], "c");
    }

    /// Two same-named functions with disjoint callers - the shape of
    /// `getNonDeletedElements` in the excalidraw corpus, which has three
    /// declarations. The tool must hand back the choice, never make it: no
    /// guessed winner, and no union of both candidates' callers either.
    /// Picking one and re-asking with its `id` still costs two calls total,
    /// exactly what the old mandatory `find_definition` step cost.
    #[test]
    fn an_ambiguous_symbol_name_returns_candidates_instead_of_walking_either() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("run_a", "Function", "run", "pkg_a::run", "a.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("run_b", "Function", "run", "pkg_b::run", "b.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "ca", "pkg::ca", "ca.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "cb", "pkg::cb", "cb.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "run_a", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "run_b", "CALLS", "tree-sitter", true))
            .unwrap();
        let conn = Arc::new(IndexStore::new(conn));

        let ambiguous = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(
            ambiguous["ambiguous"], true,
            "the candidate page must be distinguishable from a results page"
        );
        let candidates = ambiguous["results"].as_array().unwrap();
        let mut names: Vec<&str> = candidates.iter().map(|c| c["qualifiedName"].as_str().unwrap()).collect();
        names.sort();
        assert_eq!(names, vec!["pkg_a::run", "pkg_b::run"], "both declarations must be offered");
        assert!(
            candidates.iter().all(|c| c.get("callerSymbolId").is_none()),
            "no walk may have run: these are candidates to choose from, not callers"
        );

        // The follow-up the caller is expected to make - and it must answer
        // for the one symbol it picked, not for both.
        let picked =
            candidates.iter().find(|c| c["qualifiedName"] == "pkg_b::run").expect("candidate pkg_b::run");
        let params = SymbolQueryParams {
            symbol_id: Some(picked["id"].as_str().expect("a candidate carries its id").to_string()),
            ..Default::default()
        };
        let body = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        assert!(body.get("ambiguous").is_none(), "an id is never ambiguous");
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "only the chosen candidate's callers, never a union across candidates");
        assert_eq!(results[0]["callerSymbolId"], "caller_b");
    }

    /// Why candidates carry an `id` at all. Excalidraw's two distinct
    /// `getNonDeletedElements` functions (`packages/element/src/index.ts` and
    /// `packages/element/src/Scene.ts`) share the bare qualifiedName
    /// `getNonDeletedElements`, so a caller that picked one and re-asked by
    /// name would be handed the same candidate page forever. The `id` is the
    /// handle that ends the loop.
    #[test]
    fn candidates_stay_distinguishable_when_even_the_qualified_name_repeats() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("run_a", "Function", "run", "run", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("run_b", "Function", "run", "run", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "cb", "pkg::cb", "cb.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "run_b", "CALLS", "tree-sitter", true))
            .unwrap();
        let conn = Arc::new(IndexStore::new(conn));

        let by_name = SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() };
        let ambiguous = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                by_name,
            )
            .unwrap(),
        );
        assert_eq!(ambiguous["ambiguous"], true);

        // Re-asking by qualifiedName here is the loop: it is the same query.
        let requalified = SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() };
        assert_eq!(
            json_body(
                &handle_callers(
                    &conn,
                    &EmbeddingPipeline::disabled(),
                    QueryShapes::shipped(),
                    &no_capabilities(),
                    &SessionHints::default(),
                    requalified
                )
                .unwrap()
            )["ambiguous"],
            true
        );

        let mut ids: Vec<&str> =
            ambiguous["results"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["run_a", "run_b"],
            "the ids must survive even when nothing else tells them apart"
        );

        let params = SymbolQueryParams { symbol_id: Some("run_b".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        assert_eq!(body["results"].as_array().unwrap()[0]["callerSymbolId"], "caller_b");
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error_for_callers() {
        let conn = setup();
        let params =
            SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle_callers(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            &SessionHints::default(),
            params,
        )
        .unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error_for_callees() {
        let conn = setup();
        let params =
            SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle_callees(
            &Arc::new(IndexStore::new(conn)),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            &no_capabilities(),
            params,
        )
        .unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    /// Mirrors `find_references`'s small-page-size loop test. Proven here for
    /// callers only: the underlying pagination call is identical code for
    /// both directions (only `Direction` differs), so proving it once plus
    /// the trivial A/B/C chain test for callees is enough coverage without
    /// duplicating the whole loop.
    #[test]
    fn called_by_three_functions_returns_all_three_across_small_pages() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "CALLS", "tree-sitter", true))
            .unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page =
                list_calls(&conn, "target", "target.rs", &[], Direction::Incoming, 1, cursor.as_deref())
                    .unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|c| c.node.id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(
            seen,
            vec!["caller_a", "caller_b", "caller_c"],
            "all three callers must come back, once each"
        );
    }

    /// Same membership-test shape as `find_references`'s benchmark repro, but
    /// for `find_callers`: three known files call the target, a fourth known
    /// file
    /// doesn't, and an out-of-scope file also calls it - only the three
    /// in-scope callers must come back.
    #[test]
    fn scoping_find_callers_to_a_known_file_set_returns_only_the_callers_within_it() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();

        let known_files = ["k1.rs", "k2.rs", "k3.rs", "k4.rs"];
        for (i, file) in known_files.iter().enumerate() {
            let id = format!("known_{i}");
            upsert_node(
                &mut conn,
                NodeRecord::new(&id, "Function", &id, format!("pkg::{id}"), *file, "rust"),
            )
            .unwrap();
            if i < 3 {
                upsert_edge(
                    &mut conn,
                    EdgeRecord::new(format!("e_known_{i}"), &id, "target", "CALLS", "tree-sitter", true),
                )
                .unwrap();
            }
        }
        upsert_node(
            &mut conn,
            NodeRecord::new("outsider", "Function", "outsider", "pkg::outsider", "unrelated.rs", "rust"),
        )
        .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new("e_outsider", "outsider", "target", "CALLS", "tree-sitter", true),
        )
        .unwrap();

        let conn = Arc::new(IndexStore::new(conn));
        let params = SymbolQueryParams {
            symbol_id: Some("target".to_string()),
            file_paths: Some(known_files.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let body = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        let mut file_paths: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        file_paths.sort();
        assert_eq!(
            file_paths,
            vec!["k1.rs", "k2.rs", "k3.rs"],
            "must return exactly the known files that call the target"
        );
    }

    /// Omitting `file_paths` must be indistinguishable from a call made
    /// before this parameter existed, and an explicit empty array must
    /// answer identically to omitting it - same convention `edge_kinds`
    /// already uses for "no filter".
    #[test]
    fn omitting_file_paths_and_an_explicit_empty_array_both_behave_like_no_scope_for_callers() {
        let conn = Arc::new(IndexStore::new(setup_chain()));

        let omitted = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        let explicit_empty = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams {
                    symbol_id: Some("b".to_string()),
                    file_paths: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        assert_eq!(
            omitted, explicit_empty,
            "an explicit empty file_paths array must answer exactly as omitting it does"
        );
        assert_eq!(omitted["results"].as_array().unwrap().len(), 1);
    }

    /// A caller-supplied `limit` above the default page size must actually
    /// reach `paginate_edges`, not just be accepted and ignored. Proven for
    /// callers only, same reasoning as the small-page-size test above.
    #[test]
    fn a_custom_limit_returns_more_than_the_default_page_in_one_call() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        for i in 0..25 {
            let id = format!("caller_{i}");
            upsert_node(
                &mut conn,
                NodeRecord::new(&id, "Function", &id, format!("pkg::{id}"), "a.rs", "rust"),
            )
            .unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e_{i}"), &id, "target", "CALLS", "tree-sitter", true),
            )
            .unwrap();
        }
        let conn = Arc::new(IndexStore::new(conn));

        let params = SymbolQueryParams {
            symbol_id: Some("target".to_string()),
            limit: Some(25),
            ..Default::default()
        };
        let body = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                params,
            )
            .unwrap(),
        );
        assert_eq!(body["results"].as_array().unwrap().len(), 25, "all 25 must come back in one page");
        assert_eq!(body["hasMore"], false);
    }

    /// The footgun this hint closes: `symbol_name`/`symbol_id` resolution
    /// doesn't filter by kind, so a name matching a file's basename anchors
    /// on that `File` node exactly as if it were a declared symbol, and
    /// neither `find_callers` nor `find_callees` ever walks a `CALLS` edge
    /// incident on a File node - see `anchor::file_anchor_hint`. Both
    /// directions get their own assertion here (unlike the small-page-size
    /// loop test above, which proves the identical `list_calls` code once and
    /// leans on the trivial A/B/C chain for the other direction) because each
    /// `handle_*` builds its own response struct with its own `hint` field,
    /// so only exercising one would leave the other's wiring unverified.
    #[test]
    fn a_file_anchor_carries_a_hint_pointing_at_get_dependencies_for_both_directions() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new(
                "file",
                "File",
                "connection.ts",
                "src/connection.ts",
                "src/connection.ts",
                "typescript",
            ),
        )
        .unwrap();
        let conn = Arc::new(IndexStore::new(conn));

        let callers = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_id: Some("file".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        let callers_hint =
            callers["hint"].as_str().expect("a File-anchored find_callers call must carry a hint");
        assert!(callers_hint.contains("get_dependencies"), "{callers_hint}");
        assert_eq!(callers["results"].as_array().unwrap().len(), 0);
        assert_eq!(callers["hasMore"], false);
        assert_eq!(callers["allUnresolved"], false);

        let callees = json_body(
            &handle_callees(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                SymbolQueryParams { symbol_id: Some("file".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        let callees_hint =
            callees["hint"].as_str().expect("a File-anchored find_callees call must carry a hint");
        assert!(callees_hint.contains("get_dependencies"), "{callees_hint}");
        assert_eq!(callees["results"].as_array().unwrap().len(), 0);
        assert_eq!(callees["hasMore"], false);
        assert_eq!(callees["allUnresolved"], false);
    }

    /// Purely additive: an ordinary symbol anchor must never carry the
    /// `hint` field at all, not even as `null`.
    #[test]
    fn a_normal_symbol_anchor_never_carries_a_hint_field() {
        let conn = Arc::new(IndexStore::new(setup_chain()));

        let callers = json_body(
            &handle_callers(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                &SessionHints::default(),
                SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert!(callers.get("hint").is_none(), "hint must be entirely absent, not null: {callers}");

        let callees = json_body(
            &handle_callees(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() },
            )
            .unwrap(),
        );
        assert!(callees.get("hint").is_none(), "hint must be entirely absent, not null: {callees}");
    }

    #[test]
    fn handle_callees_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_a", "Function", "a", "pkg::a", "a.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_b", "Function", "b", "pkg::b", "b.rs", "rust"))
            .unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_c", "Function", "c", "pkg::c", "c.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "target", "callee_a", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "target", "callee_b", "CALLS", "tree-sitter", true))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "target", "callee_c", "CALLS", "tree-sitter", true))
            .unwrap();
        let conn = Arc::new(IndexStore::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = SymbolQueryParams {
                symbol_id: Some("target".to_string()),
                cursor: cursor.clone(),
                ..Default::default()
            };
            let result = handle_callees(
                &conn,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                params,
            )
            .unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["calleeSymbolId"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        seen.sort();
        assert_eq!(
            seen,
            vec!["callee_a", "callee_b", "callee_c"],
            "all three callees must come back, once each"
        );
    }

    /// `target` used by each of `users` (id, kind, file) through one `CALLS`
    /// edge apiece, every edge `resolved` or none.
    fn used_by(users: &[(&str, &str, &str)], resolved: bool) -> Arc<IndexStore> {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        for (id, kind, file) in users {
            upsert_node(&mut conn, NodeRecord::new(*id, *kind, *id, format!("pkg::{id}"), *file, "rust"))
                .unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e_{id}"), *id, "target", "CALLS", "tree-sitter", resolved),
            )
            .unwrap();
        }
        Arc::new(IndexStore::new(conn))
    }

    /// The `hint` a find_callers call on `target` answers with, `""` when absent.
    fn hint_for(store: &Arc<IndexStore>, hints: &SessionHints) -> String {
        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callers(
                store,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                hints,
                params,
            )
            .unwrap(),
        );
        body["hint"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn an_all_unresolved_page_carries_its_hint_on_every_call() {
        let unresolved = used_by(&[("a", "Function", "a.rs"), ("b", "Function", "b.rs")], false);
        let session = SessionHints::default();
        assert!(hint_for(&unresolved, &session).contains(session_hints::ALL_UNRESOLVED));
        assert!(hint_for(&unresolved, &session).contains(session_hints::ALL_UNRESOLVED), "every time");

        let resolved = used_by(&[("a", "Function", "a.rs"), ("b", "Function", "b.rs")], true);
        assert!(!hint_for(&resolved, &SessionHints::default()).contains(session_hints::ALL_UNRESOLVED));
    }

    #[test]
    fn a_file_row_hint_is_sent_once_per_session() {
        let symbols_only = used_by(&[("a", "Function", "a.rs")], true);
        let with_file_row = used_by(&[("f", "File", "f.rs")], true);
        let session = SessionHints::default();

        assert!(!hint_for(&symbols_only, &session).contains(session_hints::FILE_ROW));
        assert!(hint_for(&with_file_row, &session).contains(session_hints::FILE_ROW));
        assert!(!hint_for(&with_file_row, &session).contains(session_hints::FILE_ROW), "once per session");
        assert!(hint_for(&with_file_row, &SessionHints::default()).contains(session_hints::FILE_ROW));
    }

    #[test]
    fn a_files_tally_hint_is_sent_once_per_session() {
        let one_row_per_file = used_by(&[("a", "Function", "a.rs"), ("b", "Function", "b.rs")], true);
        let repeated_file = used_by(&[("a1", "Function", "a.rs"), ("a2", "Function", "a.rs")], true);
        let session = SessionHints::default();

        assert!(!hint_for(&one_row_per_file, &session).contains(session_hints::FILES_TALLY));
        assert!(hint_for(&repeated_file, &session).contains(session_hints::FILES_TALLY));
        assert!(!hint_for(&repeated_file, &session).contains(session_hints::FILES_TALLY), "once per session");
        assert!(hint_for(&repeated_file, &SessionHints::default()).contains(session_hints::FILES_TALLY));
    }

    /// Like `used_by`, but `target` calls each of `callees` (id, file).
    fn calling(callees: &[(&str, &str)], resolved: bool) -> Arc<IndexStore> {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust"))
            .unwrap();
        for (id, file) in callees {
            upsert_node(
                &mut conn,
                NodeRecord::new(*id, "Function", *id, format!("pkg::{id}"), *file, "rust"),
            )
            .unwrap();
            upsert_edge(
                &mut conn,
                EdgeRecord::new(format!("e_{id}"), "target", *id, "CALLS", "tree-sitter", resolved),
            )
            .unwrap();
        }
        Arc::new(IndexStore::new(conn))
    }

    fn callee_hint(store: &Arc<IndexStore>) -> String {
        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(
            &handle_callees(
                store,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &no_capabilities(),
                params,
            )
            .unwrap(),
        );
        body["hint"].as_str().unwrap_or_default().to_string()
    }

    #[test]
    fn an_all_unresolved_callee_page_carries_its_hint_on_every_call() {
        let unresolved = calling(&[("a", "a.rs"), ("b", "b.rs")], false);
        assert_eq!(callee_hint(&unresolved), session_hints::ALL_UNRESOLVED);
        assert_eq!(callee_hint(&unresolved), session_hints::ALL_UNRESOLVED, "every time");

        let resolved = calling(&[("a", "a.rs"), ("b", "b.rs")], true);
        assert_eq!(callee_hint(&resolved), "");
    }
}
