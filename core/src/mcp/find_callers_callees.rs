//! Real logic behind the `find_callers`/`find_callees` MCP tools. Same shape
//! as `find_references` - anchor lookup, then `paginate_edges` over one edge
//! kind - except the edge kind is `CALLS` and the direction to walk flips
//! depending on which tool is asking: callers are the `Incoming` `CALLS`
//! edges (who calls the anchor), callees are the `Outgoing` ones (what the
//! anchor calls). Both are single-hop by design; the transitive walk lives in
//! `get_dependencies`, not here.

use std::sync::{Arc, Mutex};

use anyhow::Context;
use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, Direction};
use crate::graph::queries;
use crate::storage::write::NodeRecord;

use super::tool_result::{internal_error, success};
use super::{anchor, SymbolQueryParams};

/// One "other end of a CALLS edge" record, plus whether that edge is
/// `resolved`. Direction-agnostic on purpose: `list_calls` doesn't know
/// whether it's resolving a caller or a callee, only which node id sits at
/// the non-anchor end of each edge. The two `handle_*` functions attach the
/// role-specific field name (`callerSymbolId` vs `calleeSymbolId`) when they
/// serialize.
struct CallSite {
    node: NodeRecord,
    resolved: bool,
    /// Same rule `paginate_edges`' SQL applies: `0` when this node shares the
    /// anchor's file, `1` otherwise. Carried through so `handle_callers`/
    /// `handle_callees` can rebuild a resumable cursor if the enriched page
    /// needs further truncation to fit `pagination::MAX_RESPONSE_BYTES`.
    locality: i64,
    edge_id: String,
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
    let page = pagination::paginate_edges(conn, anchor_id, direction, &["CALLS"], file_paths, anchor_file_path, page_size, cursor)
        .context("failed to paginate CALLS edges")?;

    let mut results = Vec::with_capacity(page.results.len());
    for pagination::ScoredEdge { edge, locality } in page.results {
        // Outgoing: anchor is fromId, the callee sits at toId. Incoming: anchor
        // is toId, the caller sits at fromId.
        let other_id = match direction {
            Direction::Outgoing => &edge.to_id,
            Direction::Incoming => &edge.from_id,
        };
        let node = queries::get_node(conn, other_id)
            .context("failed to resolve call-edge endpoint")?
            .with_context(|| format!("edge {} points at missing node {other_id}", edge.id))?;
        results.push(CallSite { node, resolved: edge.resolved, locality, edge_id: edge.id });
    }

    // Intermediate page, ahead of the EdgeRow/bound_page step each handle_*
    // does next - that step computes the real `all_unresolved` marker from
    // each row's `resolved` bit, so this one is a placeholder nothing reads.
    Ok(pagination::Page { results, has_more: page.has_more, next_cursor: page.next_cursor, all_unresolved: false })
}

/// Wire shape for `find_callers`: the calling function on the other end of
/// each inbound `CALLS` edge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerSite {
    caller_symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    resolved: bool,
}

impl From<CallSite> for CallerSite {
    fn from(site: CallSite) -> Self {
        CallerSite {
            caller_symbol_id: site.node.id,
            name: site.node.name,
            qualified_name: site.node.qualified_name,
            kind: site.node.kind,
            file_path: site.node.file_path,
            start_line: site.node.start_line,
            start_col: site.node.start_col,
            resolved: site.resolved,
        }
    }
}

/// Wire shape for `find_callees`: the called function on the other end of
/// each outbound `CALLS` edge.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleeSite {
    callee_symbol_id: String,
    name: String,
    qualified_name: String,
    kind: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    resolved: bool,
}

impl From<CallSite> for CalleeSite {
    fn from(site: CallSite) -> Self {
        CalleeSite {
            callee_symbol_id: site.node.id,
            name: site.node.name,
            qualified_name: site.node.qualified_name,
            kind: site.node.kind,
            file_path: site.node.file_path,
            start_line: site.node.start_line,
            start_col: site.node.start_col,
            resolved: site.resolved,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CallerPage {
    results: Vec<CallerSite>,
    has_more: bool,
    next_cursor: Option<String>,
    /// See `Page::all_unresolved` - true when every caller in `results` came
    /// from an edge the linker couldn't confirm.
    all_unresolved: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CalleePage {
    results: Vec<CalleeSite>,
    has_more: bool,
    next_cursor: Option<String>,
    /// See `Page::all_unresolved` - true when every callee in `results` came
    /// from an edge the linker couldn't confirm.
    all_unresolved: bool,
}

pub(super) fn handle_callers(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = match anchor::resolve(&conn, &params)? {
        Ok(node) => node,
        Err(finished) => return Ok(finished),
    };

    let page_size = pagination::resolve_page_size(params.limit);
    let file_paths: Vec<&str> = params.file_paths.iter().flatten().map(String::as_str).collect();
    let page = list_calls(&conn, &anchor.id, &anchor.file_path, &file_paths, Direction::Incoming, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find callers", e))?;

    let rows = page
        .results
        .into_iter()
        .map(|site| {
            let (resolved, locality, edge_id) = (site.resolved, site.locality, site.edge_id.clone());
            pagination::EdgeRow { item: CallerSite::from(site), resolved, locality, edge_id }
        })
        .collect();
    let bounded = pagination::bound_page(rows, page.has_more, page.next_cursor);

    success(&CallerPage {
        results: bounded.results,
        has_more: bounded.has_more,
        next_cursor: bounded.next_cursor,
        all_unresolved: bounded.all_unresolved,
    })
}

pub(super) fn handle_callees(conn: &Arc<Mutex<Connection>>, params: SymbolQueryParams) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();

    let anchor = match anchor::resolve(&conn, &params)? {
        Ok(node) => node,
        Err(finished) => return Ok(finished),
    };

    let page_size = pagination::resolve_page_size(params.limit);
    let file_paths: Vec<&str> = params.file_paths.iter().flatten().map(String::as_str).collect();
    let page = list_calls(&conn, &anchor.id, &anchor.file_path, &file_paths, Direction::Outgoing, page_size, params.cursor.as_deref())
        .map_err(|e| internal_error("failed to find callees", e))?;

    let rows = page
        .results
        .into_iter()
        .map(|site| {
            let (resolved, locality, edge_id) = (site.resolved, site.locality, site.edge_id.clone());
            pagination::EdgeRow { item: CalleeSite::from(site), resolved, locality, edge_id }
        })
        .collect();
    let bounded = pagination::bound_page(rows, page.has_more, page.next_cursor);

    success(&CalleePage {
        results: bounded.results,
        has_more: bounded.has_more,
        next_cursor: bounded.next_cursor,
        all_unresolved: bounded.all_unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::EdgeRecord;

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

    #[test]
    fn find_callers_of_b_returns_exactly_a_not_c_not_itself() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };
        let result = handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "B has exactly one caller, not the transitive chain");
        assert_eq!(results[0]["callerSymbolId"], "a");
    }

    #[test]
    fn find_callees_of_b_returns_exactly_c_not_a() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() };
        let result = handle_callees(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "B has exactly one callee, not the transitive chain");
        assert_eq!(results[0]["calleeSymbolId"], "c");
    }

    #[test]
    fn callers_of_a_root_is_an_empty_page_not_an_error() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("a".to_string()), ..Default::default() };
        let result = handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap();
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
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", false)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", false)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(&handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 2);
        assert_eq!(body["allUnresolved"], true, "every caller unresolved must set the response-level marker");
    }

    #[test]
    fn a_page_with_at_least_one_resolved_caller_is_not_flagged_all_unresolved() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", false)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "CALLS", "tree-sitter", false)).unwrap();

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
        let body = json_body(&handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 3);
        assert_eq!(body["allUnresolved"], false, "one resolved row among several unresolved ones must clear the marker");
    }

    #[test]
    fn callees_of_a_leaf_is_an_empty_page_not_an_error() {
        let conn = setup_chain();
        let params = SymbolQueryParams { symbol_id: Some("c".to_string()), ..Default::default() };
        let result = handle_callees(&Arc::new(Mutex::new(conn)), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["results"].as_array().unwrap().len(), 0);
        assert_eq!(body["hasMore"], false);
    }

    /// Both directions must accept the name form: the anchor lookup is
    /// shared, but the two handlers call it separately.
    #[test]
    fn an_unambiguous_symbol_name_anchors_both_directions_without_a_symbol_id() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let callers = json_body(
            &handle_callers(&conn, SymbolQueryParams { symbol_name: Some("b".to_string()), ..Default::default() })
                .unwrap(),
        );
        assert_eq!(callers["results"].as_array().unwrap().len(), 1);
        assert_eq!(callers["results"][0]["callerSymbolId"], "a");

        let callees = json_body(
            &handle_callees(&conn, SymbolQueryParams { symbol_name: Some("b".to_string()), ..Default::default() })
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
        upsert_node(&mut conn, NodeRecord::new("run_a", "Function", "run", "pkg_a::run", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("run_b", "Function", "run", "pkg_b::run", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "ca", "pkg::ca", "ca.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "cb", "pkg::cb", "cb.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "run_a", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "run_b", "CALLS", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let ambiguous = json_body(
            &handle_callers(&conn, SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() })
                .unwrap(),
        );
        assert_eq!(ambiguous["ambiguous"], true, "the candidate page must be distinguishable from a results page");
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
        let picked = candidates.iter().find(|c| c["qualifiedName"] == "pkg_b::run").expect("candidate pkg_b::run");
        let params = SymbolQueryParams {
            symbol_id: Some(picked["id"].as_str().expect("a candidate carries its id").to_string()),
            ..Default::default()
        };
        let body = json_body(&handle_callers(&conn, params).unwrap());
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
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "cb", "pkg::cb", "cb.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "run_b", "CALLS", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let by_name = SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() };
        let ambiguous = json_body(&handle_callers(&conn, by_name).unwrap());
        assert_eq!(ambiguous["ambiguous"], true);

        // Re-asking by qualifiedName here is the loop: it is the same query.
        let requalified =
            SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() };
        assert_eq!(json_body(&handle_callers(&conn, requalified).unwrap())["ambiguous"], true);

        let mut ids: Vec<&str> =
            ambiguous["results"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
        ids.sort();
        assert_eq!(ids, vec!["run_a", "run_b"], "the ids must survive even when nothing else tells them apart");

        let params = SymbolQueryParams { symbol_id: Some("run_b".to_string()), ..Default::default() };
        let body = json_body(&handle_callers(&conn, params).unwrap());
        assert_eq!(body["results"].as_array().unwrap()[0]["callerSymbolId"], "caller_b");
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error_for_callers() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle_callers(&Arc::new(Mutex::new(conn)), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn unknown_symbol_id_is_a_tool_level_error_for_callees() {
        let conn = setup();
        let params = SymbolQueryParams { symbol_id: Some("does_not_exist".to_string()), ..Default::default() };
        let result = handle_callees(&Arc::new(Mutex::new(conn)), params).unwrap();
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
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("caller_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "caller_a", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "caller_b", "target", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "caller_c", "target", "CALLS", "tree-sitter", true)).unwrap();

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = list_calls(&conn, "target", "target.rs", &[], Direction::Incoming, 1, cursor.as_deref()).unwrap();
            assert_eq!(page.results.len(), 1, "page size of 1 must return exactly one result per page");
            seen.extend(page.results.into_iter().map(|c| c.node.id));
            if !page.has_more {
                break;
            }
            cursor = page.next_cursor;
        }

        seen.sort();
        assert_eq!(seen, vec!["caller_a", "caller_b", "caller_c"], "all three callers must come back, once each");
    }

    /// Same membership-test shape as `find_references`'s benchmark repro, but
    /// for `find_callers`: three known files call the target, a fourth known
    /// file
    /// doesn't, and an out-of-scope file also calls it - only the three
    /// in-scope callers must come back.
    #[test]
    fn scoping_find_callers_to_a_known_file_set_returns_only_the_callers_within_it() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();

        let known_files = ["k1.rs", "k2.rs", "k3.rs", "k4.rs"];
        for (i, file) in known_files.iter().enumerate() {
            let id = format!("known_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, format!("pkg::{id}"), *file, "rust")).unwrap();
            if i < 3 {
                upsert_edge(&mut conn, EdgeRecord::new(format!("e_known_{i}"), &id, "target", "CALLS", "tree-sitter", true))
                    .unwrap();
            }
        }
        upsert_node(&mut conn, NodeRecord::new("outsider", "Function", "outsider", "pkg::outsider", "unrelated.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_outsider", "outsider", "target", "CALLS", "tree-sitter", true)).unwrap();

        let conn = Arc::new(Mutex::new(conn));
        let params = SymbolQueryParams {
            symbol_id: Some("target".to_string()),
            file_paths: Some(known_files.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };
        let body = json_body(&handle_callers(&conn, params).unwrap());
        let mut file_paths: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        file_paths.sort();
        assert_eq!(file_paths, vec!["k1.rs", "k2.rs", "k3.rs"], "must return exactly the known files that call the target");
    }

    /// Omitting `file_paths` must be indistinguishable from a call made
    /// before this parameter existed, and an explicit empty array must
    /// answer identically to omitting it - same convention `edge_kinds`
    /// already uses for "no filter".
    #[test]
    fn omitting_file_paths_and_an_explicit_empty_array_both_behave_like_no_scope_for_callers() {
        let conn = Arc::new(Mutex::new(setup_chain()));

        let omitted = json_body(
            &handle_callers(&conn, SymbolQueryParams { symbol_id: Some("b".to_string()), ..Default::default() }).unwrap(),
        );
        let explicit_empty = json_body(
            &handle_callers(
                &conn,
                SymbolQueryParams { symbol_id: Some("b".to_string()), file_paths: Some(Vec::new()), ..Default::default() },
            )
            .unwrap(),
        );
        assert_eq!(omitted, explicit_empty, "an explicit empty file_paths array must answer exactly as omitting it does");
        assert_eq!(omitted["results"].as_array().unwrap().len(), 1);
    }

    /// A caller-supplied `limit` above the default page size must actually
    /// reach `paginate_edges`, not just be accepted and ignored. Proven for
    /// callers only, same reasoning as the small-page-size test above.
    #[test]
    fn a_custom_limit_returns_more_than_the_default_page_in_one_call() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        for i in 0..25 {
            let id = format!("caller_{i}");
            upsert_node(&mut conn, NodeRecord::new(&id, "Function", &id, format!("pkg::{id}"), "a.rs", "rust")).unwrap();
            upsert_edge(&mut conn, EdgeRecord::new(format!("e_{i}"), &id, "target", "CALLS", "tree-sitter", true)).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let params = SymbolQueryParams { symbol_id: Some("target".to_string()), limit: Some(25), ..Default::default() };
        let body = json_body(&handle_callers(&conn, params).unwrap());
        assert_eq!(body["results"].as_array().unwrap().len(), 25, "all 25 must come back in one page");
        assert_eq!(body["hasMore"], false);
    }

    #[test]
    fn handle_callees_paginates_across_cursor_continuation() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("target", "Function", "run", "pkg::run", "target.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_a", "Function", "a", "pkg::a", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_b", "Function", "b", "pkg::b", "b.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("callee_c", "Function", "c", "pkg::c", "c.rs", "rust")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_a", "target", "callee_a", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_b", "target", "callee_b", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_c", "target", "callee_c", "CALLS", "tree-sitter", true)).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let mut seen = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = SymbolQueryParams { symbol_id: Some("target".to_string()), cursor: cursor.clone(), ..Default::default() };
            let result = handle_callees(&conn, params).unwrap();
            let body = json_body(&result);
            let results = body["results"].as_array().unwrap().clone();
            seen.extend(results.iter().map(|r| r["calleeSymbolId"].as_str().unwrap().to_string()));

            if body["hasMore"] == false {
                break;
            }
            cursor = body["nextCursor"].as_str().map(|s| s.to_string());
        }

        seen.sort();
        assert_eq!(seen, vec!["callee_a", "callee_b", "callee_c"], "all three callees must come back, once each");
    }
}
