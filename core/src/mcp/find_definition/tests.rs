use super::*;
use crate::graph::queries::{upsert_edge, upsert_node};
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
    by_name(conn, None, None, name, None)
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
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result =
        handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
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
        symbol_name: Some("mutate".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let body = json_body(
        &handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
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
    let candidates = find_candidates_by_name(&conn, NameColumn::Name, "app", None).unwrap();
    assert_eq!(candidates.results.len(), 1, "the container node must not be ranked");

    let params = FindDefinitionParams {
        symbol_name: Some("app".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let body = json_body(
        &handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
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
        symbol_name: None,
        file_path: Some("a/lib.rs".to_string()),
        position: Some(crate::protocol::types::Position { line: 2, col: 0 }),
        cursor: None,
        include_source: None,
    };
    let result =
        handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
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
        symbol_name: Some("pkg_b::run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result =
        handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
            .unwrap();
    let body = json_body(&result);
    assert_eq!(body["id"], "n2");
    assert_eq!(body["qualifiedName"], "pkg_b::run");
}

#[test]
fn no_match_is_a_tool_level_error() {
    let conn = setup();
    let params = FindDefinitionParams {
        symbol_name: Some("does_not_exist".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result =
        handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
            .unwrap();
    assert!(error_text(&result).contains("does_not_exist"));
}

#[test]
fn neither_name_nor_position_is_a_tool_level_error() {
    let conn = setup();
    let params = FindDefinitionParams {
        symbol_name: None,
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let result =
        handle(&Arc::new(IndexStore::new(conn)), &no_sources(), &EmbeddingPipeline::disabled(), params)
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
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: None,
    };
    let first = handle(&conn, &no_sources(), &EmbeddingPipeline::disabled(), first_params).unwrap();
    let first_body = json_body(&first);
    let first_results = first_body["results"].as_array().unwrap();
    assert_eq!(first_results.len(), CANDIDATE_PAGE_SIZE);
    assert_eq!(first_body["hasMore"], true);
    let cursor = first_body["nextCursor"].as_str().unwrap().to_string();

    let second_params = FindDefinitionParams {
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: Some(cursor),
        include_source: None,
    };
    let second = handle(&conn, &no_sources(), &EmbeddingPipeline::disabled(), second_params).unwrap();
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

    let by_qualified = json_body(&by_name(&conn, None, None, "pkg::run", None).unwrap());
    assert_eq!(by_qualified["resolvedBy"], "qualifiedName");

    let by_bare = json_body(&by_name(&conn, None, None, "run", None).unwrap());
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

    let body = json_body(&by_name(&conn, None, None, "DropdownMenuGroup", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "context", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "toml", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "disabled", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "fs", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "fs", None).unwrap());

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

    let result = by_name(&conn, None, None, "NoSuchThingAnywhere", None).unwrap();

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

    let body = json_body(&by_name(&conn, None, None, "run", None).unwrap());

    assert_eq!(body["ambiguous"], true);
    assert_eq!(body["resolvedBy"], "nameAmbiguous");
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

    let body = json_body(&by_name(&conn, None, None, "RegexMatcher", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "matcher::RegexMatcher", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "Binding", None).unwrap());

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

    let body = json_body(&by_name(&conn, None, None, "getNonDeletedElements", None).unwrap());

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
    json_body(&by_name(&conn, Some(project_root), None, name, None).unwrap())
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
        symbol_name: Some("run".to_string()),
        file_path: None,
        position: None,
        cursor: None,
        include_source: include,
    };

    let opted_out = json_body(
        &handle(&conn, project.path(), &EmbeddingPipeline::disabled(), params(Some(false))).unwrap(),
    );
    let default_on =
        json_body(&handle(&conn, project.path(), &EmbeddingPipeline::disabled(), params(None)).unwrap());

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

#[test]
fn a_specifier_shaped_query_is_declined_before_the_score_is_consulted() {
    // No embedding pipeline at all, so if the guard did not fire first
    // this would still return None - which is why the guard is asserted
    // directly rather than through the tool.
    assert!(is_module_specifier("@excalidraw/math"), "a scoped package is a specifier");
    assert!(is_module_specifier("packages/element/src/index.ts"), "a path is a specifier");
    assert!(!is_module_specifier("DropdownMenuGroup"), "a plain identifier is not");
    assert!(!is_module_specifier("AppState"), "nor is a type name");
}

/// The measured reason this guard exists: `@excalidraw/element` scores
/// 0.699 - above the threshold - while being junk, because only doc
/// comments and signatures are embedded and a specifier has nothing to
/// match. Score alone cannot catch it, so shape has to.
#[test]
fn a_specifier_is_refused_tersely_even_though_it_would_out_score_the_threshold() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    // Reached through the ladder, so this exercises the real miss path.
    let result = by_name(&conn, None, None, "@excalidraw/element", None).unwrap();

    assert_eq!(error_text(&result), "g-mesh: no symbol named '@excalidraw/element' found");
}

#[test]
fn without_an_embedding_pipeline_the_answer_is_exactly_what_it_was_before() {
    let conn = setup_with_vectors();
    insert_vector(&conn, "near", &[1.0, 0.0]);

    let result = by_name(&conn, None, None, "NoSuchThingAnywhere", None).unwrap();

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
            kind: "Function".to_string(),
            preview: None,
        }],
    };

    let body: serde_json::Value = serde_json::from_str(&serde_json::to_string(&page).unwrap()).unwrap();

    assert_eq!(body["resolvedBy"], "semanticNeighbours", "the rung must name itself");
    assert_eq!(body["ambiguous"], false, "these are not competing readings of one name");
    assert_eq!(body["results"][0]["id"], "near", "the handle to re-query with must be present");
}
