//! Real logic behind the `find_implementations` MCP tool. Same shape as
//! `find_references`: anchor lookup, then a single-direction `paginate_edges`
//! walk over one edge kind. The only two things that differ are the edge kind
//! (`SUPERTYPE_OF` instead of `REFERENCES`) and which end of that edge is "the
//! anchor" - `SUPERTYPE_OF` edges point subtype -> supertype (fromId is the
//! implementing/extending type, toId is the interface/base type it points
//! at), so "who implements this interface" is the `Incoming` direction,
//! resolving each edge's `from_id`. Single-hop by default: a class extending
//! a class that implements the anchor interface does NOT show up in the
//! default response, only the direct implementor/extender does - that is a
//! deliberate, cheap default, not a limitation nothing can see past. An agent
//! that genuinely needs the whole hierarchy (root-caused via a real
//! g-mesh-bench run: a task asked for implementors "including classes that
//! only get it indirectly by extending another implementation", and a session
//! that trusted a single-hop `hasMore: false` page as complete missed two of
//! them) can opt in with `transitive: true`, which walks the same
//! `SUPERTYPE_OF` edges transitively via `graph::traversal` - see
//! [`dispatch`] and [`from_root`].

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::daemon::manifest::Capabilities;
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination::{self, Direction};
use crate::graph::queries;
use crate::graph::resume_token::{self, ResumeState, VisitedNode};
use crate::graph::traversal::{self, ReachedNode, TraversalOptions, TraversalResult, TruncatedBy};
use crate::storage::write::NodeRecord;

use super::tool_result::{error, internal_error, success};
use super::{anchor, find_definition, provenance, FindImplementationsParams, SymbolQueryParams};

/// The one edge kind both the single-hop and transitive walks follow -
/// pulled out for the transitive path's own `TraversalOptions`/`ResumeState`
/// construction; the single-hop path below still spells it as a literal in
/// `list_implementations`'s `paginate_edges` call, which predates this
/// constant and has its own passing test suite - not worth touching for a
/// rename with zero behavior change.
const SUPERTYPE_EDGE: &str = "SUPERTYPE_OF";

/// One implementing/extending type on the other end of an inbound
/// `SUPERTYPE_OF` edge.
///
/// No `name` field: it never carries information `qualifiedName` doesn't
/// already have. `qualifiedName`/`startLine`/`startCol` are `None` - omitted
/// from the wire JSON entirely, never emitted as `null` or `0` - exactly
/// when `kind` is [`pagination::FILE_KIND`]; see that constant's doc comment
/// for why both are pure redundancy on a `File`-kind row.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImplementationSite {
    implementing_symbol_id: String,
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

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImplementationPage {
    /// See `anchor::AnchorInfo` - what `symbol_id`/`symbol_name` resolved to,
    /// so a caller asking about usages "elsewhere" doesn't need a separate
    /// `find_definition` call just to learn the anchor's own file/line.
    anchor: anchor::AnchorInfo,
    results: Vec<ImplementationSite>,
    has_more: bool,
    next_cursor: Option<String>,
    /// See `Page::all_unresolved` - true when every implementor in `results`
    /// came from an edge the linker couldn't confirm.
    all_unresolved: bool,
    /// See `anchor::file_anchor_hint` - present only when the anchor resolved
    /// to a `File` node, absent (not `null`) on every ordinary symbol anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
    /// See `super::provenance` - present only when the anchor's language
    /// declares a semantic tier that has not completed for this project, so
    /// this answer came from its structural tier alone. Absent (not `null`,
    /// not an "everything is fine" object) on every healthy response, which
    /// is nearly all of them.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<provenance::Provenance>,
}

/// Paginates the incoming `SUPERTYPE_OF` edges for `anchor_id` and resolves
/// each one to the implementing/extending node. Split out from `handle` so
/// tests can drive it with a small `page_size` without needing a page-size
/// field on the public tool parameters.
fn list_implementations(
    conn: &Connection,
    anchor_id: &str,
    anchor_file_path: &str,
    file_paths: &[&str],
    page_size: usize,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<ImplementationSite>> {
    let page = pagination::paginate_edges(
        conn,
        anchor_id,
        Direction::Incoming,
        &["SUPERTYPE_OF"],
        file_paths,
        anchor_file_path,
        // "Who implements this" has one answer per implementing type, and
        // an implementor both tiers found holds two edges - see
        // `Distinctness` (GM-361).
        pagination::Distinctness::OtherEndpoint,
        page_size,
        cursor,
    )
    .context("failed to paginate SUPERTYPE_OF edges")?;

    let mut rows = Vec::with_capacity(page.results.len());
    for pagination::ScoredEdge { edge, locality } in page.results {
        let implementing = queries::get_node(conn, &edge.from_id)
            .context("failed to resolve implementing node")?
            .with_context(|| format!("edge {} points at missing node {}", edge.id, edge.from_id))?;
        let is_file = implementing.kind == pagination::FILE_KIND;
        rows.push(pagination::EdgeRow {
            resolved: edge.resolved,
            locality,
            edge_id: edge.id.clone(),
            item: ImplementationSite {
                implementing_symbol_id: implementing.id,
                qualified_name: (!is_file).then_some(implementing.qualified_name),
                kind: implementing.kind,
                file_path: implementing.file_path,
                start_line: (!is_file).then_some(implementing.start_line),
                start_col: (!is_file).then_some(implementing.start_col),
                resolved: edge.resolved,
            },
        });
    }

    Ok(pagination::bound_page(rows, page.has_more, page.next_cursor))
}

pub(super) fn handle(
    conn: &Arc<Mutex<Connection>>,
    embedding: &EmbeddingPipeline,
    capabilities: &HashMap<String, Capabilities>,
    params: SymbolQueryParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let resolved = match anchor::resolve(&conn, Some(embedding), &params)? {
        Ok(resolved) => resolved,
        Err(finished) => return Ok(finished),
    };
    // Destructured here so everything below still reads the node directly,
    // while `resolved.by` stays available for the response's `resolvedBy`.
    let resolved_by = resolved.by;
    let anchor = resolved.node;
    let hint = anchor::file_anchor_hint(&anchor);
    let anchor_info = anchor::AnchorInfo::with_rung(&anchor, resolved_by);

    let page_size = pagination::resolve_page_size(params.limit);
    let file_paths: Vec<&str> = params.file_paths.iter().flatten().map(String::as_str).collect();
    let page = list_implementations(
        &conn,
        &anchor.id,
        &anchor.file_path,
        &file_paths,
        page_size,
        params.cursor.as_deref(),
    )
    .map_err(|e| internal_error("failed to find implementations", e))?;

    success(&ImplementationPage {
        anchor: anchor_info,
        results: page.results,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        all_unresolved: page.all_unresolved,
        hint,
        provenance: provenance::resolve(&conn, capabilities, &anchor.language),
    })
}

/// One implementing/extending type reached by the transitive walk, at the
/// hop count it was reached at rather than a `resolved` flag - a multi-hop
/// path can't honestly carry one boolean the way a direct edge's
/// [`ImplementationSite::resolved`] can; see `get_dependencies.rs`'s
/// `DependencyNode` doc comment, which explains the identical omission for
/// its own transitive rows.
///
/// `qualifiedName`/`startLine`/`startCol` follow the exact same
/// File-kind-omits-them rule as `ImplementationSite`; in practice a
/// `SUPERTYPE_OF` edge's `fromId` is never a `File` node, but the row shape
/// doesn't special-case that away, matching `ImplementationSite`'s own
/// doc comment.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TransitiveImplementationSite {
    implementing_symbol_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    kind: String,
    file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_col: Option<i64>,
    /// `SUPERTYPE_OF` hops from the anchor interface/base type. Always >= 1:
    /// the anchor itself is the walk's depth-0 node and is not reported back
    /// to the caller who named it, same convention as `get_dependencies`.
    depth: u32,
}

impl From<ReachedNode> for TransitiveImplementationSite {
    fn from(r: ReachedNode) -> Self {
        let is_file = r.node.kind == pagination::FILE_KIND;
        Self {
            implementing_symbol_id: r.node.id,
            qualified_name: (!is_file).then_some(r.node.qualified_name),
            kind: r.node.kind,
            file_path: r.node.file_path,
            start_line: (!is_file).then_some(r.node.start_line),
            start_col: (!is_file).then_some(r.node.start_col),
            depth: r.depth,
        }
    }
}

/// The transitive walk's own response envelope - not the single-hop
/// `ImplementationPage`'s `results`/`hasMore`/`nextCursor` shape, which has
/// no depth semantics to express. Mirrors `get_dependencies.rs`'s
/// `DependencyWalk` field-for-field (see that struct's own doc comment for
/// why `frontierNodes`/`resumeToken` are each populated only by their own
/// truncation cause), plus the one field this tool's single-hop response
/// already has that a fresh walk can still need: `hint`, present only when
/// the anchor resolved to a `File` node (see `anchor::file_anchor_hint`).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TransitiveImplementationWalk {
    /// See `anchor::AnchorInfo`. Present only on a fresh walk (`from_root`),
    /// absent (not `null`) on a resumed one - same "only when this call
    /// actually resolved an anchor" convention `hint` below already follows:
    /// a resumed call's caller already saw this on the response that handed
    /// it the token, so re-fetching the node just to repeat it would spend a
    /// query on information already delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    anchor: Option<anchor::AnchorInfo>,
    results: Vec<TransitiveImplementationSite>,
    truncated: bool,
    truncated_by: Option<&'static str>,
    frontier_nodes: Vec<String>,
    resume_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
    /// See `super::provenance`. Present on a fresh walk under exactly the
    /// same condition the single-hop page carries it, and absent on a
    /// *resumed* one for the same reason `anchor` above is - a resumed call
    /// resolves no anchor, so it has no language to name, and the response
    /// that handed out the token already carried the disclosure for this
    /// walk. Recomputing it would mean re-reading a node purely to repeat
    /// something already delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<provenance::Provenance>,
}

/// The wire spelling of each truncation cause - identical mapping to
/// `get_dependencies.rs`'s own `wire_name`, plus this function's own
/// `"responseSize"` for the same reason: fixed by
/// `docs/architecture/g-mesh-v1.md`'s truncation contract, not a per-tool
/// choice.
fn wire_name(cause: TruncatedBy) -> &'static str {
    match cause {
        TruncatedBy::MaxDepth => "maxDepth",
        TruncatedBy::MaxFanout => "maxFanout",
        TruncatedBy::ExplorationBudget => "explorationBudget",
    }
}

/// This tool's own `bound_walk`-equivalent: the exact same byte-budget
/// second-cut pattern `get_dependencies.rs`'s `bound_walk` implements for its
/// own row/response types, rebuilt here against
/// `TransitiveImplementationSite`/`TransitiveImplementationWalk` rather than
/// shared with it. Not generalized into one helper across both tools - this
/// is only the second caller of the pattern, and each caller's row shape,
/// response envelope and edge-direction convention differ enough that a
/// shared version would need its own generic parameters and trait bounds to
/// stand in for what are, today, three or four straightforward lines apiece.
///
/// Unlike `get_dependencies`, this tool's walk never varies `direction` or
/// `edge_kind` - it is always `Incoming` over `SUPERTYPE_OF` - so, unlike
/// that function, this one does not need either as a parameter; they are
/// baked into the `ResumeState` built at the bottom instead.
/// Everything a transitive page says about *itself* rather than about its
/// rows, in one argument.
///
/// The three travel together and are set together: a fresh walk
/// ([`from_root`]) has all three to give, a resumed one ([`continued`]) has
/// none of them, and no caller has ever wanted a different combination -
/// which is exactly the shape a struct exists to make unspellable. Grouping
/// them is also what keeps [`bound_walk`] under `clippy::too_many_arguments`
/// without an `allow`, now that GM-382 added the third.
struct WalkFraming {
    anchor: Option<anchor::AnchorInfo>,
    hint: Option<&'static str>,
    provenance: Option<provenance::Provenance>,
}

fn bound_walk(
    result: TraversalResult,
    max_depth: u32,
    max_fanout: u32,
    framing: WalkFraming,
    prior_visited: Vec<VisitedNode>,
    prior_walked: Vec<String>,
) -> TransitiveImplementationWalk {
    let WalkFraming { anchor, hint, provenance } = framing;
    // Present only on a fresh walk: a resumed call's `nodes` excludes
    // already-visited nodes (the anchor included), so `prior_visited` already
    // carries it forward instead.
    let anchor_id = result.nodes.first().filter(|n| n.depth == 0).map(|n| n.node.id.clone());

    let mut dtos: Vec<TransitiveImplementationSite> = Vec::with_capacity(result.nodes.len());
    for node in result.nodes {
        if node.depth > 0 {
            dtos.push(TransitiveImplementationSite::from(node));
        }
    }

    let Some(cut) = pagination::longest_prefix_fitting(&dtos, pagination::MAX_RESPONSE_BYTES) else {
        return TransitiveImplementationWalk {
            anchor,
            results: dtos,
            truncated: result.truncated,
            truncated_by: result.truncated_by.map(wire_name),
            frontier_nodes: result.frontier_nodes,
            resume_token: result.resume_token,
            hint,
            provenance,
        };
    };

    let mut visited = prior_visited;
    if let Some(id) = anchor_id {
        visited.push(VisitedNode { id, depth: 0 });
    }
    visited.extend(
        dtos[..cut].iter().map(|d| VisitedNode { id: d.implementing_symbol_id.clone(), depth: d.depth }),
    );

    let kept_ids: HashSet<&str> = dtos[..cut].iter().map(|d| d.implementing_symbol_id.as_str()).collect();
    let mut walked = prior_walked;
    // `Direction::Incoming`: the child this walk expanded through is always
    // the edge's `from_id` (the implementing/extending type) - see this
    // file's own module doc for the `SUPERTYPE_OF` direction convention.
    walked.extend(
        result.edges.into_iter().filter_map(|e| kept_ids.contains(e.from_id.as_str()).then_some(e.id)),
    );

    let token = resume_token::encode(&ResumeState {
        direction: Direction::Incoming,
        edge_kind: Some(SUPERTYPE_EDGE.to_string()),
        max_depth,
        max_fanout,
        visited,
        walked,
    });

    TransitiveImplementationWalk {
        anchor,
        results: dtos.into_iter().take(cut).collect(),
        truncated: true,
        truncated_by: Some("responseSize"),
        frontier_nodes: Vec::new(),
        resume_token: Some(token),
        hint,
        provenance,
    }
}

/// The transitive walk from a resolved anchor node, at
/// `traversal::DEFAULT_MAX_DEPTH` (5) unless the caller narrows it -
/// deliberately NOT `get_dependencies`'s own stricter default (2; see that
/// tool's `DEFAULT_MAX_DEPTH` doc comment for the reasoning behind it). That
/// tighter default exists because an `IMPORTS` walk can fan out from one
/// shared, foundational module across an entire codebase, and that fan-out
/// compounds with every extra hop of depth. A `SUPERTYPE_OF` hierarchy has no
/// equivalent hub: a real class/interface hierarchy is a handful of levels of
/// subclassing or interface implementation at most, nothing in the extractor
/// makes "half the codebase extends this one type" a shape that occurs, so
/// the walk engine's own generic, direction-agnostic default is the right
/// one here rather than a tool-specific tightening.
fn from_root(
    conn: &Connection,
    anchor_node: &NodeRecord,
    resolved_by: find_definition::ResolvedBy,
    hint: Option<&'static str>,
    max_depth: Option<u32>,
    capabilities: &HashMap<String, Capabilities>,
) -> Result<CallToolResult, ErrorData> {
    // Carries the rung for the same reason the single-hop path does: a caller
    // must be able to tell an exact resolution from one the ladder suggested,
    // and `transitive: true` is the same query with a deeper walk, not a
    // different kind of answer. Only `continued` legitimately has no rung -
    // a resumed walk carries its anchor rather than resolving one.
    let anchor_info = anchor::AnchorInfo::with_rung(anchor_node, resolved_by);
    let mut options = TraversalOptions::new(anchor_node.id.clone(), Direction::Incoming);
    options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
    if let Some(depth) = max_depth {
        options.max_depth = depth;
    }
    let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);

    let result = traversal::traverse(conn, options)
        .map_err(|e| internal_error("failed to walk the implementation hierarchy", e))?;
    let provenance = provenance::resolve(conn, capabilities, &anchor_node.language);
    let framing = WalkFraming { anchor: Some(anchor_info), hint, provenance };
    success(&bound_walk(result, max_depth, max_fanout, framing, Vec::new(), Vec::new()))
}

/// Continues a transitive walk the exploration budget or a prior
/// `responseSize` cut left short. The token carries the anchor, depth/fanout
/// limits and history of the walk it continues, so nothing about its shape is
/// re-read from the parameters here - same contract as
/// `get_dependencies.rs`'s own `continued`.
///
/// `hint` is always `None` here rather than recomputed: it only ever fires
/// for a `File`-kind anchor, and a `File` node has no incoming `SUPERTYPE_OF`
/// edges to walk, so a walk that reached a truncation cause worth resuming
/// could never have started from one - there is no real case this drops.
fn continued(conn: &Connection, token: &str) -> Result<CallToolResult, ErrorData> {
    let state =
        resume_token::decode(token).map_err(|e| internal_error("failed to decode resume token", e))?;
    let ResumeState { max_depth, max_fanout, visited: prior_visited, walked: prior_walked, .. } = state;

    let result = traversal::resume(conn, token, traversal::DEFAULT_EXPLORATION_BUDGET)
        .map_err(|e| internal_error("failed to resume the implementation walk", e))?;
    let framing = WalkFraming { anchor: None, hint: None, provenance: None };
    success(&bound_walk(result, max_depth, max_fanout, framing, prior_visited, prior_walked))
}

/// The entry point `mod.rs` calls. Dispatches on `transitive`/`resume_token`
/// and otherwise defers entirely to the unmodified single-hop [`handle`]:
/// the non-transitive path here is not a reimplementation of that logic, it
/// *is* that logic, called with a plain [`SymbolQueryParams`] built from this
/// struct's shared fields - which is what makes the single-hop response
/// byte-identical to what it was before this file gained a `transitive`
/// concept, by construction rather than by parallel maintenance.
pub(crate) fn dispatch(
    conn: &Arc<Mutex<Connection>>,
    embedding: &EmbeddingPipeline,
    capabilities: &HashMap<String, Capabilities>,
    params: FindImplementationsParams,
) -> Result<CallToolResult, ErrorData> {
    let FindImplementationsParams {
        symbol_id,
        symbol_name,
        cursor,
        limit,
        file_paths,
        transitive,
        max_depth,
        resume_token,
    } = params;

    if let Some(token) = resume_token {
        // An anchor or `transitive` next to a token is a contradiction, not a
        // preference to resolve silently: the token already names the walk
        // it continues - same rule `get_dependencies.rs` applies to
        // `resume_token` alongside `file_path`/`module_id`.
        if symbol_id.is_some() || symbol_name.is_some() || transitive.is_some() {
            return error(
                "g-mesh: `resume_token` already carries the walk it continues - call it without `symbol_id`/`symbol_name`/`transitive`",
            );
        }
        let conn = conn.lock().unwrap();
        return continued(&conn, &token);
    }

    let symbol_params = SymbolQueryParams { symbol_id, symbol_name, cursor, limit, file_paths };

    if !transitive.unwrap_or(false) {
        return handle(conn, embedding, capabilities, symbol_params);
    }

    let conn = conn.lock().unwrap();
    let resolved = match anchor::resolve(&conn, Some(embedding), &symbol_params)? {
        Ok(resolved) => resolved,
        Err(finished) => return Ok(finished),
    };
    let resolved_by = resolved.by;
    let anchor = resolved.node;
    let hint = anchor::file_anchor_hint(&anchor);
    from_root(&conn, &anchor, resolved_by, hint, max_depth, capabilities)
}

#[cfg(test)]
mod tests;
