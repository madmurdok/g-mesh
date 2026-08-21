//! Real logic behind the `get_dependencies` MCP tool - the one MVP tool whose
//! answer is genuinely transitive rather than a single hop. The walk itself,
//! and the whole truncation contract around it, already live in
//! `graph::traversal`; this module is the "anchor -> bounded `IMPORTS` walk ->
//! JSON" wiring around it, plus the two decisions `traversal` deliberately
//! leaves to its caller: which edge kind to follow, and how much of the
//! result is the caller's business.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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
use super::GetDependenciesParams;

/// The only edge kind this tool walks: `get_dependencies` answers "what does
/// this import" / "what imports this", not "what does this touch" - the
/// `CALLS`/`REFERENCES` side of the graph belongs to the single-hop tools.
const IMPORT_EDGE: &str = "IMPORTS";

/// The kind an import that resolved to nothing comes back as: a placeholder
/// standing in for a file this index does not have. See
/// [`DependencyNode::file_path`] for why it needs a case of its own.
const MODULE_KIND: &str = "Module";

/// `get_dependencies`'s own default when the caller omits `max_depth` -
/// deliberately far below `traversal::DEFAULT_MAX_DEPTH` (5), which stays
/// `TraversalOptions`' generic, direction-agnostic default for any future
/// caller of the walk engine and is left untouched here.
///
/// This tool's traffic is asymmetric in a way a single shared default can't
/// account for: an `Outgoing` walk (what does this file import) is bounded by
/// how many things one file imports, typically small; an `Incoming` walk
/// (what imports this file) can fan out across an entire codebase from one
/// shared/foundational module, and that fan-out *compounds* with every extra
/// hop of depth - `max_fanout` (default 50, see `traversal::DEFAULT_MAX_FANOUT`)
/// bounds one node's own children, not how wide a whole level gets. Measured
/// on g-mesh-bench's corpus (v0.4.0 outlier findings, `get_dependencies`
/// section): an `Incoming` walk of a shared math-utils entrypoint at the old
/// depth-5 default produced a 115,863-character response the MCP client's
/// transport rejected outright; the identical call at `max_depth: 1` dropped
/// to 10,436 characters and succeeded. Depth 1 alone only answers "who
/// directly depends on this", too narrow for the impact-analysis question
/// this tool mostly exists for ("what would changing this break, and what
/// depends on *that*"); depth 2 answers that shape while staying an order of
/// magnitude more conservative than the walk engine's own default. The size-
/// bounded truncation added alongside this default (`bound_walk`) is the
/// backstop for the remaining cases where even depth 2 is too wide on an
/// unusually shared module.
const DEFAULT_MAX_DEPTH: u32 = 2;

/// One reached dependency, with how many import hops away it is. No
/// `resolved` flag, unlike the single-hop tools: a node several hops out is
/// reached over a *path* of edges, and one flag can only describe one of
/// them, so it would read as a trust claim about the path that it does not
/// make. The edge list the walk collects is left out for the same reason it
/// isn't needed here - impact analysis asks which files an edit reaches, not
/// by which route.
///
/// No `name` field: it never carries information `qualifiedName` doesn't
/// already have (at worst a shorter, less unique view of the same symbol) -
/// same rule the single-hop tools' row shapes follow.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyNode {
    id: String,
    kind: String,
    /// Omitted (not `null`) for a `File`-kind row: a `File` node's
    /// `qualifiedName` IS its own `filePath` by construction (see
    /// `pagination::FILE_KIND`'s doc comment), so a `File` row would otherwise
    /// carry the exact same path string twice. Present for `Module` rows,
    /// where it's the only field that carries the specifier at all - the
    /// mirror image of `file_path` below being absent there.
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    /// The file this dependency *is*, and null when it is not one. An import
    /// `graph::imports` could not link - a package, or a relative path with
    /// nothing indexed behind it - stays a `Module` placeholder whose stored
    /// `filePath` is the *importing* file, because that is where the
    /// specifier is written. Echoing that column here would name the wrong
    /// file twice over: it reads as "the dependency lives there", and it
    /// collides with the importer's own row in the same walk. `qualifiedName`
    /// still carries the specifier, which is all there is to act on for
    /// something with no file to open.
    file_path: Option<String>,
    /// Import hops from the anchor. Always >= 1: the anchor itself is the
    /// walk's depth-0 node and is not reported back to the caller who named it.
    depth: u32,
}

impl From<ReachedNode> for DependencyNode {
    fn from(r: ReachedNode) -> Self {
        let is_file = r.node.kind == pagination::FILE_KIND;
        let file_path = (r.node.kind != MODULE_KIND).then_some(r.node.file_path);
        Self {
            id: r.node.id,
            kind: r.node.kind,
            qualified_name: (!is_file).then_some(r.node.qualified_name),
            file_path,
            depth: r.depth,
        }
    }
}

/// Not the `results`/`hasMore`/`nextCursor` envelope the list-shaped tools
/// share: a truncated walk is not a page, and which continuation field it
/// hands back depends on what cut it. `frontierNodes` is non-empty only for
/// `maxDepth` (re-root the same call on them), `resumeToken` is present only
/// for `explorationBudget` (call again with it, nothing else); `maxFanout`
/// needs neither - the caller re-queries the cut node with the single-hop
/// tools' own pagination.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyWalk {
    results: Vec<DependencyNode>,
    truncated: bool,
    truncated_by: Option<&'static str>,
    frontier_nodes: Vec<String>,
    resume_token: Option<String>,
}

/// The wire spelling of each truncation cause, fixed by the contract in
/// `docs/architecture/g-mesh-v1.md`, plus `bound_walk`'s own `"responseSize"` -
/// not part of that contract since it isn't a `TruncatedBy` cause at all (it
/// fires after `traversal` has already finished, on the wire DTO's own
/// serialized size), but spelled the same way for consistency.
fn wire_name(cause: TruncatedBy) -> &'static str {
    match cause {
        TruncatedBy::MaxDepth => "maxDepth",
        TruncatedBy::MaxFanout => "maxFanout",
        TruncatedBy::ExplorationBudget => "explorationBudget",
    }
}

/// A second, independent ceiling on top of `max_depth`/`max_fanout`: neither
/// bounds the *total size* of a whole level, only depth (how many levels) or
/// fanout (one node's own children) individually - a node can be depth- and
/// fanout-bounded and still, summed across every node reached at a given
/// depth, produce a response too large for `pagination::MAX_RESPONSE_BYTES`.
///
/// Reuses the exact resume-token mechanism `traversal::traverse`'s own
/// exploration-budget cut already relies on (`graph::resume_token`), just
/// built from wherever this function's own byte cut lands rather than
/// wherever the CTE's internal row budget ran out - so a caller sees the
/// same `resumeToken` continuation contract regardless of which cause
/// actually cut the walk short. `visited` reseeds every kept node (plus the
/// anchor) exactly the way an exploration-budget resume already does -
/// `ResumeState`'s own doc comment explains why that is the conservative,
/// provably-complete choice, not just cheaper to build. `walked` keeps only
/// the edges whose child landed inside the kept set: an edge to a node this
/// cut dropped must stay undiscovered as far as the token is concerned, or a
/// resumed call would never re-offer it.
///
/// A response that already fits under budget comes back with the original
/// result's `truncated`/`truncated_by`/`frontier_nodes`/`resume_token`
/// completely unchanged - this function is pure headroom for the rare
/// oversized level, never a new default for the common one.
///
/// `prior_visited`/`prior_walked` are the walk's history from *before* this
/// call - empty for a fresh walk (`from_root`), or the incoming
/// `resume_token`'s own `visited`/`walked` for a continuation (`continued`).
/// `TraversalResult.nodes`/`.edges` from a resumed call hold only what that
/// call newly discovered (see `traversal::resume`'s doc comment), so a token
/// built from them alone - without folding in what earlier calls in the
/// chain already reported - would forget that history and risk a later call
/// re-discovering and re-returning an already-reported node. Whether the
/// natural cause (`ExplorationBudget`) or this function's own byte cut is
/// what carries the token forward, the chain must accumulate the same way
/// `traversal::resume` already does internally for its own case (see
/// `ResumeState`'s doc comment) - this is that same accumulation, one layer
/// up, for the case `traversal.rs` cannot see: a response the JSON wire
/// shape made too big.
fn bound_walk(
    result: TraversalResult,
    direction: Direction,
    edge_kind: Option<String>,
    max_depth: u32,
    max_fanout: u32,
    prior_visited: Vec<VisitedNode>,
    prior_walked: Vec<String>,
) -> DependencyWalk {
    // Present only on a fresh walk: a resumed call's `nodes` excludes
    // already-visited nodes (the anchor included), so `prior_visited` already
    // carries it forward instead.
    let anchor_id = result.nodes.first().filter(|n| n.depth == 0).map(|n| n.node.id.clone());

    let mut dtos: Vec<DependencyNode> = Vec::with_capacity(result.nodes.len());
    for node in result.nodes {
        if node.depth > 0 {
            dtos.push(DependencyNode::from(node));
        }
    }

    let Some(cut) = pagination::longest_prefix_fitting(&dtos, pagination::MAX_RESPONSE_BYTES) else {
        return DependencyWalk {
            results: dtos,
            truncated: result.truncated,
            truncated_by: result.truncated_by.map(wire_name),
            frontier_nodes: result.frontier_nodes,
            resume_token: result.resume_token,
        };
    };

    let mut visited = prior_visited;
    if let Some(id) = anchor_id {
        visited.push(VisitedNode { id, depth: 0 });
    }
    visited.extend(dtos[..cut].iter().map(|d| VisitedNode { id: d.id.clone(), depth: d.depth }));

    let kept_ids: HashSet<&str> = dtos[..cut].iter().map(|d| d.id.as_str()).collect();
    let mut walked = prior_walked;
    walked.extend(result.edges.into_iter().filter_map(|e| {
        let child = match direction {
            Direction::Outgoing => &e.to_id,
            Direction::Incoming => &e.from_id,
        };
        kept_ids.contains(child.as_str()).then_some(e.id)
    }));

    let token = resume_token::encode(&ResumeState { direction, edge_kind, max_depth, max_fanout, visited, walked });

    DependencyWalk {
        results: dtos.into_iter().take(cut).collect(),
        truncated: true,
        truncated_by: Some("responseSize"),
        frontier_nodes: Vec::new(),
        resume_token: Some(token),
    }
}

/// A fresh walk's shape minus its root - what both anchor arms forward
/// unchanged once they have resolved the root the caller meant.
struct WalkShape {
    direction: Direction,
    max_depth: Option<u32>,
    max_fanout: Option<u32>,
}

/// The walk itself, at the documented defaults unless the caller narrowed
/// them. The exploration budget is deliberately not one of the caller's
/// dials: it bounds what the query engine visits internally, not what the
/// caller asked to see, so it is always the module default here.
fn from_root(conn: &Connection, root: String, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    let mut options = TraversalOptions::new(root, shape.direction);
    options.edge_kind = Some(IMPORT_EDGE.to_string());
    // `DEFAULT_MAX_DEPTH` here, not `TraversalOptions::new`'s own default -
    // see that constant's doc comment for why this tool needs a stricter one.
    options.max_depth = shape.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    if let Some(max_fanout) = shape.max_fanout {
        options.max_fanout = max_fanout;
    }
    let (direction, edge_kind, max_depth, max_fanout) =
        (options.direction, options.edge_kind.clone(), options.max_depth, options.max_fanout);

    let result =
        traversal::traverse(conn, options).map_err(|e| internal_error("failed to walk the import graph", e))?;
    success(&bound_walk(result, direction, edge_kind, max_depth, max_fanout, Vec::new(), Vec::new()))
}

fn from_file(conn: &Connection, file_path: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    let anchor = queries::find_file_node(conn, file_path).map_err(|e| internal_error("failed to look up file", e))?;

    match anchor {
        Some(node) => from_root(conn, node.id, shape),
        None => error(no_file_message(conn, file_path)?),
    }
}

/// The not-found answer, with what the index can add to it.
///
/// The caller who reaches this usually asked about a *package* or a directory -
/// "which files import from `@excalidraw/math`" - and this tool takes an
/// exact file path. A bare refusal sends them hunting with Glob for the entry
/// point, which costs a round trip: measured on g-mesh-bench as
/// `get_dependencies[57ch] -> Glob -> Glob -> get_dependencies[16267ch]`.
///
/// So when the path is a prefix of files that *are* indexed, this names them,
/// entry point first. Two forms are tried: the path as given
/// (`packages/math`), then its last segment matched as a directory
/// (`@excalidraw/math` -> `math`), which is how a workspace package name
/// relates to its directory in the layouts this meets. The second is offered
/// as a suggestion and nothing more - a name matching a directory does not
/// establish that the package lives there.
///
/// The specifier itself cannot help: `graph::imports` keeps a placeholder
/// `Module` node per import specifier, but only for the ones that never
/// resolved (`react` survives, `@excalidraw/math` became an edge to a file and
/// its placeholder is gone). So there is nothing to look the package name up
/// in, which is why this matches paths rather than pretending otherwise.
fn no_file_message(conn: &Connection, file_path: &str) -> Result<String, ErrorData> {
    const MAX_FILES: usize = 5;
    let terse = format!("g-mesh: no file '{file_path}' found in the index");

    let under = queries::find_files_under(conn, file_path, MAX_FILES)
        .map_err(|e| internal_error("failed to look up files under a prefix", e))?;
    if !under.is_empty() {
        return Ok(format!(
            "{terse} - it is not a file. These indexed files sit under it: {}. \
             This tool walks the import graph from one file, so ask about the entry point.",
            paths_of(&under),
        ));
    }

    // `@excalidraw/math` and the like: the last segment is the directory a
    // workspace package usually lives in, but this only suggests, never claims.
    let Some(segment) = file_path.rsplit('/').next().filter(|s| !s.is_empty() && *s != file_path) else {
        return Ok(terse);
    };
    let by_segment = queries::find_files_ending_in_dir(conn, segment, MAX_FILES)
        .map_err(|e| internal_error("failed to look up files by directory name", e))?;
    if by_segment.is_empty() {
        return Ok(terse);
    }
    Ok(format!(
        "{terse} - and it is not a path this index carries. If '{segment}' is the package's \
         directory, these indexed files are under one named that: {}. This tool walks the import \
         graph from one file, so ask about the entry point.",
        paths_of(&by_segment),
    ))
}

fn paths_of(nodes: &[NodeRecord]) -> String {
    nodes.iter().map(|n| n.file_path.clone()).collect::<Vec<_>>().join(", ")
}

/// A module id is already a node id, so this lookup buys nothing but the
/// error message: an unknown id would otherwise walk nothing at all and read
/// as "this module imports nothing", which is the one answer a bounded walk
/// must never fake.
///
/// It falls through to a file lookup because callers reliably put something
/// else here. `module_id` reads as "the module's name" and sits next to
/// `file_path` as its documented alternative, so a caller holding
/// `@excalidraw/math` or `packages/math/src/index.ts` puts *that* in it -
/// observed in every recorded run of g-mesh-bench's
/// `ex-deps-package-math-incoming`, which then cost a refusal, a blind Glob
/// and a second call to get the answer the first one had the input for. A
/// path that this index carries is an answerable question however the caller
/// labelled it, and refusing it on a technicality buys nothing.
fn from_module(conn: &Connection, module_id: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    let anchor = queries::get_node(conn, module_id).map_err(|e| internal_error("failed to look up module", e))?;

    match anchor {
        Some(node) => from_root(conn, node.id, shape),
        None => match queries::find_file_node(conn, module_id)
            .map_err(|e| internal_error("failed to look up file", e))?
        {
            Some(node) => from_root(conn, node.id, shape),
            None => error(no_file_message(conn, module_id)?),
        },
    }
}

/// Continues a walk the exploration budget cut short. The token carries the
/// anchor, direction and limits of the walk it continues, so nothing about
/// its shape is re-read from the parameters here.
fn continued(conn: &Connection, token: &str) -> Result<CallToolResult, ErrorData> {
    // Decoded a second time here (`traversal::resume` decodes its own copy
    // internally) purely to read the walk's shape back out for `bound_walk` -
    // cheap, and keeps `traversal`'s public surface free of a getter that
    // exists for one caller.
    let state = resume_token::decode(token).map_err(|e| internal_error("failed to decode resume token", e))?;
    let ResumeState { direction, edge_kind, max_depth, max_fanout, visited: prior_visited, walked: prior_walked } =
        state;

    let result = traversal::resume(conn, token, traversal::DEFAULT_EXPLORATION_BUDGET)
        .map_err(|e| internal_error("failed to resume the import walk", e))?;
    success(&bound_walk(result, direction, edge_kind, max_depth, max_fanout, prior_visited, prior_walked))
}

pub(super) fn handle(conn: &Arc<Mutex<Connection>>, params: GetDependenciesParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();
    let GetDependenciesParams { file_path, module_id, direction, max_depth, max_fanout, resume_token } = params;
    let shape = WalkShape { direction, max_depth, max_fanout };

    match (resume_token, file_path, module_id) {
        (Some(token), None, None) => continued(&conn, &token),
        // An anchor next to a token is a contradiction, not a preference to
        // resolve silently: the token already names the walk it continues,
        // and picking one of the two would answer a question nobody asked.
        (Some(_), _, _) => {
            error("g-mesh: `resume_token` already carries the walk it continues - call it without `file_path`/`module_id`")
        }
        (None, Some(file_path), None) => from_file(&conn, &file_path, &shape),
        (None, None, Some(module_id)) => from_module(&conn, &module_id, &shape),
        (None, Some(_), Some(_)) => error("g-mesh: give either `file_path` or `module_id`, not both"),
        (None, None, None) => error("g-mesh: give either `file_path` or `module_id` to start from"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::{self, Diff, EdgeRecord, NodeRecord};

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

    fn file(path: &str) -> NodeRecord {
        NodeRecord::new(path, "File", path, path, path, "rust")
    }

    /// `from` imports `to`, i.e. the edge points the way the dependency does.
    fn imports(conn: &mut Connection, from: &str, to: &str) {
        upsert_edge(conn, EdgeRecord::new(format!("e_{from}_{to}"), from, to, "IMPORTS", "tree-sitter", true)).unwrap();
    }

    /// a.rs -> b.rs -> c.rs, the chain both direction tests read in opposite
    /// ways.
    fn import_chain() -> Connection {
        let mut conn = setup();
        for path in ["a.rs", "b.rs", "c.rs"] {
            upsert_node(&mut conn, file(path)).unwrap();
        }
        imports(&mut conn, "a.rs", "b.rs");
        imports(&mut conn, "b.rs", "c.rs");
        conn
    }

    /// An import nothing could be linked to, stored the way the js-ts
    /// extractor stores it: a `Module` node whose `filePath` is the
    /// *importing* file, because that is where the specifier is written.
    fn unresolved_import(importer: &str, specifier: &str) -> NodeRecord {
        NodeRecord::new(format!("mod_{specifier}"), MODULE_KIND, specifier, specifier, importer, "typescript")
    }

    fn anchored_at(file_path: &str, direction: Direction) -> GetDependenciesParams {
        GetDependenciesParams {
            file_path: Some(file_path.to_string()),
            module_id: None,
            direction,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        }
    }

    /// (id, depth) per result row, in the order the walk reported them.
    fn reached(body: &serde_json::Value) -> Vec<(String, u64)> {
        body["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), r["depth"].as_u64().unwrap()))
            .collect()
    }

    /// Acceptance criteria: a three-file import chain comes back whole, not
    /// one hop of it - this is the only tool that walks past its own anchor.
    #[test]
    fn an_import_chain_comes_back_transitively_with_the_hop_count_per_node() {
        let conn = import_chain();

        let result = handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap();
        let body = json_body(&result);

        assert_eq!(reached(&body), vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]);
        assert_eq!(body["truncated"], false);
        assert!(body["truncatedBy"].is_null());
        assert_eq!(body["frontierNodes"].as_array().unwrap().len(), 0);
        assert!(body["resumeToken"].is_null());
    }

    /// The same chain read the other way: from its far end, `Incoming`
    /// reaches the importers, and the two directions must not agree.
    #[test]
    fn incoming_walks_the_importers_and_outgoing_the_imports() {
        let conn = Arc::new(Mutex::new(import_chain()));

        let upstream = json_body(&handle(&conn, anchored_at("c.rs", Direction::Incoming)).unwrap());
        assert_eq!(reached(&upstream), vec![("b.rs".to_string(), 1), ("a.rs".to_string(), 2)]);

        let downstream = json_body(&handle(&conn, anchored_at("c.rs", Direction::Outgoing)).unwrap());
        assert_eq!(reached(&downstream), vec![], "nothing imports out of the end of the chain");
        assert_eq!(downstream["truncated"], false, "an empty walk is complete, not truncated");
    }

    /// The anchor is what the caller already named; repeating it in the
    /// results would only make "how far away is this" ambiguous.
    #[test]
    fn the_anchor_itself_is_not_reported_as_its_own_dependency() {
        let conn = import_chain();
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());

        let ids: Vec<&str> = body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(!ids.contains(&"a.rs"), "the depth-0 anchor must not appear among its own dependencies: {ids:?}");
    }

    /// A placeholder must not borrow the importing file's path on the way
    /// out: "zod lives in a.rs" is both untrue and indistinguishable from
    /// a.rs's own row in the same walk.
    #[test]
    fn an_unresolved_import_is_reported_without_a_file_path_of_its_own() {
        let mut conn = import_chain();
        upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
        imports(&mut conn, "a.rs", "mod_zod");

        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());
        let rows = body["results"].as_array().unwrap();

        let module = rows.iter().find(|r| r["kind"] == "Module").expect("the placeholder is still a dependency");
        assert!(module["filePath"].is_null(), "a module placeholder has no file of its own: {module}");
        assert_eq!(module["qualifiedName"], "zod", "the specifier is all there is left to act on");

        let files: Vec<&str> =
            rows.iter().filter(|r| r["kind"] == "File").map(|r| r["filePath"].as_str().unwrap()).collect();
        assert_eq!(files, vec!["b.rs", "c.rs"], "real files are still addressed by their own path");
    }

    /// A `File`-kind row's `qualifiedName` is byte-identical to its own
    /// `filePath` by construction (see `pagination::FILE_KIND`'s doc comment),
    /// so it must be omitted from the wire JSON entirely rather than repeat
    /// the same path string twice. A `Module` placeholder has no `filePath`
    /// of its own, so it keeps `qualifiedName` as the only field carrying the
    /// specifier - the mirror image of the previous test.
    #[test]
    fn a_file_kind_row_omits_qualified_name_a_module_row_keeps_it() {
        let mut conn = import_chain();
        upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
        imports(&mut conn, "a.rs", "mod_zod");

        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());
        let rows = body["results"].as_array().unwrap();

        let files: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "File").collect();
        assert!(!files.is_empty());
        for file in files {
            assert!(file.get("qualifiedName").is_none(), "a File row must not repeat its own filePath as qualifiedName: {file}");
        }

        let module = rows.iter().find(|r| r["kind"] == "Module").expect("the placeholder is still a dependency");
        assert_eq!(module["qualifiedName"], "zod", "a Module row has no filePath, so qualifiedName must stay");
    }

    /// `name` never carries information `qualifiedName` doesn't already have
    /// (at worst a shorter, less unique view of the same symbol) - dropped
    /// entirely from every row, real file and unresolved-module placeholder
    /// alike.
    #[test]
    fn no_row_carries_a_name_field() {
        let mut conn = import_chain();
        upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
        imports(&mut conn, "a.rs", "mod_zod");

        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());
        let rows = body["results"].as_array().unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            assert!(row.get("name").is_none(), "the name field must never be present on any row: {row}");
        }
    }

    #[test]
    fn only_import_edges_are_walked() {
        let mut conn = import_chain();
        upsert_node(&mut conn, file("d.rs")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_call", "a.rs", "d.rs", "CALLS", "tree-sitter", true)).unwrap();

        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());

        let ids: Vec<&str> = body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["b.rs", "c.rs"], "a CALLS edge is not a dependency: {ids:?}");
    }

    #[test]
    fn a_module_id_anchors_the_walk_without_a_path_lookup() {
        let conn = import_chain();
        let params = GetDependenciesParams {
            file_path: None,
            module_id: Some("a.rs".to_string()),
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        };

        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(reached(&body), vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]);
    }

    #[test]
    fn an_unknown_anchor_is_a_tool_level_error_rather_than_an_empty_walk() {
        let conn = Arc::new(Mutex::new(import_chain()));

        let by_path = handle(&conn, anchored_at("does/not/exist.rs", Direction::Outgoing)).unwrap();
        assert!(error_text(&by_path).contains("does/not/exist.rs"));

        let by_module = GetDependenciesParams {
            file_path: None,
            module_id: Some("no_such_module".to_string()),
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        };
        assert!(error_text(&handle(&conn, by_module).unwrap()).contains("no_such_module"));
    }

    #[test]
    fn every_bad_anchor_combination_is_its_own_tool_level_error() {
        let conn = Arc::new(Mutex::new(import_chain()));
        let base = || GetDependenciesParams {
            file_path: None,
            module_id: None,
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        };

        let neither = handle(&conn, base()).unwrap();
        assert!(error_text(&neither).contains("file_path"));

        let both = GetDependenciesParams {
            file_path: Some("a.rs".to_string()),
            module_id: Some("a.rs".to_string()),
            ..base()
        };
        assert!(error_text(&handle(&conn, both).unwrap()).contains("not both"));

        let token_and_anchor = GetDependenciesParams {
            file_path: Some("a.rs".to_string()),
            resume_token: Some("whatever".to_string()),
            ..base()
        };
        assert!(error_text(&handle(&conn, token_and_anchor).unwrap()).contains("resume_token"));
    }

    /// Truncation contract, cause one: the walk stopped at the depth limit,
    /// so the caller gets the boundary to re-root on and nothing else.
    #[test]
    fn a_depth_cut_reports_max_depth_and_hands_back_only_the_frontier() {
        let mut conn = setup();
        for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            upsert_node(&mut conn, file(path)).unwrap();
        }
        imports(&mut conn, "a.rs", "b.rs");
        imports(&mut conn, "b.rs", "c.rs");
        imports(&mut conn, "c.rs", "d.rs");

        let params = GetDependenciesParams { max_depth: Some(1), ..anchored_at("a.rs", Direction::Outgoing) };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());

        assert_eq!(reached(&body), vec![("b.rs".to_string(), 1)]);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "maxDepth");
        assert_eq!(body["frontierNodes"], serde_json::json!(["b.rs"]), "the level to re-root the same call on");
        assert!(body["resumeToken"].is_null(), "a depth cut is re-rooted, not resumed");
    }

    /// Cause two: a node had more imports than the fan-out cap. Deliberately
    /// no extra field - the caller re-queries that node with the single-hop
    /// tools' cursor pagination, which already exists.
    #[test]
    fn a_fanout_cut_reports_max_fanout_and_hands_back_no_continuation_field() {
        let mut conn = setup();
        upsert_node(&mut conn, file("a.rs")).unwrap();
        for path in ["b.rs", "c.rs", "d.rs"] {
            upsert_node(&mut conn, file(path)).unwrap();
            imports(&mut conn, "a.rs", path);
        }

        let params = GetDependenciesParams { max_fanout: Some(1), ..anchored_at("a.rs", Direction::Outgoing) };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());

        assert_eq!(body["results"].as_array().unwrap().len(), 1, "one of the three imports, and a warning");
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "maxFanout");
        assert_eq!(body["frontierNodes"].as_array().unwrap().len(), 0, "a fanout cut is paginated, not re-rooted");
        assert!(body["resumeToken"].is_null());
    }

    /// Cause three, and the only one with state to carry: the internal
    /// budget - not a caller-facing limit - stops the walk mid-way, and the
    /// token it hands back continues it exactly where it left off.
    ///
    /// `DEFAULT_EXPLORATION_BUDGET` rows (5000) of `DependencyNode` JSON is
    /// nowhere near `pagination::MAX_RESPONSE_BYTES` (20,000 bytes), so a
    /// walk wide enough to hit the exploration budget always earns its own,
    /// stricter `responseSize` cut before `explorationBudget` is ever visible
    /// on the wire - see `bound_walk`'s doc comment. That row-count-scale
    /// case is exercised directly at the `traversal` layer instead
    /// (`graph::traversal::tests::exploration_budget_caps_visited_rows_...`,
    /// `..._a_resume_chain_covers_the_whole_walk_exactly_once`); what this
    /// tool-level test proves is the same "no continuation is dropped or
    /// double-counted" property one layer up, at the response-size scale
    /// (`bound_walk`'s `prior_visited`/`prior_walked` accumulation) that
    /// callers actually see.
    #[test]
    fn a_response_size_cut_is_continued_by_its_token_and_the_chain_covers_everything_once() {
        // Comfortably past what one response can return, comfortably short
        // of the exploration budget - so nothing but the byte cap can be
        // what's cutting each call in this chain.
        let wide = 600;
        let mut conn = setup();
        let mut diff = Diff { upsert_nodes: vec![file("a.rs")], ..Default::default() };
        for i in 0..wide {
            let path = format!("dep{i:05}.rs");
            diff.upsert_edges.push(EdgeRecord::new(format!("e{i:05}"), "a.rs", &path, "IMPORTS", "tree-sitter", true));
            diff.upsert_nodes.push(file(&path));
        }
        write::apply_diff(&mut conn, &diff).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = GetDependenciesParams { max_fanout: Some(10_000), ..anchored_at("a.rs", Direction::Outgoing) };
        let first = json_body(&handle(&conn, params).unwrap());

        let first_len = first["results"].as_array().unwrap().len();
        assert!(first_len > 0 && first_len < wide, "one response must not hold all {wide} dependencies: {first_len}");
        assert_eq!(first["truncated"], true);
        assert_eq!(first["truncatedBy"], "responseSize");
        assert_eq!(first["frontierNodes"].as_array().unwrap().len(), 0, "a size cut is resumed, not re-rooted");

        let mut all: Vec<String> =
            first["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
        let mut token = first["resumeToken"].as_str().map(str::to_string);
        let mut calls = 1;

        while let Some(t) = token {
            let resumed = GetDependenciesParams {
                file_path: None,
                module_id: None,
                // Ignored on a continuation: the token carries the walk's shape.
                direction: Direction::Incoming,
                max_depth: None,
                max_fanout: None,
                resume_token: Some(t),
            };
            let body = json_body(&handle(&conn, resumed).unwrap());
            calls += 1;
            all.extend(body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()));
            token = body["resumeToken"].as_str().map(str::to_string);
            assert!(calls < 50, "the chain must converge, not re-explore itself forever: {calls} calls so far");
        }

        assert!(calls > 2, "a page far smaller than {wide} deps must take more than one resume: only {calls} calls");

        let mut deduped = all.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), all.len(), "no dependency may be returned twice across the chain");
        assert_eq!(deduped.len(), wide, "the whole chain's union must be every dependency, exactly once");
    }

    /// Reproduces the shape of the two real `get_dependencies` failures in
    /// g-mesh-bench's v0.4.0 outlier findings (a shared module's `Incoming`
    /// fan-in producing a 115,863-character response the MCP client's
    /// transport rejected outright) as a synthetic fixture: a single file
    /// many other files import, none of it anywhere near the exploration
    /// budget or a caller-set `max_fanout`, but still too much JSON for one
    /// response.
    #[test]
    fn a_wide_fan_in_too_big_for_one_response_truncates_with_a_resume_token_instead_of_erroring() {
        let wide = 400;
        let mut conn = setup();
        let core = "packages/core/src/index.ts";
        let mut diff = Diff { upsert_nodes: vec![file(core)], ..Default::default() };
        for i in 0..wide {
            let path = format!("packages/consumer{i:05}/src/index.ts");
            diff.upsert_edges.push(EdgeRecord::new(format!("e{i:05}"), &path, core, "IMPORTS", "tree-sitter", true));
            diff.upsert_nodes.push(file(&path));
        }
        write::apply_diff(&mut conn, &diff).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = GetDependenciesParams { max_fanout: Some(10_000), ..anchored_at(core, Direction::Incoming) };
        let body = json_body(&handle(&conn, params).unwrap());

        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty(), "at least one row must always come back, even under an oversized level");
        assert!(results.len() < wide, "the full {wide}-wide fan-in must not fit in one response");
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "responseSize");
        let raw_len = serde_json::to_vec(results).unwrap().len();
        assert!(raw_len <= pagination::MAX_RESPONSE_BYTES, "the truncated page itself must respect the budget: {raw_len}");

        let token = body["resumeToken"].as_str().expect("a size cut must carry a resume token").to_string();
        let resumed = GetDependenciesParams {
            file_path: None,
            module_id: None,
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: Some(token),
        };
        let second = json_body(&handle(&conn, resumed).unwrap());
        assert!(
            !second["results"].as_array().unwrap().is_empty(),
            "resuming must make forward progress on what the first call dropped"
        );
    }

    /// Problem 2's fix: omitting `max_depth` must stop at this tool's own,
    /// stricter default - not fall through to the walk engine's generic one
    /// (`traversal::DEFAULT_MAX_DEPTH`, 5). A caller that passes `max_depth`
    /// explicitly must still get exactly that depth, unaffected.
    #[test]
    fn omitting_max_depth_uses_this_tools_own_default_not_the_walk_engines() {
        let mut conn = setup();
        let chain = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
        for path in chain {
            upsert_node(&mut conn, file(path)).unwrap();
        }
        for pair in chain.windows(2) {
            imports(&mut conn, pair[0], pair[1]);
        }
        let conn = Arc::new(Mutex::new(conn));

        let defaulted = json_body(&handle(&conn, anchored_at("a.rs", Direction::Outgoing)).unwrap());
        assert_eq!(
            reached(&defaulted),
            vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)],
            "omitting max_depth must stop at DEFAULT_MAX_DEPTH (2), not the walk engine's default (5)"
        );
        assert_eq!(defaulted["truncated"], true);
        assert_eq!(defaulted["truncatedBy"], "maxDepth");
        assert_eq!(defaulted["frontierNodes"], serde_json::json!(["c.rs"]));

        let explicit = GetDependenciesParams { max_depth: Some(4), ..anchored_at("a.rs", Direction::Outgoing) };
        let body = json_body(&handle(&conn, explicit).unwrap());
        assert_eq!(
            reached(&body),
            vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2), ("d.rs".to_string(), 3), ("e.rs".to_string(), 4)],
            "an explicit max_depth must be honored exactly, unaffected by this tool's own default"
        );
        assert_eq!(body["truncated"], false);
    }

    /// The caller asked about a package or a directory, which is what the
    /// prompt they are answering names. A bare refusal sends them hunting with
    /// Glob for the entry point - a round trip, and the recorded trace for
    /// `ex-deps-package-math-incoming` is exactly that hunt.
    #[test]
    fn a_directory_prefix_is_told_which_indexed_files_sit_under_it() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/angle.ts")).unwrap();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

        let message = no_file_message(&conn, "packages/math").unwrap();

        assert!(message.contains("packages/math/src/index.ts"), "{message}");
        // Entry point first: it is what a package specifier resolves to, and
        // what the caller is going to ask about next.
        let idx = message.find("packages/math/src/index.ts").unwrap();
        let other = message.find("packages/math/src/angle.ts").unwrap();
        assert!(idx < other, "the entry point must lead: {message}");
    }

    /// A workspace package name is not a path at all, so the only handle is
    /// its last segment matching a directory - offered as a suggestion, since
    /// a directory of that name does not establish the package lives there.
    #[test]
    fn a_package_name_is_offered_the_directory_that_shares_its_last_segment() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

        let message = no_file_message(&conn, "@excalidraw/math").unwrap();

        assert!(message.contains("packages/math/src/index.ts"), "{message}");
        assert!(message.contains("If 'math' is"), "must read as a suggestion: {message}");
    }

    /// A path matching nothing keeps the short answer. The explanation is only
    /// worth its length where there is something to explain.
    #[test]
    fn a_path_under_which_nothing_is_indexed_keeps_the_terse_answer() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

        let message = no_file_message(&conn, "packages/nowhere").unwrap();

        assert_eq!(message, "g-mesh: no file 'packages/nowhere' found in the index");
    }

    /// Callers put a path in `module_id` - the field reads as "the module's
    /// name" and is documented as the alternative to `file_path`. Every
    /// recorded run of the benchmark task that asks about a package did it,
    /// and paid a refusal plus a blind Glob for the label.
    #[test]
    fn a_path_passed_as_a_module_id_is_answered_rather_than_refused() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
        upsert_node(&mut conn, file("packages/excalidraw/viewport.ts")).unwrap();
        imports(&mut conn, "packages/excalidraw/viewport.ts", "packages/math/src/index.ts");

        let result = from_module(
            &conn,
            "packages/math/src/index.ts",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap();

        let body = json_body(&result);
        assert_eq!(body["results"][0]["filePath"], "packages/excalidraw/viewport.ts");
    }
}
