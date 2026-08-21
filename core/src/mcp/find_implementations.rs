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

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, Direction};
use crate::graph::queries;
use crate::graph::resume_token::{self, ResumeState, VisitedNode};
use crate::graph::traversal::{self, ReachedNode, TraversalOptions, TraversalResult, TruncatedBy};
use crate::storage::write::NodeRecord;

use super::tool_result::{error, internal_error, success};
use super::{anchor, FindImplementationsParams, SymbolQueryParams};

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

pub(super) fn handle(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let resolved = match anchor::resolve(&conn, &params)? {
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
    let page = list_implementations(&conn, &anchor.id, &anchor.file_path, &file_paths, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find implementations", e))?;

    success(&ImplementationPage {
        anchor: anchor_info,
        results: page.results,
        has_more: page.has_more,
        next_cursor: page.next_cursor,
        all_unresolved: page.all_unresolved,
        hint,
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
fn bound_walk(
    result: TraversalResult,
    max_depth: u32,
    max_fanout: u32,
    anchor: Option<anchor::AnchorInfo>,
    hint: Option<&'static str>,
    prior_visited: Vec<VisitedNode>,
    prior_walked: Vec<String>,
) -> TransitiveImplementationWalk {
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
        };
    };

    let mut visited = prior_visited;
    if let Some(id) = anchor_id {
        visited.push(VisitedNode { id, depth: 0 });
    }
    visited.extend(dtos[..cut].iter().map(|d| VisitedNode { id: d.implementing_symbol_id.clone(), depth: d.depth }));

    let kept_ids: HashSet<&str> = dtos[..cut].iter().map(|d| d.implementing_symbol_id.as_str()).collect();
    let mut walked = prior_walked;
    // `Direction::Incoming`: the child this walk expanded through is always
    // the edge's `from_id` (the implementing/extending type) - see this
    // file's own module doc for the `SUPERTYPE_OF` direction convention.
    walked.extend(result.edges.into_iter().filter_map(|e| kept_ids.contains(e.from_id.as_str()).then_some(e.id)));

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
    hint: Option<&'static str>,
    max_depth: Option<u32>,
) -> Result<CallToolResult, ErrorData> {
    let anchor_info = anchor::AnchorInfo::from(anchor_node);
    let mut options = TraversalOptions::new(anchor_node.id.clone(), Direction::Incoming);
    options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
    if let Some(depth) = max_depth {
        options.max_depth = depth;
    }
    let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);

    let result = traversal::traverse(conn, options)
        .map_err(|e| internal_error("failed to walk the implementation hierarchy", e))?;
    success(&bound_walk(result, max_depth, max_fanout, Some(anchor_info), hint, Vec::new(), Vec::new()))
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
    let state = resume_token::decode(token).map_err(|e| internal_error("failed to decode resume token", e))?;
    let ResumeState { max_depth, max_fanout, visited: prior_visited, walked: prior_walked, .. } = state;

    let result = traversal::resume(conn, token, traversal::DEFAULT_EXPLORATION_BUDGET)
        .map_err(|e| internal_error("failed to resume the implementation walk", e))?;
    success(&bound_walk(result, max_depth, max_fanout, None, None, prior_visited, prior_walked))
}

/// The entry point `mod.rs` calls. Dispatches on `transitive`/`resume_token`
/// and otherwise defers entirely to the unmodified single-hop [`handle`]:
/// the non-transitive path here is not a reimplementation of that logic, it
/// *is* that logic, called with a plain [`SymbolQueryParams`] built from this
/// struct's shared fields - which is what makes the single-hop response
/// byte-identical to what it was before this file gained a `transitive`
/// concept, by construction rather than by parallel maintenance.
pub(super) fn dispatch(conn: &Arc<Mutex<Connection>>, params: FindImplementationsParams) -> Result<CallToolResult, ErrorData> {
    let FindImplementationsParams { symbol_id, symbol_name, cursor, limit, file_paths, transitive, max_depth, resume_token } =
        params;

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
        return handle(conn, symbol_params);
    }

    let conn = conn.lock().unwrap();
    let resolved = match anchor::resolve(&conn, &symbol_params)? {
        Ok(resolved) => resolved,
        Err(finished) => return Ok(finished),
    };
    let resolved_by = resolved.by;
    let anchor = resolved.node;
    let hint = anchor::file_anchor_hint(&anchor);
    from_root(&conn, &anchor, hint, max_depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::{EdgeRecord, NodeRecord};

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

    /// Sets up the acceptance criteria's chain: ClassA implements Interface,
    /// ClassB extends ClassA. Edge direction per `SUPERTYPE_OF`'s subtype ->
    /// supertype convention: ClassA -> Interface, ClassB -> ClassA.
    fn setup_chain() -> Connection {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("class_a", "Type", "ClassA", "pkg::ClassA", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("class_b", "Type", "ClassB", "pkg::ClassB", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a_iface", "class_a", "interface", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b_a", "class_b", "class_a", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        conn
    }

    #[test]
    fn find_implementations_of_interface_returns_exactly_class_a_not_class_b() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "the interface has exactly one direct implementor, not the transitive subclass");
        assert_eq!(results[0]["implementingSymbolId"], "class_a");
    }

    /// `SUPERTYPE_OF`'s `from_id` is never realistically a `File` node in
    /// practice (only `Type` nodes extend/implement), but the row shape
    /// itself must not special-case that away: same `name`-dropping,
    /// `qualifiedName`/`startLine`/`startCol`-omitting rule as
    /// `find_references`/`find_callers`/`find_callees` for any `File`-kind
    /// row, proven directly against the raw resolver rather than relying on
    /// it never being exercised.
    #[test]
    fn a_file_kind_implementing_row_omits_qualified_name_and_position_but_keeps_them_for_symbol_kind_rows() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "weird.rs", "src/weird.rs", "src/weird.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("class_a", "Type", "ClassA", "pkg::ClassA", "a.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_file", "file", "interface", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_class", "class_a", "interface", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();

        let page = list_implementations(&conn, "interface", "iface.rs", &[], 10, None).unwrap();
        assert_eq!(page.results.len(), 2);

        let file_row = page.results.iter().find(|r| r.kind == "File").expect("the File-kind row must be present");
        assert!(file_row.qualified_name.is_none(), "qualifiedName duplicates filePath for a File-kind row");
        assert!(file_row.start_line.is_none(), "startLine is meaningless for a File-kind row");
        assert!(file_row.start_col.is_none(), "startCol is meaningless for a File-kind row");
        assert_eq!(file_row.file_path, "src/weird.rs");

        let symbol_row = page.results.iter().find(|r| r.kind == "Type").expect("the Type-kind row must be present");
        assert_eq!(symbol_row.qualified_name.as_deref(), Some("pkg::ClassA"), "a symbol-kind row must still carry its qualifiedName");
        assert_eq!(symbol_row.start_line, Some(0), "a symbol-kind row must still carry its startLine");
        assert_eq!(symbol_row.start_col, Some(0), "a symbol-kind row must still carry its startCol");
    }

    #[test]
    fn zero_implementations_is_an_empty_page_not_an_error() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
        assert_eq!(body["allUnresolved"], false, "an empty page has nothing to be suspicious of");
    }

    #[test]
    fn a_page_where_every_implementor_is_unresolved_is_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "interface", "SUPERTYPE_OF", "tree-sitter", false)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["allUnresolved"], true, "every implementor unresolved must set the response-level marker");
    }

    #[test]
    fn a_page_with_at_least_one_resolved_implementor_is_not_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "interface", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "interface", "SUPERTYPE_OF", "tree-sitter", false)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(body["allUnresolved"], false, "one resolved row must clear the marker");
    }

    /// Task #190: the single-hop response echoes the resolved anchor.
    #[test]
    fn the_single_hop_response_echoes_the_resolved_anchor() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["anchor"]["id"], "interface");
        assert_eq!(body["anchor"]["qualifiedName"], "pkg::Iface");
        assert_eq!(body["anchor"]["kind"], "Type");
        assert_eq!(body["anchor"]["filePath"], "iface.rs");
    }

    /// The transitive walk's own response echoes the anchor too - it is the
    /// same tool, and the resolved node is available exactly as it is on the
    /// single-hop path. A resumed call, by contrast, must not carry it: see
    /// `a_resumed_transitive_walk_does_not_repeat_the_anchor` below.
    #[test]
    fn a_fresh_transitive_walk_echoes_the_resolved_anchor() {
        let conn = Arc::new(Mutex::new(setup_chain()));
        let params = FindImplementationsParams {
            symbol_id: Some("interface".to_string()),
            transitive: Some(true),
            ..Default::default()
        };
        let body = json_body(&dispatch(&conn, params).unwrap());
        assert_eq!(body["anchor"]["id"], "interface");
        assert_eq!(body["anchor"]["qualifiedName"], "pkg::Iface");
    }

    /// A resumed walk's caller already received the anchor on the response
    /// that handed it the `resumeToken`, so re-fetching the node just to
    /// repeat it would spend a query for nothing new - see `continued`'s own
    /// doc comment for the identical reasoning already established for
    /// `hint`. Driven through `traverse` + `bound_walk`/`continued` directly,
    /// same reason as `a_response_size_cut_is_continued_...` above: reaching
    /// a `responseSize` cut needs a `max_fanout` wider than this tool's own
    /// default, which `FindImplementationsParams` does not expose.
    #[test]
    fn a_resumed_transitive_walk_does_not_repeat_the_anchor() {
        let wide: usize = 600;
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        for i in 0..wide {
            let id = format!("impl{i:05}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "impl.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e{i:05}"), &id, "interface", "SUPERTYPE_OF", "tree-sitter", true))
                .unwrap();
        }

        let anchor_node = queries::get_node(&conn, "interface").unwrap().expect("anchor node must exist");
        let anchor_info = anchor::AnchorInfo::from(&anchor_node);

        let mut options = TraversalOptions::new("interface", Direction::Incoming);
        options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
        options.max_fanout = 10_000;
        let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);
        let result = traversal::traverse(&conn, options).unwrap();
        let first = bound_walk(result, max_depth, max_fanout, Some(anchor_info), None, Vec::new(), Vec::new());
        assert!(first.anchor.is_some(), "the first page of a fresh walk still carries the anchor");
        let token = first.resume_token.expect("this wide a fanout must truncate and hand back a token");

        let resumed = json_body(&continued(&conn, &token).unwrap());
        assert!(resumed.get("anchor").is_none(), "a resumed page must not repeat the anchor, not even as null: {resumed}");
    }

    /// Anchoring by name must reach the same node the id does - here the
    /// interface, whose one direct implementor is `class_a`.
    #[test]
    fn an_unambiguous_symbol_name_anchors_the_walk_without_a_symbol_id() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let by_id = json_body(
            &handle(&conn, SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() })
                .unwrap(),
        );
        let by_name = json_body(
            &handle(&conn, SymbolQueryParams { symbol_name: Some("Iface".to_string()), ..Default::default() }).unwrap(),
        );
        // Everything but `resolvedBy` must match: the walk, the results and the
        // anchor are properties of the node, however the caller addressed it.
        // `resolvedBy` is the one field that exists to differ - it reports the
        // rung, and the two calls took different rungs to the same node.
        let strip = |mut body: serde_json::Value| {
            body["anchor"].as_object_mut().expect("an anchor object").remove("resolvedBy");
            body
        };
        assert_eq!(
            strip(by_name.clone()),
            strip(by_id.clone()),
            "a name that resolves to one node must answer exactly as its id does"
        );
        assert_eq!(by_id["anchor"]["resolvedBy"], "id");
        assert_eq!(by_name["anchor"]["resolvedBy"], "name");
        assert_eq!(by_name["results"][0]["implementingSymbolId"], "class_a");
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn implemented_by_three_types_returns_all_three_across_small_pages() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_c", "Type", "C", "pkg::C", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "impl_c", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_implementations(&conn, "target", "target.rs", &[], 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|r| r.implementing_symbol_id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(seen, vec!["impl_a", "impl_b", "impl_c"], "all three implementors must come back, once each");
    }

    /// A caller-supplied `limit` above the default page size must actually
    /// reach `paginate_edges`, not just be accepted and ignored.
    #[test]
    fn a_custom_limit_returns_more_than_the_default_page_in_one_call() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust")).unwrap();
        for i in 0..25 {
            let id = format!("impl_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "a.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), limit: Some(25), ..Default::default() };
        let body = json_body(&handle(&conn, params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 25, "all 25 must come back in one page");
        assert_eq!(body["hasMore"], false);
    }

    /// The footgun this hint closes: a name matching a file's basename
    /// anchors on that `File` node exactly as if it were a declared symbol,
    /// and `find_implementations` never walks a `SUPERTYPE_OF` edge incident
    /// on a File node - see `anchor::file_anchor_hint`.
    #[test]
    fn a_file_anchor_carries_a_hint_pointing_at_get_dependencies() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("file", "File", "connection.ts", "src/connection.ts", "src/connection.ts", "typescript"))
            .unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams { symbol_id: Some("file".to_string()), ..Default::default() };
        let body = json_body(&handle(&conn, params).unwrap());

        let hint = body["hint"].as_str().expect("a File-anchored call must carry a hint");
        assert!(hint.contains("get_dependencies"), "the hint must point at get_dependencies: {hint}");
        assert_eq!(body["results"].as_array().unwrap().len(), 0, "a File anchor still answers as an empty page, not an error");
        assert_eq!(body["hasMore"], false);
        assert_eq!(body["allUnresolved"], false);
    }

    /// Purely additive: an ordinary symbol anchor must never carry the
    /// `hint` field at all, not even as `null`.
    #[test]
    fn a_normal_symbol_anchor_never_carries_a_hint_field() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert!(body.get("hint").is_none(), "hint must be entirely absent, not null, on a normal symbol anchor: {body}");
    }

    #[test]
    fn handle_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("impl_c", "Type", "C", "pkg::C", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "impl_c", "target", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = SymbolQueryParams { symbol_id: Some("target".to_string()), cursor: cursor.clone(), ..Default::default() };
            let result = handle(&conn, params).unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["implementingSymbolId"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        seen.sort();
        assert_eq!(seen, vec!["impl_a", "impl_b", "impl_c"]);
    }

    // --- transitive mode -------------------------------------------------

    /// The core backward-compatibility guarantee: `dispatch` (what `mod.rs`
    /// now calls) with `transitive` absent must answer byte-for-byte the same
    /// JSON the old, still-unmodified `handle` produces for the equivalent
    /// `SymbolQueryParams` - not just "a similar shape". No transitive-only
    /// field (`truncated`, `truncatedBy`, `frontierNodes`, `depth`) may leak
    /// into it.
    #[test]
    fn dispatch_without_transitive_answers_byte_identically_to_the_unmodified_single_hop_handle() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let via_handle = json_body(
            &handle(&conn, SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() }).unwrap(),
        );
        let via_dispatch = json_body(
            &dispatch(&conn, FindImplementationsParams { symbol_id: Some("interface".to_string()), ..Default::default() })
                .unwrap(),
        );

        assert_eq!(via_dispatch, via_handle, "transitive absent must be byte-identical to the pre-existing shape");
        assert!(via_dispatch.get("truncated").is_none());
        assert!(via_dispatch.get("truncatedBy").is_none());
        assert!(via_dispatch.get("frontierNodes").is_none());
        assert!(via_dispatch.get("depth").is_none());
    }

    /// The acceptance criteria's own motivating case: Interface <- ClassA <-
    /// ClassB. `transitive: true` must reach both, at their correct hop
    /// counts; absent and explicit `false` must still answer only the direct
    /// implementor, exactly like today.
    #[test]
    fn transitive_true_reaches_the_whole_hierarchy_while_false_or_absent_stays_single_hop() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let absent = json_body(
            &dispatch(&conn, FindImplementationsParams { symbol_id: Some("interface".to_string()), ..Default::default() })
                .unwrap(),
        );
        assert_eq!(absent["results"].as_array().unwrap().len(), 1, "absent transitive must stay single-hop");
        assert_eq!(absent["results"][0]["implementingSymbolId"], "class_a");

        let explicit_false = json_body(
            &dispatch(
                &conn,
                FindImplementationsParams {
                    symbol_id: Some("interface".to_string()),
                    transitive: Some(false),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        assert_eq!(explicit_false, absent, "transitive: false must answer exactly like omitting it");

        let walked = json_body(
            &dispatch(
                &conn,
                FindImplementationsParams {
                    symbol_id: Some("interface".to_string()),
                    transitive: Some(true),
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let reached: Vec<(String, u64)> = walked["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["implementingSymbolId"].as_str().unwrap().to_string(), r["depth"].as_u64().unwrap()))
            .collect();
        assert_eq!(reached, vec![("class_a".to_string(), 1), ("class_b".to_string(), 2)]);
        assert_eq!(walked["truncated"], false);
        assert!(walked["truncatedBy"].is_null());
        assert_eq!(walked["frontierNodes"].as_array().unwrap().len(), 0);
        assert!(walked["resumeToken"].is_null());
    }

    /// interface <- a <- b <- c <- d (`SUPERTYPE_OF` edges point subtype ->
    /// supertype), so a transitive walk from `interface` reaches a (depth 1),
    /// b (depth 2), c (depth 3), d (depth 4).
    fn setup_deep_chain() -> Connection {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        for id in ["a", "b", "c", "d"] {
            upsert_node(&mut conn, NodeRecord::new(id, "Type", id, format!("pkg::{id}"), format!("{id}.rs"), "rust")).unwrap();
        }
        upsert_edge(&mut conn, EdgeRecord::new("e_a_iface", "a", "interface", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b_a", "b", "a", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c_b", "c", "b", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_d_c", "d", "c", "SUPERTYPE_OF", "tree-sitter", true)).unwrap();
        conn
    }

    /// Truncation contract, cause one: the walk stopped at `max_depth`, so
    /// the caller gets the boundary to re-root on and nothing else - same
    /// contract `get_dependencies`'s own depth-cut test proves.
    #[test]
    fn a_max_depth_cut_reports_frontier_nodes_to_re_root_on() {
        let conn = Arc::new(Mutex::new(setup_deep_chain()));
        let params = FindImplementationsParams {
            symbol_id: Some("interface".to_string()),
            transitive: Some(true),
            max_depth: Some(2),
            ..Default::default()
        };
        let body = json_body(&dispatch(&conn, params).unwrap());

        let reached: Vec<(String, u64)> = body["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["implementingSymbolId"].as_str().unwrap().to_string(), r["depth"].as_u64().unwrap()))
            .collect();
        assert_eq!(reached, vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "maxDepth");
        assert_eq!(body["frontierNodes"], serde_json::json!(["b"]), "the level to re-root the same call on");
        assert!(body["resumeToken"].is_null(), "a depth cut is re-rooted, not resumed");
    }

    /// Truncation contract, cause two: a node had more implementors than the
    /// fan-out cap. `FindImplementationsParams` has no caller-facing
    /// `max_fanout` knob (only `max_depth` is exposed, per the acceptance
    /// criteria), so this drives `bound_walk` directly off a `TraversalResult`
    /// built with a narrowed `TraversalOptions.max_fanout` - the same thing
    /// `dispatch`/`from_root` would do internally, just with the one field
    /// this tool's own params don't surface.
    #[test]
    fn a_max_fanout_cut_reports_max_fanout_with_no_continuation_field() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        for id in ["a", "b", "c"] {
            upsert_node(&mut conn, NodeRecord::new(id, "Type", id, format!("pkg::{id}"), "impl.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{id}"), id, "interface", "SUPERTYPE_OF", "tree-sitter", true))
                .unwrap();
        }

        let mut options = TraversalOptions::new("interface", Direction::Incoming);
        options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
        options.max_fanout = 1;
        let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);
        let result = traversal::traverse(&conn, options).unwrap();
        let walk = bound_walk(result, max_depth, max_fanout, None, None, Vec::new(), Vec::new());

        assert_eq!(walk.results.len(), 1, "one of the three implementors, and a warning");
        assert!(walk.truncated);
        assert_eq!(walk.truncated_by, Some("maxFanout"));
        assert!(walk.frontier_nodes.is_empty(), "a fanout cut is paginated per node, not re-rooted");
        assert!(walk.resume_token.is_none());
    }

    /// Truncation contract, cause three: the response-size budget, continued
    /// by its token across a resume chain whose union covers every
    /// implementor exactly once - same property `get_dependencies`'s own
    /// equivalent test proves for `IMPORTS`. Driven through `traverse` +
    /// `bound_walk`/`continued` directly for the same reason as the fanout
    /// test above: reaching this cause needs `max_fanout` wide enough to
    /// clear this tool's own 50-per-level default, which
    /// `FindImplementationsParams` does not expose.
    #[test]
    fn a_response_size_cut_is_continued_by_its_token_and_the_chain_covers_every_implementor_once() {
        let wide: usize = 600;
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust")).unwrap();
        for i in 0..wide {
            let id = format!("impl{i:05}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "impl.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e{i:05}"), &id, "interface", "SUPERTYPE_OF", "tree-sitter", true))
                .unwrap();
        }

        let mut options = TraversalOptions::new("interface", Direction::Incoming);
        options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
        options.max_fanout = 10_000;
        let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);
        let result = traversal::traverse(&conn, options).unwrap();
        let first = bound_walk(result, max_depth, max_fanout, None, None, Vec::new(), Vec::new());

        assert!(!first.results.is_empty(), "at least one row must come back");
        assert!(first.results.len() < wide, "one response must not hold all {wide} implementors: {}", first.results.len());
        assert!(first.truncated);
        assert_eq!(first.truncated_by, Some("responseSize"));
        assert!(first.frontier_nodes.is_empty(), "a size cut is resumed, not re-rooted");

        let mut all: Vec<String> = first.results.iter().map(|r| r.implementing_symbol_id.clone()).collect();
        let mut token = first.resume_token.clone();
        let mut calls = 1;

        while let Some(t) = token {
            let body = json_body(&continued(&conn, &t).unwrap());
            calls += 1;
            all.extend(body["results"].as_array().unwrap().iter().map(|r| r["implementingSymbolId"].as_str().unwrap().to_string()));
            token = body["resumeToken"].as_str().map(str::to_string);
            assert!(calls < 50, "the chain must converge, not re-explore itself forever: {calls} calls so far");
        }

        assert!(calls > 2, "a page far smaller than {wide} implementors must take more than one resume: only {calls} calls");

        let mut deduped = all.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), all.len(), "no implementor may be returned twice across the chain");
        assert_eq!(deduped.len(), wide, "the whole chain's union must be every implementor, exactly once");
    }

    /// An anchor or `transitive` next to a `resume_token` is a contradiction,
    /// not a preference to resolve silently - same rule `get_dependencies`
    /// applies to `resume_token` alongside `file_path`/`module_id`.
    #[test]
    fn resume_token_alongside_an_anchor_or_transitive_is_a_tool_level_error() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let with_symbol_id = dispatch(
            &conn,
            FindImplementationsParams {
                symbol_id: Some("interface".to_string()),
                resume_token: Some("whatever".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(error_text(&with_symbol_id).contains("resume_token"));

        let with_symbol_name = dispatch(
            &conn,
            FindImplementationsParams {
                symbol_name: Some("Iface".to_string()),
                resume_token: Some("whatever".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(error_text(&with_symbol_name).contains("resume_token"));

        let with_transitive_only = dispatch(
            &conn,
            FindImplementationsParams { transitive: Some(true), resume_token: Some("whatever".to_string()), ..Default::default() },
        )
        .unwrap();
        assert!(error_text(&with_transitive_only).contains("resume_token"));
    }

    /// The same footgun `a_file_anchor_carries_a_hint_pointing_at_get_dependencies`
    /// proves for the single-hop path must still be caught in transitive
    /// mode: a name matching a file's basename anchors on that `File` node,
    /// and no `SUPERTYPE_OF` edge is ever incident on one, so the walk is
    /// honestly empty either way.
    #[test]
    fn a_file_anchor_hint_still_fires_in_transitive_mode() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new("file", "File", "connection.ts", "src/connection.ts", "src/connection.ts", "typescript"),
        )
        .unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params =
            FindImplementationsParams { symbol_id: Some("file".to_string()), transitive: Some(true), ..Default::default() };
        let body = json_body(&dispatch(&conn, params).unwrap());

        let hint = body["hint"].as_str().expect("a File-anchored transitive call must still carry a hint");
        assert!(hint.contains("get_dependencies"), "the hint must point at get_dependencies: {hint}");
        assert_eq!(body["results"].as_array().unwrap().len(), 0, "a File anchor still answers as an empty walk, not an error");
        assert_eq!(body["truncated"], false);
        assert!(body["resumeToken"].is_null());
    }
}
