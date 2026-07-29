//! Real logic behind the `get_dependencies` MCP tool - the one MVP tool whose
//! answer is genuinely transitive rather than a single hop. The walk itself,
//! and the whole truncation contract around it, already live in
//! `graph::traversal`; this module is the "anchor -> bounded `IMPORTS` walk ->
//! JSON" wiring around it, plus the two decisions `traversal` deliberately
//! leaves to its caller: which edge kind to follow, and how much of the
//! result is the caller's business.

use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::Direction;
use crate::graph::queries;
use crate::graph::traversal::{self, ReachedNode, TraversalOptions, TraversalResult, TruncatedBy};

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

/// One reached dependency, with how many import hops away it is. No
/// `resolved` flag, unlike the single-hop tools: a node several hops out is
/// reached over a *path* of edges, and one flag can only describe one of
/// them, so it would read as a trust claim about the path that it does not
/// make. The edge list the walk collects is left out for the same reason it
/// isn't needed here - impact analysis asks which files an edit reaches, not
/// by which route.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyNode {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
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
        let file_path = (r.node.kind != MODULE_KIND).then_some(r.node.file_path);
        Self {
            id: r.node.id,
            kind: r.node.kind,
            name: r.node.name,
            qualified_name: r.node.qualified_name,
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

impl From<TraversalResult> for DependencyWalk {
    fn from(result: TraversalResult) -> Self {
        Self {
            results: result.nodes.into_iter().filter(|r| r.depth > 0).map(DependencyNode::from).collect(),
            truncated: result.truncated,
            truncated_by: result.truncated_by.map(wire_name),
            frontier_nodes: result.frontier_nodes,
            resume_token: result.resume_token,
        }
    }
}

/// The wire spelling of each truncation cause, fixed by the contract in
/// `docs/architecture/g-mesh-v1.md`.
fn wire_name(cause: TruncatedBy) -> &'static str {
    match cause {
        TruncatedBy::MaxDepth => "maxDepth",
        TruncatedBy::MaxFanout => "maxFanout",
        TruncatedBy::ExplorationBudget => "explorationBudget",
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
    if let Some(max_depth) = shape.max_depth {
        options.max_depth = max_depth;
    }
    if let Some(max_fanout) = shape.max_fanout {
        options.max_fanout = max_fanout;
    }

    let result =
        traversal::traverse(conn, options).map_err(|e| internal_error("failed to walk the import graph", e))?;
    success(&DependencyWalk::from(result))
}

fn from_file(conn: &Connection, file_path: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    let anchor = queries::find_file_node(conn, file_path).map_err(|e| internal_error("failed to look up file", e))?;

    match anchor {
        Some(node) => from_root(conn, node.id, shape),
        None => error(format!("g-mesh: no file '{file_path}' found in the index")),
    }
}

/// A module id is already a node id, so this lookup buys nothing but the
/// error message: an unknown id would otherwise walk nothing at all and read
/// as "this module imports nothing", which is the one answer a bounded walk
/// must never fake.
fn from_module(conn: &Connection, module_id: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    let anchor = queries::get_node(conn, module_id).map_err(|e| internal_error("failed to look up module", e))?;

    match anchor {
        Some(node) => from_root(conn, node.id, shape),
        None => error(format!("g-mesh: no module '{module_id}' found in the index")),
    }
}

/// Continues a walk the exploration budget cut short. The token carries the
/// anchor, direction and limits of the walk it continues, so nothing about
/// its shape is re-read from the parameters here.
fn continued(conn: &Connection, token: &str) -> Result<CallToolResult, ErrorData> {
    let result = traversal::resume(conn, token, traversal::DEFAULT_EXPLORATION_BUDGET)
        .map_err(|e| internal_error("failed to resume the import walk", e))?;
    success(&DependencyWalk::from(result))
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
    #[test]
    fn a_budget_cut_reports_exploration_budget_and_is_continued_by_its_token() {
        // Wider than the internal budget, with the caller's own limits held
        // open, so nothing but the budget can be what cuts this walk.
        let over_budget = traversal::DEFAULT_EXPLORATION_BUDGET as usize + 100;
        let mut conn = setup();
        let mut diff = Diff { upsert_nodes: vec![file("a.rs")], ..Default::default() };
        for i in 0..over_budget {
            let path = format!("dep{i:05}.rs");
            diff.upsert_edges.push(EdgeRecord::new(format!("e{i:05}"), "a.rs", &path, "IMPORTS", "tree-sitter", true));
            diff.upsert_nodes.push(file(&path));
        }
        write::apply_diff(&mut conn, &diff).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = GetDependenciesParams {
            max_fanout: Some(1_000_000),
            max_depth: Some(10),
            ..anchored_at("a.rs", Direction::Outgoing)
        };
        let first = json_body(&handle(&conn, params).unwrap());

        // The budget counts the anchor's own row too, so one fewer dependency
        // than the budget fits into the first call.
        let first_len = first["results"].as_array().unwrap().len();
        assert_eq!(first_len, traversal::DEFAULT_EXPLORATION_BUDGET as usize - 1);
        assert_eq!(first["truncated"], true);
        assert_eq!(first["truncatedBy"], "explorationBudget");
        assert_eq!(first["frontierNodes"].as_array().unwrap().len(), 0, "a budget cut has no depth boundary");
        let token = first["resumeToken"].as_str().expect("only a budget cut carries a token").to_string();

        let resumed = GetDependenciesParams {
            file_path: None,
            module_id: None,
            // Ignored on a continuation: the token carries the walk's shape.
            direction: Direction::Incoming,
            max_depth: None,
            max_fanout: None,
            resume_token: Some(token),
        };
        let second = json_body(&handle(&conn, resumed).unwrap());

        assert_eq!(second["results"].as_array().unwrap().len(), over_budget - first_len, "the rest, and only the rest");
        assert_eq!(second["truncated"], false, "the continuation ran out of graph, not of budget");
        assert!(second["truncatedBy"].is_null());
        assert!(second["resumeToken"].is_null());

        let mut all: Vec<String> = first["results"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["results"].as_array().unwrap().iter())
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        all.sort();
        all.dedup();
        assert_eq!(all.len(), over_budget, "the two calls together cover every dependency exactly once");
    }
}
