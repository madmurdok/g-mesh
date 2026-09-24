use super::*;
use crate::graph::queries::{upsert_edge, upsert_node};
use crate::storage::schema;
use crate::storage::write::{EdgeRecord, NodeRecord};

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

/// Sets up the acceptance criteria's chain: ClassA implements Interface,
/// ClassB extends ClassA. Edge direction per `SUPERTYPE_OF`'s subtype ->
/// supertype convention: ClassA -> Interface, ClassB -> ClassA.
fn setup_chain() -> Connection {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("class_a", "Type", "ClassA", "pkg::ClassA", "a.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("class_b", "Type", "ClassB", "pkg::ClassB", "b.rs", "rust"))
        .unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_a_iface", "class_a", "interface", "SUPERTYPE_OF", "tree-sitter", true),
    )
    .unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_b_a", "class_b", "class_a", "SUPERTYPE_OF", "tree-sitter", true),
    )
    .unwrap();
    conn
}

#[test]
fn find_implementations_of_interface_returns_exactly_class_a_not_class_b() {
    let conn = setup_chain();
    let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
    let result =
        handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap();
    let body = json_body(&result);
    let results = body["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "the interface has exactly one direct implementor, not the transitive subclass"
    );
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
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    upsert_node(
        &mut conn,
        NodeRecord::new("file", "File", "weird.rs", "src/weird.rs", "src/weird.rs", "rust"),
    )
    .unwrap();
    upsert_node(&mut conn, NodeRecord::new("class_a", "Type", "ClassA", "pkg::ClassA", "a.rs", "rust"))
        .unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_file", "file", "interface", "SUPERTYPE_OF", "tree-sitter", true),
    )
    .unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_class", "class_a", "interface", "SUPERTYPE_OF", "tree-sitter", true),
    )
    .unwrap();

    let page = list_implementations(&conn, "interface", "iface.rs", &[], 10, None).unwrap();
    assert_eq!(page.results.len(), 2);

    let file_row = page.results.iter().find(|r| r.kind == "File").expect("the File-kind row must be present");
    assert!(file_row.qualified_name.is_none(), "qualifiedName duplicates filePath for a File-kind row");
    assert!(file_row.start_line.is_none(), "startLine is meaningless for a File-kind row");
    assert!(file_row.start_col.is_none(), "startCol is meaningless for a File-kind row");
    assert_eq!(file_row.file_path, "src/weird.rs");

    let symbol_row =
        page.results.iter().find(|r| r.kind == "Type").expect("the Type-kind row must be present");
    assert_eq!(
        symbol_row.qualified_name.as_deref(),
        Some("pkg::ClassA"),
        "a symbol-kind row must still carry its qualifiedName"
    );
    assert_eq!(symbol_row.start_line, Some(0), "a symbol-kind row must still carry its startLine");
    assert_eq!(symbol_row.start_col, Some(0), "a symbol-kind row must still carry its startCol");
}

#[test]
fn zero_implementations_is_an_empty_page_not_an_error() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();

    let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
    let result =
        handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap();
    let body = json_body(&result);
    assert_eq!(body["results"].as_array().unwrap().len(), 0);
    assert_eq!(body["hasMore"], false);
    assert_eq!(body["allUnresolved"], false, "an empty page has nothing to be suspicious of");
}

#[test]
fn a_page_where_every_implementor_is_unresolved_is_flagged_all_unresolved() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_a", "impl_a", "interface", "SUPERTYPE_OF", "tree-sitter", false),
    )
    .unwrap();

    let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
    let body = json_body(
        &handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap(),
    );
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    assert_eq!(
        body["allUnresolved"], true,
        "every implementor unresolved must set the response-level marker"
    );
}

#[test]
fn a_page_with_at_least_one_resolved_implementor_is_not_flagged_all_unresolved() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_a", "impl_a", "interface", "SUPERTYPE_OF", "tree-sitter", true),
    )
    .unwrap();
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_b", "impl_b", "interface", "SUPERTYPE_OF", "tree-sitter", false),
    )
    .unwrap();

    let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
    let body = json_body(
        &handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap(),
    );
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
    assert_eq!(body["allUnresolved"], false, "one resolved row must clear the marker");
}

/// Task #190: the single-hop response echoes the resolved anchor.
#[test]
fn the_single_hop_response_echoes_the_resolved_anchor() {
    let conn = setup_chain();
    let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
    let body = json_body(
        &handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap(),
    );
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
    let body =
        json_body(&dispatch(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap());
    assert_eq!(body["anchor"]["id"], "interface");
    assert_eq!(body["anchor"]["qualifiedName"], "pkg::Iface");
    // The rung travels with the anchor, and this path is where it was
    // dropped: `dispatch` resolved it for the transitive walk and threw
    // it away, so `transitive: true` answered without a `resolvedBy`
    // while the single-hop path beside it carried one. The compiler said
    // so - an unused-variable warning - which is the only reason it was
    // caught, since every assertion here passed without it.
    assert_eq!(body["anchor"]["resolvedBy"], "id");
}

/// The rung has to be the one that actually reached the anchor, not a
/// constant: asserting only `resolvedBy`'s presence would pass just as
/// well if `from_root` hardcoded a value.
#[test]
fn a_transitive_walk_reports_the_rung_that_reached_its_anchor() {
    let conn = Arc::new(Mutex::new(setup_chain()));
    let params = FindImplementationsParams {
        symbol_name: Some("pkg::Iface".to_string()),
        transitive: Some(true),
        ..Default::default()
    };
    let body =
        json_body(&dispatch(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap());
    assert_eq!(body["anchor"]["id"], "interface");
    assert_eq!(body["anchor"]["resolvedBy"], "qualifiedName", "resolved by qualifiedName, not by id");
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
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    for i in 0..wide {
        let id = format!("impl{i:05}");
        upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "impl.rs", "rust"))
            .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new(format!("e{i:05}"), &id, "interface", "SUPERTYPE_OF", "tree-sitter", true),
        )
        .unwrap();
    }

    let anchor_node = queries::get_node(&conn, "interface").unwrap().expect("anchor node must exist");
    let anchor_info = anchor::AnchorInfo::from(&anchor_node);

    let mut options = TraversalOptions::new("interface", Direction::Incoming);
    options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
    options.max_fanout = 10_000;
    let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);
    let result = traversal::traverse(&conn, options).unwrap();
    let first = bound_walk(
        result,
        max_depth,
        max_fanout,
        WalkFraming { anchor: Some(anchor_info), hint: None, provenance: None },
        Vec::new(),
        Vec::new(),
    );
    assert!(first.anchor.is_some(), "the first page of a fresh walk still carries the anchor");
    let token = first.resume_token.expect("this wide a fanout must truncate and hand back a token");

    let resumed = json_body(&continued(&conn, &token).unwrap());
    assert!(
        resumed.get("anchor").is_none(),
        "a resumed page must not repeat the anchor, not even as null: {resumed}"
    );
}

/// Anchoring by name must reach the same node the id does - here the
/// interface, whose one direct implementor is `class_a`.
#[test]
fn an_unambiguous_symbol_name_anchors_the_walk_without_a_symbol_id() {
    let conn = Arc::new(Mutex::new(setup_chain()));

    let by_id = json_body(
        &handle(
            &conn,
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
            SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() },
        )
        .unwrap(),
    );
    let by_name = json_body(
        &handle(
            &conn,
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
            SymbolQueryParams { symbol_name: Some("Iface".to_string()), ..Default::default() },
        )
        .unwrap(),
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
    let result =
        handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap();
    assert!(error_text(&result).contains("does_not_exist"));
}

#[test]
fn implemented_by_three_types_returns_all_three_across_small_pages() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_c", "Type", "C", "pkg::C", "c.rs", "rust")).unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "target", "SUPERTYPE_OF", "tree-sitter", true))
        .unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "target", "SUPERTYPE_OF", "tree-sitter", true))
        .unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_c", "impl_c", "target", "SUPERTYPE_OF", "tree-sitter", true))
        .unwrap();

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

/// GM-361: one implementor both tiers found is one row.
///
/// An `edges` row's id is `(fromId, kind, toId)` *per tier*, so a
/// structural edge and the semantic one confirming it are two rows for
/// one fact - which is not an anomaly to be repaired in storage, it is
/// what the `source` column is for. Measured on ripgrep, that turned
/// `find_implementations("Sink")` into a 12-row page describing 7
/// implementors, with `StandardSink`, `JSONSink`, `SummarySink` and
/// `KitchenSink` each listed twice.
#[test]
fn an_implementor_both_tiers_found_is_one_row_not_two() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
    for (id, from, tier) in [
        ("e_a_syn", "impl_a", "tree-sitter"),
        ("e_a_sem", "impl_a", "ts-compiler"),
        ("e_b_syn", "impl_b", "tree-sitter"),
        ("e_b_sem", "impl_b", "ts-compiler"),
    ] {
        upsert_edge(&mut conn, EdgeRecord::new(id, from, "target", "SUPERTYPE_OF", tier, true)).unwrap();
    }

    let params = SymbolQueryParams { symbol_id: Some("target".to_string()), ..Default::default() };
    let result =
        handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap();
    let body = json_body(&result);
    let mut ids: Vec<&str> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["implementingSymbolId"].as_str().unwrap())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["impl_a", "impl_b"], "four edges, two implementors");
    assert_eq!(body["hasMore"], false);
}

/// The de-duplication has to happen *in* the query, not to the rows it
/// returned: collapsing a page after the fact leaves the second edge
/// onto an endpoint free to reappear as the first row of the next page,
/// which is the same duplicate one call later.
#[test]
fn a_duplicated_implementor_does_not_reappear_on_the_next_page() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust"))
        .unwrap();
    for name in ["a", "b", "c"] {
        let id = format!("impl_{name}");
        upsert_node(
            &mut conn,
            NodeRecord::new(&id, "Type", name, format!("pkg::{name}"), format!("{name}.rs"), "rust"),
        )
        .unwrap();
        for tier in ["tree-sitter", "ts-compiler"] {
            let edge = format!("e_{name}_{tier}");
            upsert_edge(&mut conn, EdgeRecord::new(edge, &id, "target", "SUPERTYPE_OF", tier, true)).unwrap();
        }
    }

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = list_implementations(&conn, "target", "target.rs", &[], 1, cursor.as_deref()).unwrap();
        assert_eq!(page.results.len(), 1, "one row per page at page size 1");
        seen.extend(page.results.into_iter().map(|r| r.implementing_symbol_id));
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    seen.sort();
    assert_eq!(seen, vec!["impl_a", "impl_b", "impl_c"], "six edges, three implementors, once each");
}

/// A caller-supplied `limit` above the default page size must actually
/// reach `paginate_edges`, not just be accepted and ignored.
#[test]
fn a_custom_limit_returns_more_than_the_default_page_in_one_call() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust"))
        .unwrap();
    for i in 0..25 {
        let id = format!("impl_{i}");
        upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "a.rs", "rust"))
            .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new(format!("e_{i}"), &id, "target", "SUPERTYPE_OF", "tree-sitter", true),
        )
        .unwrap();
    }
    let conn = Arc::new(Mutex::new(conn));

    let params =
        SymbolQueryParams { symbol_id: Some("target".to_string()), limit: Some(25), ..Default::default() };
    let body = json_body(&handle(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap());
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
    let conn = Arc::new(Mutex::new(conn));

    let params = SymbolQueryParams { symbol_id: Some("file".to_string()), ..Default::default() };
    let body = json_body(&handle(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap());

    let hint = body["hint"].as_str().expect("a File-anchored call must carry a hint");
    assert!(hint.contains("get_dependencies"), "the hint must point at get_dependencies: {hint}");
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        0,
        "a File anchor still answers as an empty page, not an error"
    );
    assert_eq!(body["hasMore"], false);
    assert_eq!(body["allUnresolved"], false);
}

/// Purely additive: an ordinary symbol anchor must never carry the
/// `hint` field at all, not even as `null`.
#[test]
fn a_normal_symbol_anchor_never_carries_a_hint_field() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();

    let params = SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() };
    let body = json_body(
        &handle(&Arc::new(Mutex::new(conn)), &EmbeddingPipeline::disabled(), &no_capabilities(), params)
            .unwrap(),
    );
    assert!(
        body.get("hint").is_none(),
        "hint must be entirely absent, not null, on a normal symbol anchor: {body}"
    );
}

#[test]
fn handle_paginates_across_cursor_continuation() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("target", "Type", "Iface", "pkg::Iface", "target.rs", "rust"))
        .unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_a", "Type", "A", "pkg::A", "a.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_b", "Type", "B", "pkg::B", "b.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("impl_c", "Type", "C", "pkg::C", "c.rs", "rust")).unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_a", "impl_a", "target", "SUPERTYPE_OF", "tree-sitter", true))
        .unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_b", "impl_b", "target", "SUPERTYPE_OF", "tree-sitter", true))
        .unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_c", "impl_c", "target", "SUPERTYPE_OF", "tree-sitter", true))
        .unwrap();
    let conn = Arc::new(Mutex::new(conn));

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let params = SymbolQueryParams {
            symbol_id: Some("target".to_string()),
            cursor: cursor.clone(),
            ..Default::default()
        };
        let result = handle(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap();
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
        &handle(
            &conn,
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
            SymbolQueryParams { symbol_id: Some("interface".to_string()), ..Default::default() },
        )
        .unwrap(),
    );
    let via_dispatch = json_body(
        &dispatch(
            &conn,
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
            FindImplementationsParams { symbol_id: Some("interface".to_string()), ..Default::default() },
        )
        .unwrap(),
    );

    assert_eq!(
        via_dispatch, via_handle,
        "transitive absent must be byte-identical to the pre-existing shape"
    );
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
        &dispatch(
            &conn,
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
            FindImplementationsParams { symbol_id: Some("interface".to_string()), ..Default::default() },
        )
        .unwrap(),
    );
    assert_eq!(absent["results"].as_array().unwrap().len(), 1, "absent transitive must stay single-hop");
    assert_eq!(absent["results"][0]["implementingSymbolId"], "class_a");

    let explicit_false = json_body(
        &dispatch(
            &conn,
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
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
            &EmbeddingPipeline::disabled(),
            &no_capabilities(),
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
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    for id in ["a", "b", "c", "d"] {
        upsert_node(
            &mut conn,
            NodeRecord::new(id, "Type", id, format!("pkg::{id}"), format!("{id}.rs"), "rust"),
        )
        .unwrap();
    }
    upsert_edge(
        &mut conn,
        EdgeRecord::new("e_a_iface", "a", "interface", "SUPERTYPE_OF", "tree-sitter", true),
    )
    .unwrap();
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
    let body =
        json_body(&dispatch(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap());

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
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    for id in ["a", "b", "c"] {
        upsert_node(&mut conn, NodeRecord::new(id, "Type", id, format!("pkg::{id}"), "impl.rs", "rust"))
            .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new(format!("e_{id}"), id, "interface", "SUPERTYPE_OF", "tree-sitter", true),
        )
        .unwrap();
    }

    let mut options = TraversalOptions::new("interface", Direction::Incoming);
    options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
    options.max_fanout = 1;
    let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);
    let result = traversal::traverse(&conn, options).unwrap();
    let walk = bound_walk(
        result,
        max_depth,
        max_fanout,
        WalkFraming { anchor: None, hint: None, provenance: None },
        Vec::new(),
        Vec::new(),
    );

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
    upsert_node(&mut conn, NodeRecord::new("interface", "Type", "Iface", "pkg::Iface", "iface.rs", "rust"))
        .unwrap();
    for i in 0..wide {
        let id = format!("impl{i:05}");
        upsert_node(&mut conn, NodeRecord::new(&id, "Type", &id, format!("pkg::{id}"), "impl.rs", "rust"))
            .unwrap();
        upsert_edge(
            &mut conn,
            EdgeRecord::new(format!("e{i:05}"), &id, "interface", "SUPERTYPE_OF", "tree-sitter", true),
        )
        .unwrap();
    }

    let mut options = TraversalOptions::new("interface", Direction::Incoming);
    options.edge_kind = Some(SUPERTYPE_EDGE.to_string());
    options.max_fanout = 10_000;
    let (max_depth, max_fanout) = (options.max_depth, options.max_fanout);
    let result = traversal::traverse(&conn, options).unwrap();
    let first = bound_walk(
        result,
        max_depth,
        max_fanout,
        WalkFraming { anchor: None, hint: None, provenance: None },
        Vec::new(),
        Vec::new(),
    );

    assert!(!first.results.is_empty(), "at least one row must come back");
    assert!(
        first.results.len() < wide,
        "one response must not hold all {wide} implementors: {}",
        first.results.len()
    );
    assert!(first.truncated);
    assert_eq!(first.truncated_by, Some("responseSize"));
    assert!(first.frontier_nodes.is_empty(), "a size cut is resumed, not re-rooted");

    let mut all: Vec<String> = first.results.iter().map(|r| r.implementing_symbol_id.clone()).collect();
    let mut token = first.resume_token.clone();
    let mut calls = 1;

    while let Some(t) = token {
        let body = json_body(&continued(&conn, &t).unwrap());
        calls += 1;
        all.extend(
            body["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["implementingSymbolId"].as_str().unwrap().to_string()),
        );
        token = body["resumeToken"].as_str().map(str::to_string);
        assert!(calls < 50, "the chain must converge, not re-explore itself forever: {calls} calls so far");
    }

    assert!(
        calls > 2,
        "a page far smaller than {wide} implementors must take more than one resume: only {calls} calls"
    );

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
        &EmbeddingPipeline::disabled(),
        &no_capabilities(),
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
        &EmbeddingPipeline::disabled(),
        &no_capabilities(),
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
        &EmbeddingPipeline::disabled(),
        &no_capabilities(),
        FindImplementationsParams {
            transitive: Some(true),
            resume_token: Some("whatever".to_string()),
            ..Default::default()
        },
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
    let conn = Arc::new(Mutex::new(conn));

    let params = FindImplementationsParams {
        symbol_id: Some("file".to_string()),
        transitive: Some(true),
        ..Default::default()
    };
    let body =
        json_body(&dispatch(&conn, &EmbeddingPipeline::disabled(), &no_capabilities(), params).unwrap());

    let hint = body["hint"].as_str().expect("a File-anchored transitive call must still carry a hint");
    assert!(hint.contains("get_dependencies"), "the hint must point at get_dependencies: {hint}");
    assert_eq!(
        body["results"].as_array().unwrap().len(),
        0,
        "a File anchor still answers as an empty walk, not an error"
    );
    assert_eq!(body["truncated"], false);
    assert!(body["resumeToken"].is_null());
}
