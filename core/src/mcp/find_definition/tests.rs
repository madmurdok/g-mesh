use super::*;
use std::collections::HashMap;

use crate::graph::queries::{upsert_edge, upsert_node};
use crate::protocol::types::QualifiedPath;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::EdgeRecord;

/// A project root with nothing in it, for the tests that are about
/// resolution rather than about source. Every snippet lookup under it
/// misses, so `source` is absent and these assertions read exactly as
/// they did before the field existed - which is the point: adding the
/// field must not quietly change what they are testing.
fn no_sources() -> std::path::PathBuf {
    std::env::temp_dir().join("g-mesh-tests-with-no-sources")
}

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn node_with_span(
    id: &str,
    name: &str,
    qualified_name: &str,
    file_path: &str,
    end: (i64, i64),
) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", name, qualified_name, file_path, "rust");
    node.end_line = end.0;
    node.end_col = end.1;
    node
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

/// An import placeholder, as the Go plugin emits one for `import
/// "context"`: a `Module` row named and qualified after the specifier,
/// sitting on the import line of the file that wrote it.
fn import_placeholder(id: &str, name: &str, specifier: &str, file: &str) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Module", name, specifier, file, "go");
    node.native_kind = Some(crate::graph::imports::EXTERNAL_MODULE_NATIVE_KIND.to_string());
    node.start_line = 8;
    node.end_line = 8;
    node.end_col = 20;
    node
}

fn resolved_by_name(conn: &Connection, name: &str) -> Result<CallToolResult, ErrorData> {
    by_name(conn, None, &SemanticRung::off(), name, None)
}

/// GM-367's measured gin case, in miniature: `context` is an import
/// placeholder and nothing else, and 3.8.0 answered it as a declaration -
/// `resolvedBy: "qualifiedName"`, unflagged, pointing at the import line.
///
/// **Evidence.** With the exclusion reverted in `graph::queries` this
/// fails on the first assertion, resolving to `ph` instead of refusing.
#[test]
fn a_name_carried_only_by_import_placeholders_is_refused_and_says_what_it_is() {
    let mut conn = setup();
    upsert_node(&mut conn, import_placeholder("ph", "context", "context", "app/context_test.go")).unwrap();

    let text = error_text(&resolved_by_name(&conn, "context").unwrap());

    assert!(text.contains("nothing named 'context' is declared"), "{text}");
    assert!(text.contains("'context' (1)"), "the specifier and how many import it: {text}");
    assert!(text.contains("get_dependencies"), "the tool that does answer for it: {text}");
}

/// The other half of the gin case: `net/http` is the qualifiedName of 63
/// placeholders, which 3.8.0 offered as a ranked page of 20 candidates
/// that no caller can re-query into a definition. Three stands in for 63.
///
/// **Evidence.** With the exclusion reverted this fails: the answer is a
/// candidate page (`ambiguous: true`), not an error.
#[test]
fn a_specifier_carried_by_many_import_placeholders_is_not_a_candidate_page() {
    let mut conn = setup();
    for (i, file) in ["a.go", "b.go", "c.go"].iter().enumerate() {
        upsert_node(&mut conn, import_placeholder(&format!("ph{i}"), "http", "net/http", file)).unwrap();
    }

    for query in ["http", "net/http"] {
        let text = error_text(&resolved_by_name(&conn, query).unwrap());
        assert!(text.contains("'net/http' (3)"), "{query}: {text}");
        assert!(text.contains("3 import record(s)"), "{query}: {text}");
    }
}

/// The case that moves an *answer* rather than a refusal, and the one
/// with the widest reach: a real declaration whose name an import also
/// carries used to be one of two candidates, so `find_references` and the
/// other three anchored tools answered with a candidate page instead of
/// their result. ripgrep's `test` and requests' `ssl` are the measured
/// instances.
///
/// **Evidence.** With the exclusion reverted this fails: the answer is a
/// two-row candidate page rather than the declaration.
#[test]
fn a_declaration_whose_name_an_import_shares_resolves_to_the_declaration() {
    let mut conn = setup();
    upsert_node(&mut conn, import_placeholder("ph", "test", "test", "a.rs")).unwrap();
    upsert_node(&mut conn, node_with_span("decl", "test", "tests::test", "b.rs", (9, 0))).unwrap();

    let body = json_body(&resolved_by_name(&conn, "test").unwrap());

    assert_eq!(body["id"], "decl", "the declaration, not the import: {body}");
    assert_eq!(body["resolvedBy"], "name");
}

/// **Control.** A declaration with no import anywhere near it resolves
/// exactly as it did, by the same rung, with the same id - this passes in
/// both arms, and is what says the tests above are about placeholders
/// rather than about the filter having swallowed everything.
#[test]
fn a_declaration_with_no_import_in_sight_resolves_exactly_as_before() {
    let mut conn = setup();
    upsert_node(&mut conn, node_with_span("decl", "Marshal", "codec::Marshal", "b.rs", (9, 0))).unwrap();

    let body = json_body(&resolved_by_name(&conn, "codec::Marshal").unwrap());

    assert_eq!(body["id"], "decl");
    assert_eq!(body["resolvedBy"], "qualifiedName", "the fast path is untouched");
}

/// **Control.** A name several *declarations* carry is still an
/// ambiguity, and still a ranked candidate page - the exclusion narrows
/// which rows are declarations, never what happens once two of them
/// compete. Passes in both arms.
#[test]
fn two_declarations_sharing_a_name_are_still_a_candidate_page() {
    let mut conn = setup();
    upsert_node(&mut conn, node_with_span("a", "run", "pkg_a::run", "a.rs", (5, 0))).unwrap();
    upsert_node(&mut conn, node_with_span("b", "run", "pkg_b::run", "b.rs", (5, 0))).unwrap();

    let body = json_body(&resolved_by_name(&conn, "run").unwrap());

    assert_eq!(body["ambiguous"], true);
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
    assert_eq!(body["results"].as_array().unwrap().len(), 2);
}

/// The rung order, asserted rather than assumed: gin's `context` is both
/// an import placeholder *and* the stem of `context.go`, and the file
/// wins - its declarations are a better answer than a note about an
/// import. [`import_only_refusal`]'s own doc has the argument.
#[test]
fn a_file_named_like_the_import_answers_before_the_import_note_does() {
    let mut conn = setup();
    upsert_node(&mut conn, import_placeholder("ph", "context", "context", "app/main.go")).unwrap();
    upsert_node(&mut conn, node_with_span("decl", "Context", "Context", "context.go", (30, 0))).unwrap();

    let body = json_body(&resolved_by_name(&conn, "context").unwrap());

    assert_eq!(body["resolvedBy"], "fileName");
    assert_eq!(body["results"][0]["id"], "decl");
}

#[test]
fn ambiguous_bare_name_returns_both_as_ranked_candidates() {
    let mut conn = setup();
    upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
    upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();
    // A third, unrelated node calls n2 twice so its inbound CALLS count
    // outranks n1's zero - exercises the ranking, not just presence.
    upsert_node(
        &mut conn,
        NodeRecord::new("caller1", "Function", "caller1", "pkg_c::caller1", "c/lib.rs", "rust"),
    )
    .unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e1", "caller1", "n2", "CALLS", "tree-sitter", true)).unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e2", "caller1", "n2", "REFERENCES", "tree-sitter", true))
        .unwrap();

    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result = handle(
        &Arc::new(IndexStore::new(conn)),
        &no_sources(),
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        params,
    )
    .unwrap();
    let body = json_body(&result);
    let results = body["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "both same-named symbols must come back as candidates");
    assert_eq!(results[0]["qualifiedName"], "pkg_b::run", "the higher inbound-edge count must rank first");
    assert_eq!(results[1]["qualifiedName"], "pkg_a::run");
}

/// Neither placeholder kind is a definition: one is named after a symbol
/// this file imports, the other after one it only republishes, and a
/// monorepo has a barrel republishing almost everything - so without the
/// filter a plain name lookup would turn ambiguous project-wide.
#[test]
fn placeholders_named_after_a_symbol_are_not_definition_candidates() {
    let mut conn = setup();
    upsert_node(&mut conn, node_with_span("n1", "mutate", "mutate", "target.ts", (5, 0))).unwrap();

    for (id, native_kind, file) in
        [("pending", "pending_symbol", "caller.ts"), ("reexported", "reexport", "index.ts")]
    {
        let mut placeholder = NodeRecord::new(id, "Module", "mutate", "target.ts#mutate", file, "typescript");
        placeholder.native_kind = Some(native_kind.to_string());
        placeholder.end_line = 5;
        upsert_node(&mut conn, placeholder).unwrap();
    }

    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("mutate".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let body = json_body(
        &handle(
            &Arc::new(IndexStore::new(conn)),
            &no_sources(),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            params,
        )
        .unwrap(),
    );
    assert_eq!(body["ambiguous"], serde_json::Value::Null, "only one node is a real definition");
    assert_eq!(body["filePath"], "target.ts");
}

/// A Rust function `app` in module `app`: the module is a core-owned
/// container node carrying the same name. Were it a candidate, this
/// unambiguous lookup would come back as an ambiguous page offering a
/// row with no file behind it.
#[test]
fn a_container_named_like_its_member_is_not_a_definition_candidate() {
    let mut conn = setup();
    let mut member = node_with_span("n1", "app", "app::app", "src/app.rs", (5, 0));
    member.container = Some("app".to_string());
    upsert_node(&mut conn, member).unwrap();
    let candidates = find_candidates_by_name(&conn, NameColumn::Name, &[Lookup::any("app")], None).unwrap();
    assert_eq!(candidates.results.len(), 1, "the container node must not be ranked");

    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("app".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let body = json_body(
        &handle(
            &Arc::new(IndexStore::new(conn)),
            &no_sources(),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            params,
        )
        .unwrap(),
    );
    assert_eq!(body["ambiguous"], serde_json::Value::Null);
    assert_eq!(body["id"], "n1");
    assert_eq!(body["filePath"], "src/app.rs");
}

#[test]
fn file_and_position_query_returns_a_single_node_not_a_list() {
    let mut conn = setup();
    upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
    upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();

    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: None,
        file_path: Some("a/lib.rs".to_string()),
        position: Some(crate::protocol::types::Position { line: 2, col: 0 }),
        cursor: None,
        include_source: None,
    };
    let result = handle(
        &Arc::new(IndexStore::new(conn)),
        &no_sources(),
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        params,
    )
    .unwrap();
    let body = json_body(&result);
    assert_eq!(body["id"], "n1");
    assert_eq!(body["qualifiedName"], "pkg_a::run");
    assert!(
        body.get("results").is_none(),
        "an unambiguous file+position query must not be wrapped in a list"
    );
}

#[test]
fn qualified_name_requery_returns_the_exact_node() {
    let mut conn = setup();
    upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
    upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();

    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("pkg_b::run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result = handle(
        &Arc::new(IndexStore::new(conn)),
        &no_sources(),
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        params,
    )
    .unwrap();
    let body = json_body(&result);
    assert_eq!(body["id"], "n2");
    assert_eq!(body["qualifiedName"], "pkg_b::run");
}

#[test]
fn no_match_is_a_tool_level_error() {
    let conn = setup();
    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("does_not_exist".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result = handle(
        &Arc::new(IndexStore::new(conn)),
        &no_sources(),
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        params,
    )
    .unwrap();
    assert!(error_text(&result).contains("does_not_exist"));
}

#[test]
fn neither_name_nor_position_is_a_tool_level_error() {
    let conn = setup();
    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: None,
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result = handle(
        &Arc::new(IndexStore::new(conn)),
        &no_sources(),
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        params,
    )
    .unwrap();
    assert!(error_text(&result).contains("symbol_name"));
}

#[test]
fn ambiguous_candidates_paginate_across_cursor_continuation() {
    let mut conn = setup();
    // One more node than CANDIDATE_PAGE_SIZE, so the first page is
    // truncated and a second `handle()` call with its cursor must
    // return exactly the remainder, with nothing repeated or skipped.
    let total = CANDIDATE_PAGE_SIZE + 1;
    for i in 0..total {
        let id = format!("n{i}");
        upsert_node(&mut conn, node_with_span(&id, "run", &format!("pkg{i}::run"), "a/lib.rs", (5, 0)))
            .unwrap();
    }
    let conn = Arc::new(IndexStore::new(conn));

    let first_params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let first =
        handle(&conn, &no_sources(), &EmbeddingPipeline::disabled(), QueryShapes::shipped(), first_params)
            .unwrap();
    let first_body = json_body(&first);
    let first_results = first_body["results"].as_array().unwrap();
    assert_eq!(first_results.len(), CANDIDATE_PAGE_SIZE);
    assert_eq!(first_body["hasMore"], true);
    let cursor = first_body["nextCursor"].as_str().unwrap().to_string();

    let second_params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: Some(cursor),
        include_source: None,
    };
    let second =
        handle(&conn, &no_sources(), &EmbeddingPipeline::disabled(), QueryShapes::shipped(), second_params)
            .unwrap();
    let second_body = json_body(&second);
    let second_results = second_body["results"].as_array().unwrap();
    assert_eq!(second_results.len(), 1, "the one remaining candidate must land on the second page");
    assert_eq!(second_body["hasMore"], false);

    let mut all_ids: Vec<String> = first_results
        .iter()
        .chain(second_results.iter())
        .map(|c| c["qualifiedName"].as_str().unwrap().to_string())
        .collect();
    all_ids.sort();
    all_ids.dedup();
    assert_eq!(all_ids.len(), total, "every candidate must appear exactly once across both pages");
}

/// The ladder's contract: every answer says which rung reached it, so a
/// caller can tell "this is your symbol" from "these might be".
#[test]
fn an_exact_name_reports_the_rung_that_resolved_it() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("run", "Function", "run", "pkg::run", "src/run.rs", "rust"))
        .unwrap();

    let by_qualified = json_body(&by_name(&conn, None, &SemanticRung::off(), "pkg::run", None).unwrap());
    assert_eq!(by_qualified["resolvedBy"], "qualifiedName");

    let by_bare = json_body(&by_name(&conn, None, &SemanticRung::off(), "run", None).unwrap());
    assert_eq!(by_bare["resolvedBy"], "name");
}

/// The rung 2.9.0 shipped as prose in an error, now the same facts in the
/// shape the ambiguous rung already uses - so a caller has one contract to
/// know (re-query by a candidate's id) rather than two.
#[test]
fn a_name_only_a_file_carries_returns_that_files_declarations_as_candidates() {
    let mut conn = setup();
    upsert_node(
        &mut conn,
        NodeRecord::new(
            "menu_group",
            "Function",
            "MenuGroup",
            "MenuGroup",
            "src/components/DropdownMenuGroup.tsx",
            "typescript",
        ),
    )
    .unwrap();

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "DropdownMenuGroup", None).unwrap());

    assert_eq!(body["resolvedBy"], "fileName");
    // Not an ambiguity: these are not competing readings of one name, they
    // are what a differently-named thing declares. The rung says which.
    assert_eq!(body["ambiguous"], false);
    assert_eq!(body["results"][0]["id"], "menu_group");
    assert_eq!(body["results"][0]["qualifiedName"], "MenuGroup");
    assert!(
        body["explanation"].as_str().expect("an explanation").contains("default import"),
        "the page has to say why a name that is plainly in the source resolved to nothing: {body}"
    );
}

/// A declaration in `file`, at `line`, with `inbound` other declarations
/// referencing it - the three things the file-name rung's ordering reads.
/// Public, because the ordering it replaced put exported first and a
/// control has to hold that constant.
fn referenced_decl(
    conn: &mut Connection,
    id: &str,
    kind: &str,
    name: &str,
    file: &str,
    line: i64,
    inbound: usize,
) {
    let mut node = NodeRecord::new(id, kind, name, name, file, "go");
    node.start_line = line;
    node.end_line = line;
    node.visibility = "public".to_string();
    node.exported = true;
    upsert_node(conn, node).unwrap();
    for i in 0..inbound {
        let user = format!("{id}_user{i}");
        upsert_node(conn, NodeRecord::new(&user, "Function", &user, &user, "uses.go", "go")).unwrap();
        upsert_edge(
            conn,
            EdgeRecord::new(format!("{id}_e{i}"), &user, id, "REFERENCES", "tree-sitter", true),
        )
        .unwrap();
    }
}

/// GM-373's measured gin case, in miniature: nothing is named `context`,
/// `context.go` is, and that file opens with a block of exported MIME
/// constants while the type the file exists for is declared ninety-odd
/// declarations later. Ordered by source position the page is the
/// constants and the caller never sees `Context`; ordered by how much of
/// the project leans on each declaration (`Context` 335 inbound edges
/// against `MIMEJSON`'s 15, measured on a real gin index) it is the first
/// row.
///
/// **Evidence.** With `exported DESC, startLine ASC` restored as
/// `graph::queries::find_in_file_named`'s only ordering, this fails on the
/// first assertion: `MIMEJSON` is the first row.
#[test]
fn the_file_name_rung_offers_a_files_most_referenced_declaration_first() {
    let mut conn = setup();
    referenced_decl(&mut conn, "MIMEJSON", "Variable", "MIMEJSON", "context.go", 10, 2);
    referenced_decl(&mut conn, "MIMEXML", "Variable", "MIMEXML", "context.go", 11, 2);
    referenced_decl(&mut conn, "Context", "Type", "Context", "context.go", 100, 5);

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "context", None).unwrap());

    assert_eq!(body["resolvedBy"], "fileName");
    assert_eq!(body["results"][0]["qualifiedName"], "Context", "{body}");
    // A reorder, not a filter: the constants are still on the page, which
    // is what keeps this an answer to "which of these did you mean".
    let listed: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["qualifiedName"].as_str().unwrap()).collect();
    assert!(listed.contains(&"MIMEJSON"), "{listed:?}");
}

/// **Control.** The same rung, on the case it already answered well:
/// gin's `render/toml.go`, where the type `TOML` both comes first in the
/// file and is the most referenced thing in it. It is the first row under
/// either ordering, so this passes in both arms - which is what says the
/// test above is about the ordering rule and not about a page tuned until
/// one query looked right.
#[test]
fn a_file_whose_subject_is_also_its_first_declaration_is_unchanged() {
    let mut conn = setup();
    referenced_decl(&mut conn, "TOML", "Type", "TOML", "render/toml.go", 20, 6);
    referenced_decl(&mut conn, "tomlBinding.Name", "Function", "tomlBinding.Name", "render/toml.go", 30, 0);

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "toml", None).unwrap());

    assert_eq!(body["resolvedBy"], "fileName");
    assert_eq!(body["results"][0]["qualifiedName"], "TOML", "{body}");
}

/// **Control.** A file whose declarations are all unreferenced - ripgrep's
/// `crates/core/index/disabled.rs` is the real instance - has nothing for
/// the ranking to read, and falls back to exactly the order this rung
/// returned before: exported first, then source position. Passes in both
/// arms, and is what makes the change a refinement rather than a
/// replacement.
#[test]
fn a_file_with_no_inbound_edges_keeps_the_old_order() {
    let mut conn = setup();
    referenced_decl(&mut conn, "write", "Function", "write", "index/disabled.go", 10, 0);
    referenced_decl(&mut conn, "read", "Function", "read", "index/disabled.go", 20, 0);

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "disabled", None).unwrap());

    let listed: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["qualifiedName"].as_str().unwrap()).collect();
    assert_eq!(listed, vec!["write", "read"], "source order, as before: {body}");
}

/// **Discrimination.** `find_in_file_named`'s stem match is
/// directory-agnostic, so gin's real `fs.go` and `internal/fs/fs.go` both
/// answer for the stem `fs` and the page mixes rows from both. Before
/// GM-377 the explanation named only the first row's file as if it were
/// the page's sole source - false of a page that also carries a row from
/// the other file.
///
/// **Evidence.** Reverting the `by_file_name` fix (naming
/// `in_file[0].file_path` as "the file") makes this fail: the
/// explanation claims `fs.go` alone while `results` still carries a row
/// from `internal/fs/fs.go`.
#[test]
fn a_page_mixing_two_files_names_both_instead_of_the_first_rows_alone() {
    let mut conn = setup();
    referenced_decl(&mut conn, "Stat", "Function", "Stat", "fs.go", 10, 3);
    referenced_decl(&mut conn, "Open", "Function", "Open", "internal/fs/fs.go", 12, 1);

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "fs", None).unwrap());

    assert_eq!(body["resolvedBy"], "fileName");
    let paths: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["fs.go", "internal/fs/fs.go"], "{body}");

    let explanation = body["explanation"].as_str().expect("an explanation").to_string();
    assert!(
        explanation.contains("fs.go") && explanation.contains("internal/fs/fs.go"),
        "the explanation must name every file the rows came from, not just the first: \
         {explanation}"
    );
    assert!(
        !explanation.contains("The file fs.go is"),
        "must not claim fs.go alone as the page's source when internal/fs/fs.go also \
         contributed a row: {explanation}"
    );
}

/// **Control.** A single-file page reads exactly as it always has - this
/// is what says the sentence above is a fix for mixed pages, not a
/// rewording of every page's explanation.
#[test]
fn a_single_file_page_keeps_the_unqualified_sentence() {
    let mut conn = setup();
    referenced_decl(&mut conn, "Stat", "Function", "Stat", "fs.go", 10, 3);
    referenced_decl(&mut conn, "Open", "Function", "Open", "fs.go", 12, 1);

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "fs", None).unwrap());

    assert_eq!(body["resolvedBy"], "fileName");
    let explanation = body["explanation"].as_str().expect("an explanation").to_string();
    assert!(
        explanation.starts_with(
            "No declaration is named 'fs'. The file fs.go is, and \
                                  declares these - its most-referenced declarations first."
        ),
        "{explanation}"
    );
}

/// A name that is nowhere keeps the short refusal. A page of candidates
/// with nothing in it would be a worse answer than saying so.
#[test]
fn a_name_matching_neither_a_declaration_nor_a_file_is_still_refused() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("run", "Function", "run", "pkg::run", "src/run.rs", "rust"))
        .unwrap();

    let result = by_name(&conn, None, &SemanticRung::off(), "NoSuchThingAnywhere", None).unwrap();

    assert_eq!(error_text(&result), "g-mesh: no symbol named 'NoSuchThingAnywhere' found");
}

/// The ambiguous rung keeps its own label, so the three candidate-shaped
/// answers stay distinguishable by one field rather than by sniffing.
#[test]
fn an_ambiguous_name_labels_its_page_as_the_ambiguity_it_is() {
    let mut conn = setup();
    for (id, file) in [("a", "a.rs"), ("b", "b.rs")] {
        upsert_node(&mut conn, NodeRecord::new(id, "Function", "run", "run", file, "rust")).unwrap();
    }

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "run", None).unwrap());

    assert_eq!(body["ambiguous"], true);
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS, "on every candidate page");
}

#[test]
fn a_unique_name_carries_no_ambiguity_explanation() {
    let mut conn = setup();
    upsert_node(&mut conn, NodeRecord::new("a", "Function", "run", "run", "a.rs", "rust")).unwrap();

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "run", None).unwrap());

    assert!(body.get("explanation").is_none(), "{body}");
}

// --- GM-360: neither key is a unique one -------------------------------

/// ripgrep's own `RegexMatcher` set, reduced to what the resolver reads:
/// four declarations of one name, three of them module-qualified and
/// one (the `pub(crate)` fixture under `tests/`) carrying the bare
/// string because it sits at its crate's root. The inbound-edge counts are the
/// measured ones (`find_candidates_by_name`'s own doc), so the ranking
/// these tests assert is ripgrep's ranking and not a convenient one.
fn ripgrep_regex_matchers() -> Connection {
    let mut conn = setup();
    upsert_node(
        &mut conn,
        NodeRecord::new("user", "Function", "user", "user", "crates/core/search.rs", "rust"),
    )
    .unwrap();
    for (id, qualified_name, file, inbound) in [
        ("regex", "matcher::RegexMatcher", "crates/regex/src/matcher.rs", 11),
        ("searcher", "testutil::RegexMatcher", "crates/searcher/src/testutil.rs", 7),
        ("pcre2", "matcher::RegexMatcher", "crates/pcre2/src/matcher.rs", 5),
        ("fixture", "RegexMatcher", "crates/matcher/tests/util.rs", 2),
    ] {
        upsert_node(&mut conn, node_with_span(id, "RegexMatcher", qualified_name, file, (5, 0))).unwrap();
        for n in 0..inbound {
            let edge = EdgeRecord::new(format!("{id}-{n}"), "user", id, "REFERENCES", "tree-sitter", true);
            upsert_edge(&mut conn, edge).unwrap();
        }
    }
    conn
}

/// Face A1. The bare query used to resolve - exactly, unflagged,
/// `resolvedBy: qualifiedName` - to the one declaration whose
/// qualifiedName *is* the bare string, which in ripgrep is a `pub(crate)`
/// test fixture. Carrying the bare string is a fact about module depth,
/// so it competes with its namesakes instead of short-circuiting past
/// them.
#[test]
fn a_bare_name_does_not_resolve_to_whichever_declaration_sits_at_a_root() {
    let conn = ripgrep_regex_matchers();

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "RegexMatcher", None).unwrap());

    assert_eq!(body["ambiguous"], true, "four declarations carry this name: {body}");
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
    let results = body["results"].as_array().expect("a candidate page");
    assert_eq!(results.len(), 4, "every declaration of the name is offered");
    assert_eq!(
        results[0]["filePath"], "crates/regex/src/matcher.rs",
        "ranked by inbound edges, so the production matcher leads"
    );
    assert_eq!(
        results[3]["filePath"], "crates/matcher/tests/util.rs",
        "the test fixture is ranked last, not excluded - find_candidates_by_name's own doc"
    );
}

/// Face A5, the mirror image: two crates each have a `matcher` module, so
/// the exact rung matches two rows. "Not exactly one" used to mean "no
/// match", which dropped the query into a bare-name lookup no qualified
/// spelling can match, and on from there to the semantic rung.
#[test]
fn a_qualified_name_two_declarations_carry_is_an_ambiguity_not_a_miss() {
    let conn = ripgrep_regex_matchers();

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "matcher::RegexMatcher", None).unwrap());

    assert_eq!(body["ambiguous"], true, "two declarations carry this qualifiedName: {body}");
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
    let results = body["results"].as_array().expect("a candidate page");
    assert_eq!(results.len(), 2, "only the two declarations that carry it, not all four namesakes");
    assert_eq!(results[0]["filePath"], "crates/regex/src/matcher.rs");
    assert_eq!(results[1]["filePath"], "crates/pcre2/src/matcher.rs");
}

/// The control this fix is measured against: gin's `Binding`, two
/// `interface` declarations behind build tags, both with bare
/// qualifiedNames. It was already ambiguous before GM-360 and has to
/// answer identically after - it is the case that proves ambiguity
/// detection works and that Rust's naming scheme, not the detector, was
/// what slipped past it.
#[test]
fn two_declarations_sharing_one_bare_qualified_name_are_ambiguous_exactly_as_before() {
    let mut conn = setup();
    for (id, file) in [("binding", "binding/binding.go"), ("nomsgpack", "binding/binding_nomsgpack.go")] {
        upsert_node(&mut conn, node_with_span(id, "Binding", "Binding", file, (40, 0))).unwrap();
    }

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "Binding", None).unwrap());

    assert_eq!(body["ambiguous"], true);
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
    assert_eq!(body["results"].as_array().expect("a candidate page").len(), 2);
}

/// The other control, and the reason the fix is a comparison against a
/// declaration's own `name` rather than a search for `::`: a language
/// whose qualifiedNames are bare by construction (TypeScript) must keep
/// resolving a lone declaration on the strongest rung, not lose it to a
/// bare-name lookup with a weaker label.
#[test]
fn a_lone_declaration_whose_qualified_name_is_bare_still_resolves_on_the_exact_rung() {
    let mut conn = setup();
    upsert_node(
        &mut conn,
        node_with_span(
            "n1",
            "getNonDeletedElements",
            "getNonDeletedElements",
            "packages/element/src/index.ts",
            (5, 0),
        ),
    )
    .unwrap();

    let body = json_body(&by_name(&conn, None, &SemanticRung::off(), "getNonDeletedElements", None).unwrap());

    assert_eq!(body["resolvedBy"], "qualifiedName");
    assert_eq!(body["id"], "n1");
    assert!(body.get("results").is_none(), "one declaration is not a candidate page");
}

// --- the declaration's source ------------------------------------------

/// A project whose one file really contains `body`, so the snippet is read
/// off disk exactly as a real answer would read it.
fn project_with(body: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create a temp project");
    std::fs::create_dir_all(dir.path().join("a")).expect("failed to create the fixture directory");
    std::fs::write(dir.path().join("a/lib.rs"), body).expect("failed to write the fixture");
    dir
}

fn definition_of(name: &str, project_root: &std::path::Path, span: (i64, i64)) -> serde_json::Value {
    let mut conn = setup();
    let mut node = node_with_span("n1", name, &format!("pkg::{name}"), "a/lib.rs", (span.1, 0));
    node.start_line = span.0;
    upsert_node(&mut conn, node).unwrap();
    json_body(&by_name(&conn, Some(project_root), &SemanticRung::off(), name, None).unwrap())
}

/// The point of the whole change: the answer to "where is this defined"
/// carries what is there, so the caller does not spend a round trip - worth
/// 18,000-22,000 tokens at this tool's prompt prefix - reading it.
#[test]
fn a_definition_carries_the_declarations_own_source() {
    let project = project_with("mod a;\nfn run() {\n    work();\n}\nfn other() {}\n");

    let body = definition_of("run", project.path(), (1, 3));

    assert_eq!(body["source"]["text"], "fn run() {\n    work();\n}");
    assert_eq!(body["source"]["firstLine"], 2, "1-based, as an editor shows it");
    assert!(body["source"]["omittedLines"].is_null(), "a complete snippet says nothing about omissions");
    // The coordinates are still there and still 0-based: the snippet is an
    // addition, not a replacement, and anything doing arithmetic on
    // startLine must keep working.
    assert_eq!(body["startLine"], 1);
}

#[test]
fn include_source_false_leaves_the_response_exactly_as_it_was() {
    let project = project_with("fn run() {\n    work();\n}\n");
    let mut conn = setup();
    let mut node = node_with_span("n1", "run", "pkg::run", "a/lib.rs", (2, 0));
    node.start_line = 0;
    upsert_node(&mut conn, node).unwrap();
    let conn = Arc::new(IndexStore::new(conn));

    let params = |include| FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: include,
    };

    let opted_out = json_body(
        &handle(
            &conn,
            project.path(),
            &EmbeddingPipeline::disabled(),
            QueryShapes::shipped(),
            params(Some(false)),
        )
        .unwrap(),
    );
    let default_on = json_body(
        &handle(&conn, project.path(), &EmbeddingPipeline::disabled(), QueryShapes::shipped(), params(None))
            .unwrap(),
    );

    assert!(opted_out["source"].is_null(), "include_source: false must omit it");
    assert!(!default_on["source"].is_null(), "omitting the flag must default to on");
    assert_eq!(opted_out["id"], default_on["id"], "nothing else about the answer changes");
    assert_eq!(opted_out["startLine"], default_on["startLine"]);
}

/// The index outliving the file it describes is ordinary - a file edited
/// or deleted since the last walk. Coordinates are still a correct answer,
/// so the snippet goes missing rather than the call failing.
#[test]
fn a_definition_whose_file_no_longer_matches_still_answers_with_coordinates() {
    let project = project_with("fn run() {}\n");

    // The node claims lines 40-45; the file has one line.
    let body = definition_of("run", project.path(), (40, 45));

    assert_eq!(body["qualifiedName"], "pkg::run", "the definition still resolves");
    assert_eq!(body["startLine"], 40);
    assert!(body["source"].is_null(), "a snippet that cannot be read honestly is absent");
}

/// The cap has to be exercised on a real declaration, and the truncation
/// has to be visible: a caller reading a cut body as a complete one draws
/// conclusions from code that is not there.
#[test]
fn a_declaration_past_the_cap_is_cut_visibly() {
    let long: String = (0..source::MAX_LINES + 20).map(|n| format!("    let x{n} = {n};\n")).collect();
    let project = project_with(&format!("fn run() {{\n{long}}}\n"));

    let body = definition_of("run", project.path(), (0, (source::MAX_LINES + 21) as i64));

    let text = body["source"]["text"].as_str().expect("a snippet");
    assert_eq!(text.lines().count(), source::MAX_LINES);
    assert_eq!(body["source"]["omittedLines"], 22, "the cut says exactly how much is missing");
}

// --- the semantic rung -------------------------------------------------

/// Vectors are inserted directly rather than produced by the real model,
/// so these tests pin the *rung's* logic - threshold, guard, page shape -
/// and not the model's opinions, which GMB-133 measured separately.
fn insert_vector(conn: &Connection, node_id: &str, embedding: &[f32]) {
    crate::storage::vectors::insert(conn, node_id, embedding, "test-model").unwrap();
}

/// A query vector identical to the stored one scores 1.0; an orthogonal
/// one scores 0.0. Two dimensions is enough to place a hit on either side
/// of the threshold deliberately.
fn setup_with_vectors() -> Connection {
    crate::storage::vectors::register_extension();
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    upsert_node(&mut conn, NodeRecord::new("near", "Function", "near", "pkg::near", "a.rs", "rust")).unwrap();
    conn
}

/// A pipeline whose model is not yet known to be missing, so a deferred pass
/// that reaches the rung records the name. It never loads: only the
/// `Embedded` pass embeds, and these tests drive the deferred one.
fn unloaded_pipeline() -> EmbeddingPipeline {
    EmbeddingPipeline::with_loader(
        Path::new("/nonexistent-model-dir"),
        |_: &Path| -> anyhow::Result<_> { panic!("a deferred pass never loads the model") },
        None,
    )
}

fn shapes(starts_with: &[&str], contains: &[&str]) -> crate::daemon::manifest::NonSymbolShapes {
    crate::daemon::manifest::NonSymbolShapes {
        starts_with: starts_with.iter().map(|s| s.to_string()).collect(),
        contains: contains.iter().map(|s| s.to_string()).collect(),
    }
}

/// With the shipped declarations every language refuses `@` and `/`, so the
/// first pass stops at the rung without asking for the query to be embedded.
/// A plain name on the same pipeline does ask, which shows the pipeline
/// would have embedded.
///
/// Control: remove the `refused_by_all` check in `by_semantic_neighbours` -
/// every specifier is then recorded in `reached`.
#[test]
fn with_the_shipped_shapes_a_specifier_is_refused_before_it_is_embedded() {
    let conn = setup_with_vectors();
    let embedding = unloaded_pipeline();

    for query in ["@excalidraw/math", "packages/element/src/index.ts", "./extract.js", "@Component"] {
        let rung = SemanticRung::deferred(&embedding, QueryShapes::shipped());
        by_name(&conn, None, &rung, query, None).unwrap();
        assert_eq!(rung.reached(), None, "{query} must not be embedded");
    }
    let rung = SemanticRung::deferred(&embedding, QueryShapes::shipped());
    by_name(&conn, None, &rung, "DropdownMenuGroup", None).unwrap();
    assert_eq!(rung.reached().as_deref(), Some("DropdownMenuGroup"), "a plain name reaches the rung");
}

/// The second pass of the ladder with a query vector identical to `near`'s,
/// so the semantic rung scores 1.0 and answers whenever it is consulted.
fn with_a_matching_vector(conn: &Connection, name: &str) -> CallToolResult {
    with_a_matching_vector_and(conn, QueryShapes::shipped(), name)
}

fn with_a_matching_vector_and(conn: &Connection, shapes: &QueryShapes, name: &str) -> CallToolResult {
    let query = [1.0_f32, 0.0];
    by_name(conn, None, &SemanticRung::Embedded { name, query: Some(&query), shapes }, name, None).unwrap()
}

/// A language's shapes only ever remove that language's candidates. The
/// fixture's hit is Rust; another language declares `get`, so `getX` is not
/// refused by every language and the Rust hit, whose language declares
/// nothing, is still offered.
///
/// Control: make `QueryShapes::refuses` ignore its `language` (refuse when
/// any language's shapes match) - the Rust hit is dropped and this fails.
#[test]
fn another_languages_shapes_never_remove_this_languages_candidates() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);
    let map = QueryShapes::of(&[("fake", shapes(&["get"], &[])), ("rust", shapes(&[], &[]))]);

    let body = json_body(&with_a_matching_vector_and(&conn, &map, "getX"));

    assert_eq!(body["resolvedBy"], "semanticNeighbours", "{body}");
    assert_eq!(body["results"][0]["id"], "near", "{body}");
}

/// The per-candidate filter: Rust refuses `get` and another language does
/// not, so the query is embedded, and the Rust hit, though it scores 1.0,
/// is dropped by its own language's shapes.
///
/// Control: remove `&& !shapes.refuses(&hit.language, name)` from the filter
/// in `by_semantic_neighbours` - the Rust hit is offered and this fails.
#[test]
fn a_candidate_is_dropped_by_its_own_languages_shapes() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);
    let map = QueryShapes::of(&[("fake", shapes(&[], &[])), ("rust", shapes(&["get"], &[]))]);

    let result = with_a_matching_vector_and(&conn, &map, "getX");

    assert_eq!(error_text(&result), "g-mesh: no symbol named 'getX' found");
}

/// With the shipped declarations a relative import (`.models`) is refused by
/// Python alone: it still reaches the rung, the Python hit is dropped though
/// it scores 1.0, and the Rust hit beside it is offered.
///
/// Control: remove `"."` from `plugins/python/plugin.toml`'s `starts_with` -
/// the Python hit is offered too and this fails.
#[test]
fn with_the_shipped_shapes_a_relative_import_drops_only_python_candidates() {
    let mut conn = setup_with_vectors();
    upsert_node(&mut conn, NodeRecord::new("py", "Function", "near", "pkg.near", "a.py", "python")).unwrap();
    insert_vector(&conn, "near", &[1.0, 0.0]);
    insert_vector(&conn, "py", &[1.0, 0.0]);

    let embedding = unloaded_pipeline();
    let rung = SemanticRung::deferred(&embedding, QueryShapes::shipped());
    by_name(&conn, None, &rung, ".models", None).unwrap();
    assert_eq!(rung.reached().as_deref(), Some(".models"), "not every language refuses it");

    let body = json_body(&with_a_matching_vector(&conn, ".models"));

    assert_eq!(body["resolvedBy"], "semanticNeighbours", "{body}");
    let ids: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["near"], "{body}");
}

/// With no declarations at all, nothing is refused by shape: core holds no
/// fallback list of its own.
///
/// Control: put `|| name.starts_with('@') || name.contains('/')` back into
/// `by_semantic_neighbours`' first check - this is refused and fails.
#[test]
fn without_declarations_a_specifier_shaped_query_is_answered_by_score() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    let body = json_body(&with_a_matching_vector_and(&conn, &QueryShapes::default(), "@excalidraw/element"));

    assert_eq!(body["resolvedBy"], "semanticNeighbours", "{body}");
}

/// The measured reason these shapes exist: `@excalidraw/element` scores
/// 0.699 - above the threshold - while being junk, because only doc
/// comments and signatures are embedded and a specifier has nothing to
/// match. Score alone cannot catch it, so the guard has to. None of these
/// spellings is stored anywhere in the index, which is why a lookup of
/// stored module keys and file paths could not refuse them: a package the
/// project never imports, a relative specifier, and a path.
#[test]
fn a_specifier_is_refused_tersely_even_though_it_would_out_score_the_threshold() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    for query in ["@excalidraw/element", "./extract.js", "packages/element/src/index.ts"] {
        let result = with_a_matching_vector(&conn, query);
        assert_eq!(error_text(&result), format!("g-mesh: no symbol named '{query}' found"));
    }
}

/// The fixture's own control: on the same index and vector, a name with no
/// specifier's spelling reaches the rung and is answered by it, so the
/// refusals around it are the guard's doing and not the fixture's.
#[test]
fn a_plain_unknown_name_reaches_the_semantic_rung_on_the_same_fixture() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    let body = json_body(&with_a_matching_vector(&conn, "NoSuchThingAnywhere"));

    assert_eq!(body["resolvedBy"], "semanticNeighbours");
    assert_eq!(body["results"][0]["id"], "near");
}

/// What the spelling rule costs: a symbol-name query that starts with `@`
/// but is no specifier is refused rather than offered neighbours.
#[test]
fn a_non_specifier_with_a_specifiers_spelling_is_refused_too() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    let result = with_a_matching_vector(&conn, "@Component");

    assert_eq!(error_text(&result), "g-mesh: no symbol named '@Component' found");
}

#[test]
fn without_an_embedding_pipeline_the_answer_is_exactly_what_it_was_before() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    let result = by_name(&conn, None, &SemanticRung::off(), "NoSuchThingAnywhere", None).unwrap();

    assert_eq!(error_text(&result), "g-mesh: no symbol named 'NoSuchThingAnywhere' found");
}

/// The threshold is the whole safety property, so it is asserted on both
/// sides with the same fixture: one vector, two queries, one accepted and
/// one refused purely on similarity.
#[test]
fn the_threshold_decides_between_candidates_and_a_refusal() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    // Cosine 1.0 - comfortably above the fixture language's floor.
    let accepted = super::super::search_code::search(&conn, &[1.0, 0.0], SEMANTIC_CANDIDATES, None).unwrap();
    assert!(
        accepted.results[0].score >= similarity::floor("rust"),
        "the fixture must place this above the threshold: {}",
        accepted.results[0].score
    );

    // Cosine 0.0 - below it, so the rung must stay silent.
    let rejected = super::super::search_code::search(&conn, &[0.0, 1.0], SEMANTIC_CANDIDATES, None).unwrap();
    assert!(
        rejected.results[0].score < similarity::floor("rust"),
        "the fixture must place this below the threshold: {}",
        rejected.results[0].score
    );
}

/// A candidate page, not a resolution - the distinction the whole rung
/// rests on. Asserted on the wire shape, because that is what a caller
/// reads: a caller that cannot tell a suggestion from an answer is exactly
/// what makes a confident wrong hit worse than a refusal.
#[test]
fn a_semantic_page_is_labelled_as_candidates_and_carries_ids_to_requery() {
    let page = FileNamePage {
        resolved_by: ResolvedBy::SemanticNeighbours,
        ambiguous: false,
        explanation: "…".to_string(),
        results: vec![DefinitionCandidate {
            id: "near".to_string(),
            qualified_name: "pkg::near".to_string(),
            file_path: "a.rs".to_string(),
            start_line: None,
            end_line: None,
            end_col: None,
            kind: "Function".to_string(),
            preview: None,
            source: None,
        }],
    };

    let body: serde_json::Value = serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();

    assert_eq!(body["resolvedBy"], "semanticNeighbours", "the rung must name itself");
    assert_eq!(body["ambiguous"], false, "these are not competing readings of one name");
    assert_eq!(body["results"][0]["id"], "near", "the handle to re-query with must be present");
}

// --- rung 3.5: qualifiedName suffix --------------------------------------

/// `head` joined to each `(sep, name)` in turn.
fn qpath(head: &str, rest: &[(&str, &str)]) -> QualifiedPath {
    rest.iter().fold(QualifiedPath::root(head), |path, (sep, name)| path.child(*sep, *name))
}

/// A declaration carrying its plugin's segments: named after the last one,
/// qualified by their display, with `aliases` as the plugin would send them.
fn path_decl(conn: &mut Connection, id: &str, kind: &str, path: QualifiedPath, aliases: Vec<QualifiedPath>) {
    let name = path.last().unwrap().name.clone();
    let mut node = NodeRecord::new(id, kind, name, path.display(), format!("{id}.rs"), "rust");
    node.qualified_path = Some(path);
    node.alias_paths = aliases;
    upsert_node(conn, node).unwrap();
}

fn rust_fn(conn: &mut Connection, id: &str, head: &str, rest: &[(&str, &str)]) {
    path_decl(conn, id, "Function", qpath(head, rest), Vec::new());
}

/// `count` inbound `CALLS` edges onto `id`, from fresh callers.
fn called(conn: &mut Connection, id: &str, count: usize) {
    for i in 0..count {
        let caller = format!("{id}_caller{i}");
        rust_fn(conn, &caller, &caller, &[]);
        upsert_edge(
            conn,
            EdgeRecord::new(format!("{id}_call{i}"), &caller, id, "CALLS", "tree-sitter", true),
        )
        .unwrap();
    }
}

/// `(id, resolvedBy)` of a resolution; panics on a refusal.
fn answer(conn: &Connection, query: &str) -> (String, String) {
    let body = json_body(&resolved_by_name(conn, query).unwrap());
    (body["id"].as_str().unwrap_or_default().to_string(), body["resolvedBy"].as_str().unwrap().to_string())
}

fn refused(conn: &Connection, query: &str) -> bool {
    error_text(&resolved_by_name(conn, query).unwrap()).contains(&format!("no symbol named '{query}' found"))
}

/// g-mesh's own shape: `read` is declared several times, so the bare name is
/// ambiguous, and `IndexStore::read` is a stored suffix of exactly one.
fn index_store_project() -> Connection {
    let mut conn = setup();
    rust_fn(&mut conn, "is_read", "storage", &[("::", "index_store"), ("::", "IndexStore"), ("::", "read")]);
    rust_fn(&mut conn, "cr_read", "io", &[("::", "ChunkedReader"), ("::", "read")]);
    rust_fn(&mut conn, "free_read", "config", &[("::", "read")]);
    conn
}

/// **Control.** Remove the `by_qualified_name_suffix` arm from
/// `resolve_symbol_name`: both spellings fall through to the refusal.
#[test]
fn a_partial_path_resolves_by_its_qualified_name_suffix() {
    let conn = index_store_project();
    for query in ["IndexStore::read", "index_store::IndexStore::read"] {
        let body = json_body(&resolved_by_name(&conn, query).unwrap());
        assert_eq!(body["id"], "is_read", "{query}: {body}");
        assert_eq!(body["resolvedBy"], "qualifiedNameSuffix", "{query}: {body}");
        assert_eq!(body["qualifiedName"], "storage::index_store::IndexStore::read", "{query}");
    }
}

/// A suffix is whole segments: `Store` is not a segment of
/// `...::IndexStore::read`.
///
/// **Control.** In `graph::queries::find_by_qualified_suffix`, replace
/// `s.suffix = ?1` with `s.suffix LIKE '%' || ?1`: `Store::read` resolves to
/// `is_read`.
#[test]
fn a_suffix_that_splits_a_segment_is_not_a_match() {
    let conn = index_store_project();
    assert!(refused(&conn, "Store::read"));
}

/// A Rust field is `module::T.f` and its getter `module::T::f`: each
/// spelling reaches only its own node.
///
/// **Control.** Remove the `by_qualified_name_suffix` arm from
/// `resolve_symbol_name`: both are refused. Make the lookup
/// separator-insensitive (compare `replace(s.suffix, '.', '::')` to the
/// query with the same replacement) and `Ledger.total` is an ambiguity.
#[test]
fn a_rust_field_suffix_matches_only_the_field() {
    let mut conn = setup();
    path_decl(&mut conn, "field", "Variable", qpath("gaps", &[("::", "Ledger"), (".", "total")]), Vec::new());
    rust_fn(&mut conn, "getter", "gaps", &[("::", "Ledger"), ("::", "total")]);

    assert_eq!(answer(&conn, "Ledger.total"), ("field".into(), "qualifiedNameSuffix".into()));
    assert_eq!(answer(&conn, "Ledger::total"), ("getter".into(), "qualifiedNameSuffix".into()));
}

/// A trait-impl member's primary path names `<Square as Shape>`; the plugin's
/// alias `shapes::Square::area` is what `Square::area` matches.
///
/// **Control.** Drop the `for alias in aliases` loop from
/// `storage::qualified_path::suffixes`: `Square::area` is refused, while the
/// primary-path suffix `<Square as Shape>::area` still resolves.
#[test]
fn a_trait_impl_member_is_found_through_its_alias() {
    let mut conn = setup();
    path_decl(
        &mut conn,
        "area",
        "Function",
        qpath("shapes", &[("::", "<Square as Shape>"), ("::", "area")]),
        vec![qpath("shapes", &[("::", "Square"), ("::", "area")])],
    );

    assert_eq!(answer(&conn, "Square::area"), ("area".into(), "qualifiedNameSuffix".into()));
    assert_eq!(answer(&conn, "<Square as Shape>::area"), ("area".into(), "qualifiedNameSuffix".into()));
}

/// Two impls of one self type both carry `Stream::read`: the same ranked
/// page as an ambiguous name, most inbound edges first.
///
/// **Control.** Return `matched.remove(0)` for any non-empty match set in
/// `by_qualified_name_suffix`: the page becomes a single resolution. Flip
/// `paginate_by_score`'s `score DESC` and the order assertion fails.
#[test]
fn a_suffix_several_declarations_carry_is_a_ranked_ambiguity() {
    let mut conn = setup();
    for (id, module) in [("unix", "unix"), ("windows", "windows")] {
        path_decl(
            &mut conn,
            id,
            "Function",
            qpath("ipc", &[("::", module), ("::", "<Stream as Read>"), ("::", "read")]),
            vec![qpath("ipc", &[("::", module), ("::", "Stream"), ("::", "read")])],
        );
    }
    called(&mut conn, "windows", 2);

    let body = json_body(&resolved_by_name(&conn, "Stream::read").unwrap());
    assert_eq!(body["resolvedBy"], "nameAmbiguous", "{body}");
    assert_eq!(body["ambiguous"], true);
    let ids: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["windows", "unix"], "inbound edge count descending");

    // A candidate's own qualifiedName takes rung 2.
    assert_eq!(answer(&conn, "ipc::unix::<Stream as Read>::read"), ("unix".into(), "qualifiedName".into()));
}

/// Queries rungs 1-3 answer keep their answer and label, even where another
/// declaration also stores the same spelling as a suffix.
///
/// **Control.** Call `by_qualified_name_suffix` first in
/// `resolve_symbol_name` and return its answer whenever it has one:
/// `pkg_b::run` resolves to `n3` by `qualifiedNameSuffix`.
#[test]
fn a_query_the_earlier_rungs_answer_is_answered_as_before() {
    let mut conn = setup();
    rust_fn(&mut conn, "n2", "pkg_b", &[("::", "run")]);
    rust_fn(&mut conn, "n3", "outer", &[("::", "pkg_b"), ("::", "run")]);
    rust_fn(&mut conn, "solo", "pkg", &[("::", "only_one")]);

    assert_eq!(answer(&conn, "pkg_b::run"), ("n2".into(), "qualifiedName".into()));
    assert_eq!(answer(&conn, "only_one"), ("solo".into(), "name".into()));
    let body = json_body(&resolved_by_name(&conn, "run").unwrap());
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
}

/// `_` is a `LIKE` wildcard; the suffix lookup is equality.
///
/// **Control.** In `graph::queries::find_by_qualified_suffix`, replace
/// `s.suffix = ?1` with `s.suffix LIKE ?1`: `store::read_all` resolves to
/// `x`.
#[test]
fn an_underscore_in_the_query_is_not_a_wildcard() {
    let mut conn = setup();
    rust_fn(&mut conn, "x", "app", &[("::", "store"), ("::", "readXall")]);
    assert_eq!(answer(&conn, "store::readXall"), ("x".into(), "qualifiedNameSuffix".into()));
    assert!(refused(&conn, "store::read_all"));
}

// --- the strip-prefix rung (`[plugin.symbol_query_prefixes]`) -------------

fn ts_decl(conn: &mut Connection, id: &str, name: &str, qualified_name: &str, language: &str) {
    upsert_node(conn, NodeRecord::new(id, "Function", name, qualified_name, format!("{id}.ts"), language))
        .unwrap();
}

/// The ladder with no model and `map`'s declarations in place of the
/// shipped ones.
fn resolved_with(conn: &Connection, map: &QueryShapes, name: &str) -> CallToolResult {
    let embedding = EmbeddingPipeline::disabled();
    by_name(conn, None, &SemanticRung::deferred(&embedding, map), name, None).unwrap()
}

/// Two languages that both refuse `@` and `/`, with `strip = ["@"]` for
/// each language in `opting`.
fn decorator_shapes(opting: &[&str]) -> QueryShapes {
    let both = QueryShapes::of(&[("typescript", shapes(&["@"], &["/"])), ("python", shapes(&["@"], &["/"]))]);
    opting.iter().fold(both, |map, language| map.with_strip(language, &["@"]))
}

fn ids(body: &serde_json::Value) -> Vec<&str> {
    let mut ids: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    ids.sort_unstable();
    ids
}

/// T7: with the shipped manifests, a decorator's use-site spelling finds the
/// decorator, labelled with the rung the remainder resolved at and the name
/// looked up.
///
/// Control: delete the `by_stripped_prefix` arm from `resolve_symbol_name` -
/// `@Component` is refused and this fails.
#[test]
fn a_decorator_query_resolves_to_the_decorator() {
    let mut conn = setup();
    ts_decl(&mut conn, "deco", "Component", "Component", "typescript");

    let body = json_body(&resolved_by_name(&conn, "@Component").unwrap());

    assert_eq!(body["id"], "deco", "{body}");
    assert_eq!(body["resolvedBy"], "qualifiedName", "{body}");
    assert_eq!(body["queriedAs"], "Component", "{body}");
    let plain = json_body(&resolved_by_name(&conn, "Component").unwrap());
    assert!(plain.get("queriedAs").is_none(), "only a rewritten answer says what it looked up: {plain}");
}

/// T8: only TypeScript opts in, so only TypeScript's `Component` is a
/// candidate for `@Component`; Python's namesake does not make it ambiguous.
///
/// Control: make `Lookup::admits` return `true` - both rows are candidates,
/// the answer is an ambiguous page, and this fails.
#[test]
fn a_rewrite_finds_only_the_opting_languages_declarations() {
    let mut conn = setup();
    ts_decl(&mut conn, "ts", "Component", "Component", "typescript");
    ts_decl(&mut conn, "py", "Component", "Component", "python");

    let body = json_body(&resolved_with(&conn, &decorator_shapes(&["typescript"]), "@Component"));

    assert_eq!(body["id"], "ts", "{body}");
    assert_eq!(body["queriedAs"], "Component", "{body}");
}

/// T9: both languages opt in, so `@Component` names two decorators and the
/// answer is the ranked page with both - and with no third language's
/// namesake, which the page's own SQL leaves out.
///
/// Control: collapse the union to one language (take only the first pair of
/// `QueryShapes::rewrites` in `by_stripped_prefix`) - Python's alone
/// resolves and this fails. Control for the third row: drop the
/// `n.language IN (...)` condition in `Lookup::filter` - the Rust row joins
/// the page and this fails.
#[test]
fn when_both_languages_opt_in_the_rewrite_is_an_ambiguous_page_of_both() {
    let mut conn = setup();
    ts_decl(&mut conn, "ts", "Component", "Component", "typescript");
    ts_decl(&mut conn, "py", "Component", "Component", "python");
    ts_decl(&mut conn, "rs", "Component", "Component", "rust");

    let body = json_body(&resolved_with(&conn, &decorator_shapes(&["typescript", "python"]), "@Component"));

    assert_eq!(body["ambiguous"], true, "{body}");
    assert_eq!(body["resolvedBy"], "nameAmbiguous", "{body}");
    assert_eq!(body["queriedAs"], "Component", "{body}");
    assert_eq!(ids(&body), vec!["py", "ts"], "{body}");
}

/// The language filter sits inside the page's query, so a full page of the
/// opting language's rows says `hasMore: false` even while a more-referenced
/// namesake of another language exists.
///
/// Control: filter the page's rows after `paginate_by_score` instead of in
/// `Lookup::filter` (or drop the `n.language IN (...)` condition) - the Rust
/// row takes a slot, `hasMore` turns true, and this fails.
#[test]
fn a_rewritten_page_counts_only_the_opting_languages_rows() {
    let mut conn = setup();
    for i in 0..CANDIDATE_PAGE_SIZE {
        ts_decl(&mut conn, &format!("ts{i:02}"), "Component", &format!("m{i}.Component"), "typescript");
    }
    ts_decl(&mut conn, "rs", "Component", "Component", "rust");
    called(&mut conn, "rs", 3);

    let body = json_body(&resolved_with(&conn, &decorator_shapes(&["typescript"]), "@Component"));

    assert_eq!(body["results"].as_array().unwrap().len(), CANDIDATE_PAGE_SIZE, "{body}");
    assert!(!ids(&body).contains(&"rs"), "{body}");
    assert_eq!(body["hasMore"], false, "{body}");
}

/// The remainder is retried on the qualifiedName-suffix rung too, as
/// `@Widget.size` measured on py-deco.
///
/// Control: return `Ok(None)` in place of the suffix lookup in
/// `by_stripped_prefix` - this is refused and fails.
#[test]
fn a_rewrite_reaches_the_qualified_name_suffix_rung() {
    let mut conn = setup();
    let path = qpath("deco", &[(".", "Widget"), (".", "size")]);
    let mut node = NodeRecord::new("size", "Function", "size", path.display(), "deco.py", "python");
    node.qualified_path = Some(path);
    upsert_node(&mut conn, node).unwrap();

    let body = json_body(&resolved_by_name(&conn, "@Widget.size").unwrap());

    assert_eq!(body["id"], "size", "{body}");
    assert_eq!(body["resolvedBy"], "qualifiedNameSuffix", "{body}");
    assert_eq!(body["queriedAs"], "Widget.size", "{body}");
}

/// T10: a remainder the language refuses is never looked up, so a path can
/// never resolve to a `File` node by the back door.
///
/// Control: drop `&& !shapes.refused.matches(remainder)` from
/// `QueryShapes::rewrites` - `src/app.ts` resolves to the file and this fails.
#[test]
fn a_prefixed_path_is_still_refused() {
    let mut conn = setup();
    upsert_node(
        &mut conn,
        NodeRecord::new("file", "File", "app.ts", "src/app.ts", "src/app.ts", "typescript"),
    )
    .unwrap();

    assert!(refused(&conn, "@src/app.ts"));
}

/// T11: `@scope/pkg` keeps today's answer, the import note: its remainder
/// is refused, so no lookup runs, and the later rungs see the original query.
///
/// Control: have `by_stripped_prefix` hand its remainder to the rest of the
/// ladder (`by_file_name(conn, semantic, remainder)`) and drop the shape check
/// - the answer is then about `scope/pkg` and this fails.
#[test]
fn a_scoped_package_keeps_its_import_note() {
    let mut conn = setup();
    let mut ph = NodeRecord::new("ph", "Module", "pkg", "@scope/pkg", "src/a.ts", "typescript");
    ph.native_kind = Some(crate::graph::imports::EXTERNAL_MODULE_NATIVE_KIND.to_string());
    upsert_node(&mut conn, ph).unwrap();
    ts_decl(&mut conn, "deco", "Component", "Component", "typescript");

    let text = error_text(&resolved_by_name(&conn, "@scope/pkg").unwrap());

    assert!(text.contains("'@scope/pkg' (1)"), "{text}");
    assert!(text.contains("get_dependencies"), "{text}");
}

/// T12: a prefixed query nothing declares still stops before the semantic
/// rung: the rewrite is structural only, and every shipped language refuses
/// `@`, so nothing is embedded.
///
/// Control: run the semantic rung on the remainder when the rewrite misses
/// (`by_semantic_neighbours(conn, semantic, remainder)`) - `NoSuchThing` is
/// recorded in `reached` and this fails.
#[test]
fn a_prefixed_unknown_name_is_never_embedded() {
    let mut conn = setup_with_vectors();
    ts_decl(&mut conn, "deco", "Component", "Component", "typescript");
    let embedding = unloaded_pipeline();

    let rung = SemanticRung::deferred(&embedding, QueryShapes::shipped());
    let result = by_name(&conn, None, &rung, "@NoSuchThing", None).unwrap();

    assert_eq!(rung.reached(), None);
    assert_eq!(error_text(&result), "g-mesh: no symbol named '@NoSuchThing' found");
}

/// T13: the query as typed goes first, so a declaration whose qualifiedName
/// is literally the query wins over the rewrite's hit.
///
/// Control: call `by_stripped_prefix` at the top of `resolve_symbol_name` -
/// the answer is the ambiguous `Component` page and this fails.
#[test]
fn the_original_query_wins_over_a_rewrite() {
    let mut conn = setup();
    ts_decl(&mut conn, "literal", "Component", "@Component", "typescript");
    ts_decl(&mut conn, "deco", "Component", "Component", "typescript");

    let body = json_body(&resolved_by_name(&conn, "@Component").unwrap());

    assert_eq!(body["id"], "literal", "{body}");
    assert!(body.get("queriedAs").is_none(), "{body}");
}

/// A TypeScript declaration stored with `path`, named after its last segment.
fn ts_path_decl(conn: &mut Connection, id: &str, path: QualifiedPath) {
    let name = path.last().unwrap().name.clone();
    let mut node = NodeRecord::new(id, "Function", name, path.display(), format!("{id}.ts"), "typescript");
    node.qualified_path = Some(path);
    upsert_node(conn, node).unwrap();
}

/// The remainder resolves on the rewrite's own exact-qualifiedName rung,
/// where the declaration's qualifiedName is not its name - a class member
/// spelled `@Widget.size`.
///
/// Control: delete the `exact.len() == 1 && exact[0].0.name != exact[0].1`
/// early return from `by_stripped_prefix` - `Widget.size` is then no
/// declaration's name and has no stored suffix, so the query is refused and
/// this fails.
#[test]
fn a_rewrite_resolves_on_the_exact_qualified_name_rung() {
    let mut conn = setup();
    ts_decl(&mut conn, "method", "size", "Widget.size", "typescript");

    let body = json_body(&resolved_by_name(&conn, "@Widget.size").unwrap());

    assert_eq!(body["id"], "method", "{body}");
    assert_eq!(body["resolvedBy"], "qualifiedName", "{body}");
    assert_eq!(body["queriedAs"], "Widget.size", "{body}");
}

/// Two declarations carry the remainder as their qualifiedName: the
/// rewrite's answer is the page over the qualifiedName column, labelled with
/// the name it looked up.
///
/// Controls: drop the `(_, 2..) => Some(NameColumn::QualifiedName)` arm from
/// `by_stripped_prefix`'s `ambiguous_over` - the query is refused and this
/// fails; or pass `None` for `page_label` to `CandidatePage::ambiguous` -
/// `queriedAs` is missing and this fails.
#[test]
fn a_rewrite_ambiguous_over_qualified_names_is_a_labelled_page() {
    let mut conn = setup();
    ts_decl(&mut conn, "a", "size", "Widget.size", "typescript");
    ts_decl(&mut conn, "b", "size", "Widget.size", "typescript");

    let body = json_body(&resolved_by_name(&conn, "@Widget.size").unwrap());

    assert_eq!(body["ambiguous"], true, "{body}");
    assert_eq!(body["resolvedBy"], "nameAmbiguous", "{body}");
    assert_eq!(body["queriedAs"], "Widget.size", "{body}");
    assert_eq!(ids(&body), vec!["a", "b"], "{body}");
}

/// Two declarations store the remainder as a qualifiedName suffix: the
/// rewrite's answer is the suffix rung's page, labelled with the name it
/// looked up.
///
/// Controls: in `by_stripped_prefix`'s suffix `match`, answer the `_` arm
/// with `answer(suffixed.remove(0), ResolvedBy::QualifiedNameSuffix)` - one
/// declaration resolves and this fails; or pass `None` for `page_label` -
/// `queriedAs` is missing and this fails.
#[test]
fn a_rewrite_ambiguous_at_the_suffix_rung_is_a_labelled_page() {
    let mut conn = setup();
    ts_path_decl(&mut conn, "a", qpath("a", &[(".", "Widget"), (".", "size")]));
    ts_path_decl(&mut conn, "b", qpath("b", &[(".", "Widget"), (".", "size")]));

    let body = json_body(&resolved_by_name(&conn, "@Widget.size").unwrap());

    assert_eq!(body["ambiguous"], true, "{body}");
    assert_eq!(body["resolvedBy"], "nameAmbiguous", "{body}");
    assert_eq!(body["queriedAs"], "Widget.size", "{body}");
    assert_eq!(ids(&body), vec!["a", "b"], "{body}");
}

/// The query as typed wins on the name rung: a declaration literally named
/// `@Component` resolves even though the rewrite would find `Component`.
///
/// Control: call `by_stripped_prefix` before `queries::find_by_name` in
/// `resolve_symbol_name` (returning its answer when it has one) - `deco`
/// resolves with `queriedAs` and this fails.
#[test]
fn the_original_query_wins_over_a_rewrite_on_the_name_rung() {
    let mut conn = setup();
    ts_decl(&mut conn, "literal", "@Component", "deco.@Component", "typescript");
    ts_decl(&mut conn, "deco", "Component", "Component", "typescript");

    let body = json_body(&resolved_by_name(&conn, "@Component").unwrap());

    assert_eq!(body["id"], "literal", "{body}");
    assert_eq!(body["resolvedBy"], "name", "{body}");
    assert!(body.get("queriedAs").is_none(), "{body}");
}

/// The query as typed wins on the suffix rung: a declaration storing
/// `@Widget.size` as a suffix resolves even though the rewrite would find
/// `Widget.size` by its exact qualifiedName.
///
/// Control: swap the order of `by_qualified_name_suffix` and
/// `by_stripped_prefix` in `resolve_symbol_name` - `rewritten` resolves with
/// `queriedAs` and this fails.
#[test]
fn the_original_query_wins_over_a_rewrite_on_the_suffix_rung() {
    let mut conn = setup();
    ts_path_decl(&mut conn, "literal", qpath("deco", &[(".", "@Widget"), (".", "size")]));
    ts_decl(&mut conn, "rewritten", "size", "Widget.size", "typescript");

    let body = json_body(&resolved_by_name(&conn, "@Widget.size").unwrap());

    assert_eq!(body["id"], "literal", "{body}");
    assert_eq!(body["resolvedBy"], "qualifiedNameSuffix", "{body}");
    assert!(body.get("queriedAs").is_none(), "{body}");
}

/// A rewritten ambiguous page continues through its cursor: the second page
/// holds exactly the remainder and still says what was looked up.
///
/// Control: pass `None` in place of `cursor` to `find_candidates_by_name` in
/// `by_stripped_prefix` - the second call returns the first page again and
/// this fails.
#[test]
fn a_rewritten_page_continues_through_its_cursor() {
    let mut conn = setup();
    let total = CANDIDATE_PAGE_SIZE + 1;
    for i in 0..total {
        ts_decl(&mut conn, &format!("c{i:02}"), "Component", &format!("m{i}.Component"), "typescript");
    }
    let store = Arc::new(IndexStore::new(conn));
    let page = |cursor: Option<String>| {
        let params = FindDefinitionParams {
            symbol_id: None,
            symbol_name: Some("@Component".to_string()),
            file_path: None,
            position: None,
            cursor,
            include_source: None,
        };
        json_body(
            &handle(&store, &no_sources(), &EmbeddingPipeline::disabled(), QueryShapes::shipped(), params)
                .unwrap(),
        )
    };

    let first = page(None);
    assert_eq!(first["queriedAs"], "Component", "{first}");
    assert_eq!(first["results"].as_array().unwrap().len(), CANDIDATE_PAGE_SIZE, "{first}");
    assert_eq!(first["hasMore"], true, "{first}");
    let second = page(Some(first["nextCursor"].as_str().unwrap().to_string()));

    assert_eq!(second["queriedAs"], "Component", "{second}");
    assert_eq!(second["results"].as_array().unwrap().len(), 1, "{second}");
    assert_eq!(second["hasMore"], false, "{second}");
    let mut all: Vec<&str> = ids(&first).into_iter().chain(ids(&second)).collect();
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total, "every candidate exactly once across both pages");
}

// --- candidate lines, symbol_id and sourced candidates -----------------

/// A project holding each `(path, body)` on disk.
fn project_files(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create a temp project");
    for (path, body) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).expect("failed to create the fixture directory");
        std::fs::write(full, body).expect("failed to write the fixture");
    }
    dir
}

/// One `run` per `(id, file, start, end)`.
fn runs(spans: &[(&str, &str, i64, i64)]) -> Arc<IndexStore> {
    let mut conn = setup();
    for (id, file, start, end) in spans {
        let mut node = node_with_span(id, "run", &format!("{id}::run"), file, (*end, 0));
        node.start_line = *start;
        upsert_node(&mut conn, node).unwrap();
    }
    Arc::new(IndexStore::new(conn))
}

fn definition_params() -> FindDefinitionParams {
    FindDefinitionParams {
        symbol_id: None,
        symbol_name: None,
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    }
}

fn define(store: &Arc<IndexStore>, root: &std::path::Path, params: FindDefinitionParams) -> CallToolResult {
    handle(store, root, &EmbeddingPipeline::disabled(), QueryShapes::shipped(), params).unwrap()
}

fn named(name: &str) -> FindDefinitionParams {
    FindDefinitionParams { symbol_name: Some(name.to_string()), ..definition_params() }
}

fn by_id(id: &str) -> FindDefinitionParams {
    FindDefinitionParams { symbol_id: Some(id.to_string()), ..definition_params() }
}

fn candidate_ids(body: &serde_json::Value) -> Vec<String> {
    body["results"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn ambiguous_candidates_carry_their_start_and_end_lines() {
    let store = runs(&[("a", "a.rs", 3, 7), ("b", "b.rs", 10, 12)]);

    let body = json_body(&define(&store, &no_sources(), named("run")));

    let lines: Vec<(String, i64, i64)> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().to_string(),
                c["startLine"].as_i64().unwrap(),
                c["endLine"].as_i64().unwrap(),
            )
        })
        .collect();
    let mut lines = lines;
    lines.sort();
    assert_eq!(lines, vec![("a".to_string(), 3, 7), ("b".to_string(), 10, 12)], "{body}");
}

/// The page's explanation names `symbol_id` as the field to re-query with,
/// and following it literally resolves the chosen candidate.
#[test]
fn following_the_ambiguous_explanation_resolves_the_chosen_candidate_by_id() {
    let store = runs(&[("a", "a/lib.rs", 0, 0), ("b", "b/lib.rs", 1, 3)]);
    let project = project_files(&[("b/lib.rs", "// head\nfn run() {\n    work();\n}\n")]);

    let page = json_body(&define(&store, &no_sources(), named("run")));
    assert_eq!(page["explanation"], crate::mcp::session_hints::AMBIGUOUS, "{page}");
    assert!(page["explanation"].as_str().unwrap().contains("`symbol_id`"), "{page}");

    let body = json_body(&define(&store, project.path(), by_id("b")));

    assert_eq!(body["id"], "b", "{body}");
    assert_eq!(body["resolvedBy"], "id", "{body}");
    assert_eq!(body["source"]["text"], "fn run() {\n    work();\n}", "{body}");
}

#[test]
fn symbol_id_beside_a_name_or_a_position_is_refused() {
    let store = runs(&[("a", "a.rs", 0, 0)]);
    let position = crate::protocol::types::Position { line: 0, col: 0 };
    let mixed = [
        FindDefinitionParams { symbol_name: Some("run".to_string()), ..by_id("a") },
        FindDefinitionParams { file_path: Some("a.rs".to_string()), position: Some(position), ..by_id("a") },
        FindDefinitionParams { file_path: Some("a.rs".to_string()), ..by_id("a") },
    ];

    for params in mixed {
        let text = error_text(&define(&store, &no_sources(), params));
        assert!(text.contains("give `symbol_id` alone"), "{text}");
    }
}

#[test]
fn an_unknown_symbol_id_is_a_tool_level_error() {
    let store = runs(&[("a", "a.rs", 0, 0)]);

    let text = error_text(&define(&store, &no_sources(), by_id("nope")));

    assert!(text.contains("no symbol with id 'nope'"), "{text}");
}

/// Three readings of one name, each a different file on disk.
fn three_runs() -> (Arc<IndexStore>, tempfile::TempDir) {
    let store = runs(&[("a", "a.rs", 0, 2), ("b", "b.rs", 1, 1), ("c", "c.rs", 0, 0)]);
    let project = project_files(&[
        ("a.rs", "fn run() {\n    alpha();\n}\n"),
        ("b.rs", "// b\nfn run() { beta() }\n"),
        ("c.rs", "fn run() {}\n"),
    ]);
    (store, project)
}

#[test]
fn three_candidates_on_a_complete_first_page_each_carry_their_source() {
    let (store, project) = three_runs();

    let sourced = json_body(&define(&store, project.path(), named("run")));
    let bare = json_body(&define(
        &store,
        project.path(),
        FindDefinitionParams { include_source: Some(false), ..named("run") },
    ));

    assert_eq!(sourced["hasMore"], false, "precondition: {sourced}");
    assert_eq!(sourced["explanation"], crate::mcp::session_hints::AMBIGUOUS_SOURCED, "{sourced}");
    let texts: HashMap<String, String> = sourced["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let text = c["source"]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("every candidate is sourced: {sourced}"));
            (c["id"].as_str().unwrap().to_string(), text.to_string())
        })
        .collect();
    assert_eq!(texts["a"], "fn run() {\n    alpha();\n}");
    assert_eq!(texts["b"], "fn run() { beta() }");
    assert_eq!(texts["c"], "fn run() {}");
    assert_eq!(candidate_ids(&sourced), candidate_ids(&bare), "sourcing does not reorder the candidates");
}

#[test]
fn include_source_false_gives_candidates_their_lines_without_source() {
    let (store, project) = three_runs();

    let body = json_body(&define(
        &store,
        project.path(),
        FindDefinitionParams { include_source: Some(false), ..named("run") },
    ));

    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS, "{body}");
    for candidate in body["results"].as_array().unwrap() {
        assert!(candidate.get("source").is_none(), "{body}");
        assert!(candidate["startLine"].is_i64() && candidate["endLine"].is_i64(), "{body}");
    }
}

/// The ids of the page's candidates that carry `source`, sorted.
fn sourced_ids(body: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = body["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c.get("source").is_some())
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// Two readable candidates and one whose `endLine` is past the end of its
/// 2-line file - a file shortened since the walk. (`(2, 0)` would not do: that
/// is an old index's whole-file end, which is read - GM-527.) Its span is
/// refused, so the page carries source for some candidates only and must not
/// claim "each with its source".
fn runs_one_past_the_end() -> (Arc<IndexStore>, tempfile::TempDir) {
    let store = runs(&[("a", "a.rs", 0, 2), ("b", "b.rs", 1, 1), ("m", "m.rs", 0, 5)]);
    let project = project_files(&[
        ("a.rs", "fn run() {\n    alpha();\n}\n"),
        ("b.rs", "// b\nfn run() { beta() }\n"),
        ("m.rs", "run = 1\nother = 2\n"),
    ]);
    (store, project)
}

#[test]
fn a_page_where_one_candidates_span_ends_past_the_file_says_only_some_are_sourced() {
    let (store, project) = runs_one_past_the_end();

    let body = json_body(&define(&store, project.path(), named("run")));

    assert_eq!(body["hasMore"], false, "precondition: {body}");
    assert_eq!(body["results"].as_array().unwrap().len(), 3, "precondition: {body}");
    assert_eq!(sourced_ids(&body), vec!["a", "b"], "the past-the-end span is refused, the rest read: {body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_PARTLY_SOURCED, "{body}");
    assert_ne!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_SOURCED, "{body}");
}

/// A file deleted since the walk is the other way a candidate loses its
/// source; the page says "some" for it just the same.
#[test]
fn a_page_where_one_candidates_file_is_missing_says_only_some_are_sourced() {
    let store = runs(&[("a", "a.rs", 0, 2), ("gone", "gone.rs", 0, 0)]);
    let project = project_files(&[("a.rs", "fn run() {\n    alpha();\n}\n")]);

    let body = json_body(&define(&store, project.path(), named("run")));

    assert_eq!(sourced_ids(&body), vec!["a"], "{body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_PARTLY_SOURCED, "{body}");
}

/// The partly-sourced explanation sends the reader to the candidates it
/// could not show, by the field the page uses to mark them.
#[test]
fn the_partly_sourced_explanation_names_how_to_reach_the_unsourced_candidates() {
    let text = crate::mcp::session_hints::AMBIGUOUS_PARTLY_SOURCED;

    assert!(text.contains("`symbol_id`"), "{text}");
    assert!(text.contains("`source`"), "{text}");
    assert_ne!(text, crate::mcp::session_hints::AMBIGUOUS_SOURCED);
    assert_ne!(text, crate::mcp::session_hints::AMBIGUOUS);
}

/// Opting out of source is "none sourced", not "some": the mixed page with
/// `include_source: false` reads exactly like any unsourced page.
#[test]
fn include_source_false_on_a_mixed_page_is_plainly_ambiguous() {
    let (store, project) = runs_one_past_the_end();

    let body = json_body(&define(
        &store,
        project.path(),
        FindDefinitionParams { include_source: Some(false), ..named("run") },
    ));

    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS, "{body}");
    assert!(sourced_ids(&body).is_empty(), "{body}");
}

/// When every candidate's span is refused the page carries no source at
/// all, and says the plain thing rather than "some".
#[test]
fn a_page_where_no_candidates_span_can_be_read_is_plainly_ambiguous() {
    let store = runs(&[("a", "a.rs", 0, 2), ("b", "b.rs", 5, 9)]);
    let project = project_files(&[("a.rs", "fn run() {}\n"), ("b.rs", "fn run() {}\n")]);

    let body = json_body(&define(&store, project.path(), named("run")));

    assert_eq!(body["results"].as_array().unwrap().len(), 2, "precondition: {body}");
    assert!(sourced_ids(&body).is_empty(), "{body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS, "{body}");
}

/// The largest set that is sourced (GM-526): four candidates on a complete
/// first page each carry their own lines.
#[test]
fn four_candidates_on_a_complete_first_page_each_carry_their_source() {
    let store = runs(&[("a", "a.rs", 0, 0), ("b", "b.rs", 0, 0), ("c", "c.rs", 0, 0), ("d", "d.rs", 0, 0)]);
    let project = project_files(&[
        ("a.rs", "fn run() { alpha() }\n"),
        ("b.rs", "fn run() { beta() }\n"),
        ("c.rs", "fn run() { gamma() }\n"),
        ("d.rs", "fn run() { delta() }\n"),
    ]);

    let body = json_body(&define(&store, project.path(), named("run")));

    assert_eq!(body["results"].as_array().unwrap().len(), 4, "precondition: {body}");
    assert_eq!(body["hasMore"], false, "precondition: {body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_SOURCED, "{body}");
    assert_eq!(sourced_ids(&body), vec!["a", "b", "c", "d"], "{body}");
    for (id, call) in [("a", "alpha"), ("b", "beta"), ("c", "gamma"), ("d", "delta")] {
        let candidate = body["results"].as_array().unwrap().iter().find(|c| c["id"] == id).unwrap();
        assert_eq!(candidate["source"]["text"], format!("fn run() {{ {call}() }}"), "{body}");
    }
}

/// One past the largest sourced set: five candidates on a complete first page
/// carry no source at all.
#[test]
fn five_candidates_carry_no_source() {
    let store = runs(&[
        ("a", "a.rs", 0, 0),
        ("b", "b.rs", 0, 0),
        ("c", "c.rs", 0, 0),
        ("d", "d.rs", 0, 0),
        ("e", "e.rs", 0, 0),
    ]);
    let project = project_files(&[
        ("a.rs", "fn run() {}\n"),
        ("b.rs", "fn run() {}\n"),
        ("c.rs", "fn run() {}\n"),
        ("d.rs", "fn run() {}\n"),
        ("e.rs", "fn run() {}\n"),
    ]);

    let body = json_body(&define(&store, project.path(), named("run")));

    assert_eq!(body["results"].as_array().unwrap().len(), 5, "precondition: {body}");
    assert_eq!(body["hasMore"], false, "precondition: {body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS, "{body}");
    assert!(sourced_ids(&body).is_empty(), "{body}");
}

/// A later page holds a few candidates, but they are not the whole set, so
/// they are not sourced either.
#[test]
fn a_short_continuation_page_carries_no_source() {
    let count = CANDIDATE_PAGE_SIZE + 2;
    let ids: Vec<String> = (0..count).map(|i| format!("r{i:02}")).collect();
    let files: Vec<String> = ids.iter().map(|id| format!("{id}.rs")).collect();
    let spans: Vec<(&str, &str, i64, i64)> =
        ids.iter().zip(&files).map(|(id, file)| (id.as_str(), file.as_str(), 0, 0)).collect();
    let store = runs(&spans);
    let bodies: Vec<(&str, &str)> = files.iter().map(|file| (file.as_str(), "fn run() {}\n")).collect();
    let project = project_files(&bodies);

    let first = json_body(&define(&store, project.path(), named("run")));
    assert_eq!(first["hasMore"], true, "precondition: {first}");
    let cursor = first["nextCursor"].as_str().unwrap().to_string();

    let second = json_body(&define(
        &store,
        project.path(),
        FindDefinitionParams { cursor: Some(cursor), ..named("run") },
    ));

    assert_eq!(second["results"].as_array().unwrap().len(), 2, "precondition: {second}");
    assert_eq!(second["hasMore"], false, "precondition: {second}");
    assert_eq!(second["explanation"], crate::mcp::session_hints::AMBIGUOUS, "{second}");
    for candidate in second["results"].as_array().unwrap() {
        assert!(candidate.get("source").is_none(), "{second}");
    }
}

#[test]
fn a_candidates_source_is_cut_at_twenty_lines_and_says_so() {
    let body: String = (0..28).map(|n| format!("    let x{n} = {n};\n")).collect();
    let store = runs(&[("a", "a.rs", 0, 29), ("b", "b.rs", 0, 0)]);
    let project = project_files(&[("a.rs", &format!("fn run() {{\n{body}}}\n")), ("b.rs", "fn run() {}\n")]);

    let page = json_body(&define(&store, project.path(), named("run")));

    let long = page["results"].as_array().unwrap().iter().find(|c| c["id"] == "a").expect("candidate a");
    let text = long["source"]["text"].as_str().unwrap_or_else(|| panic!("a sourced candidate: {page}"));
    assert_eq!(text.lines().count(), CANDIDATE_SOURCE_LINES, "{page}");
    assert_eq!(long["source"]["omittedLines"], 30 - CANDIDATE_SOURCE_LINES, "{page}");
}

/// The anchored tools answer an ambiguous name with the same candidates,
/// lines included, and never with their source.
#[test]
fn an_anchored_tools_ambiguous_page_carries_lines_and_no_source() {
    let store = runs(&[("a", "a.rs", 3, 7), ("b", "b.rs", 10, 12)]);

    let result = crate::mcp::find_references::handle(
        &store,
        &EmbeddingPipeline::disabled(),
        QueryShapes::shipped(),
        &HashMap::new(),
        &crate::mcp::session_hints::SessionHints::default(),
        crate::mcp::SymbolQueryParams { symbol_name: Some("run".to_string()), ..Default::default() },
    )
    .unwrap();
    let body = json_body(&result);

    assert_eq!(body["ambiguous"], true, "precondition: {body}");
    for candidate in body["results"].as_array().unwrap() {
        assert!(candidate["startLine"].is_i64() && candidate["endLine"].is_i64(), "{body}");
        assert!(candidate.get("source").is_none(), "{body}");
    }
}

// ---------------------------------------------------------------------
// a position miss on a path whose language is not indexed at all
// ---------------------------------------------------------------------

use crate::mcp::not_indexed::test_support::*;

fn at_position(file_path: &str) -> FindDefinitionParams {
    FindDefinitionParams {
        symbol_id: None,
        symbol_name: None,
        file_path: Some(file_path.to_string()),
        position: Some(crate::protocol::types::Position { line: 0, col: 0 }),
        cursor: None,
        include_source: None,
    }
}

fn covered_call(
    conn: Connection,
    coverage: Option<&PathCoverage>,
    params: FindDefinitionParams,
) -> CallToolResult {
    handle_in(&Arc::new(IndexStore::new(conn)), &no_sources(), &SemanticRung::off(), coverage, params)
        .unwrap()
}

/// File + position on an absent language's file is
/// refused with the install command.
///
/// Control: pass `None` instead of `coverage` from `handle_in` to
/// `by_position` (or call `error(..)` in its `None` arm) - the body is not
/// JSON.
#[test]
fn a_position_in_an_absent_languages_file_is_refused_with_the_install_command() {
    let result = covered_call(setup(), Some(&python_absent()), at_position("tools/gen.py"));
    assert_python_absent_refusal(&refusal_body(&result), "g-mesh: no symbol found at tools/gen.py:0:0");
}

/// A position in a failed language's file names `g-mesh reindex` and the innermost cause.
///
/// Control: as above.
#[test]
fn a_position_in_a_failed_languages_file_is_refused_with_the_reindex_command() {
    let conn = setup();
    record_failed(&conn, "python");
    let result = covered_call(conn, Some(&python_failed()), at_position("tools/gen.py"));
    assert_python_failed_refusal(&refusal_body(&result), "g-mesh: no symbol found at tools/gen.py:0:0");
}

/// A covered language's position miss stays the plain message.
///
/// Control: make `not_indexed::miss`'s `None` arm build a refusal.
#[test]
fn a_position_miss_in_a_covered_language_stays_the_plain_message() {
    let result = covered_call(setup(), None, at_position("src/nope.rs"));
    assert_eq!(plain_error(&result), "g-mesh: no symbol found at src/nope.rs:0:0");
}

/// The name mode never carries the field, even when the
/// caller's coverage says the (unused) path is uncovered.
///
/// Control: route `coverage` into `by_name`'s miss (`not_indexed::miss`
/// there) - the answer becomes JSON.
#[test]
fn a_name_miss_never_carries_the_not_indexed_reason() {
    let params = FindDefinitionParams {
        symbol_id: None,
        symbol_name: Some("does_not_exist".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result = covered_call(setup(), Some(&python_absent()), params);
    assert!(plain_error(&result).contains("does_not_exist"));
}

/// A hit answers normally whatever the coverage.
///
/// Control: refuse whenever `coverage` is `Some` in `by_position`.
#[test]
fn a_position_hit_carries_no_not_indexed_key_whatever_the_coverage() {
    let mut conn = setup();
    let mut node = node_with_span("n1", "gen", "tools.gen.gen", "tools/gen.py", (5, 0));
    node.language = "python".to_string();
    upsert_node(&mut conn, node).unwrap();
    let body = json_body(&covered_call(conn, Some(&python_absent()), at_position("tools/gen.py")));
    assert_eq!(body["id"], "n1");
    assert!(body.get("notIndexed").is_none(), "{body}");
}

// --- a whole-file candidate's source, new and old index (GM-527) ----------

/// requests' `__version__.py`: 14 lines and a final newline. Its name is
/// both the module's and a variable's in it, so `__version__` is an
/// ambiguous page with a whole-file candidate on it.
const VERSION_PY: &str = "# requests\n\n\n\
    __title__ = \"requests\"\n\
    __description__ = \"Python HTTP for Humans.\"\n\
    __url__ = \"https://requests.readthedocs.io\"\n\
    __version__ = \"2.32.3\"\n\
    __build__ = 0x023203\n\
    __author__ = \"Kenneth Reitz\"\n\
    __author_email__ = \"me@kennethreitz.org\"\n\
    __license__ = \"Apache-2.0\"\n\
    __copyright__ = \"Copyright Kenneth Reitz\"\n\
    \n\
    __cake__ = \"cake\"\n";

/// The module `requests.__version__` (whole-file span ending at
/// `module_end`, a `(line, col)`) and its variable `__version__` (line 6),
/// with `VERSION_PY` on disk.
fn version_page(module_end: (i64, i64)) -> (Arc<IndexStore>, tempfile::TempDir) {
    let mut conn = setup();
    let file = "requests/__version__.py";
    let mut module =
        NodeRecord::new("module", "Module", "__version__", "requests.__version__", file, "python");
    module.end_line = module_end.0;
    module.end_col = module_end.1;
    upsert_node(&mut conn, module).unwrap();
    let mut variable = NodeRecord::new(
        "variable",
        "Variable",
        "__version__",
        "requests.__version__.__version__",
        file,
        "python",
    );
    variable.start_line = 6;
    variable.end_line = 6;
    variable.end_col = 22;
    upsert_node(&mut conn, variable).unwrap();
    (Arc::new(IndexStore::new(conn)), project_files(&[(file, VERSION_PY)]))
}

/// The page's `source.text` per candidate id.
fn source_texts(body: &serde_json::Value) -> HashMap<String, String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| Some((c["id"].as_str()?.to_string(), c["source"]["text"].as_str()?.to_string())))
        .collect()
}

/// A new index ends the module on its last real line, `(13, 17)`: an
/// ordinary span, read like any other, so every candidate carries source.
/// (No control of its own: it pins that the new end needs no clamp; the
/// old-index test below carries this behaviour's control.)
#[test]
fn a_whole_file_candidate_in_a_new_index_carries_its_source() {
    assert_eq!(VERSION_PY.lines().count(), 14, "precondition: requests' shape");
    let (store, project) = version_page((13, 17));

    let body = json_body(&define(&store, project.path(), named("__version__")));

    assert_eq!(body["results"].as_array().unwrap().len(), 2, "precondition: {body}");
    assert_eq!(sourced_ids(&body), vec!["module", "variable"], "{body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_SOURCED, "{body}");
    let texts = source_texts(&body);
    assert_eq!(texts["module"], VERSION_PY.trim_end(), "the whole file: {body}");
    assert_eq!(texts["variable"], "__version__ = \"2.32.3\"", "{body}");
}

/// An index built before GM-527 holds the module's end as `(14, 0)`, one
/// past the last line. The page still reads the whole file for it, so it is
/// "each with its source", not "some". The end column never reaches the
/// page's JSON.
///
/// Control: pass `None` instead of `candidate.end_col` in
/// `CandidatePage::ambiguous` (or remove the clamp in `read_span_within`);
/// the module loses its source and the explanation becomes PARTLY_SOURCED.
#[test]
fn a_whole_file_candidate_in_an_old_index_still_carries_its_source() {
    let (store, project) = version_page((14, 0));

    let body = json_body(&define(&store, project.path(), named("__version__")));

    assert_eq!(body["results"].as_array().unwrap().len(), 2, "precondition: {body}");
    assert_eq!(sourced_ids(&body), vec!["module", "variable"], "{body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_SOURCED, "{body}");
    assert_eq!(source_texts(&body)["module"], VERSION_PY.trim_end(), "the whole file: {body}");
    for candidate in body["results"].as_array().unwrap() {
        assert!(candidate.get("endCol").is_none(), "the page's shape is unchanged: {body}");
    }
}

/// A whole-file end past even the old convention - the file was shortened
/// since the walk - is stale, and the page says only some are sourced.
///
/// Control: make the clamp in `read_span_within` unconditional; the module
/// is read and the explanation becomes AMBIGUOUS_SOURCED.
#[test]
fn a_whole_file_candidate_past_the_old_end_is_stale_and_unsourced() {
    let (store, project) = version_page((20, 0));

    let body = json_body(&define(&store, project.path(), named("__version__")));

    assert_eq!(sourced_ids(&body), vec!["variable"], "{body}");
    assert_eq!(body["explanation"], crate::mcp::session_hints::AMBIGUOUS_PARTLY_SOURCED, "{body}");
}
