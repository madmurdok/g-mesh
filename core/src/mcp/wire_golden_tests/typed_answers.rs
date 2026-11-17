//! The typed answers behind the golden bytes: what `plugin_check` reads
//! (`serde_json::to_value` of a page), what `response_bound::render` sends,
//! and which `Refusal` variant a handler picks.

use serde_json::Value;

use super::*;
use crate::mcp::query::{self, Refusal, ToolAnswer, WirePage};
use crate::mcp::response_bound::wire_len;

/// The text of every content block `answer` renders to.
fn blocks<P: WirePage>(answer: &ToolAnswer<P>) -> Vec<String> {
    answer
        .rendered()
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        })
        .collect()
}

/// `answer`'s page or candidates as `plugin_check` reads them, and as the
/// JSON block a client receives parses.
fn typed_and_rendered<P: WirePage>(answer: &ToolAnswer<P>) -> (Value, Value) {
    let typed = match answer {
        ToolAnswer::Page(page) => serde_json::to_value(page),
        ToolAnswer::Candidates(candidates) => serde_json::to_value(candidates),
        ToolAnswer::Refused(refusal) => panic!("expected an answer, got a refusal: {}", refusal.message()),
    }
    .unwrap();
    let text = blocks(answer).pop().expect("an answer has a JSON block");
    (typed, serde_json::from_str(&text).unwrap())
}

/// Every answer shape `plugin_check` consumes, through the `query` doors it
/// calls, has the same `Value` typed as it has when parsed from the wire.
/// Control: make `render` change a page on its way to the wire (e.g. wrap
/// `ToolAnswer::Page` JSON in an object) - the kit's value no longer matches.
#[test]
fn every_answer_the_kit_reads_has_the_same_value_typed_as_on_the_wire() {
    let mut node = setup();
    upsert_node(&mut node, func("n1", "run", "pkg::run", "a/lib.rs", 3)).unwrap();
    let mut ambiguous = setup();
    upsert_node(&mut ambiguous, func("n1", "run", "pkg_a::run", "a/lib.rs", 3)).unwrap();
    upsert_node(&mut ambiguous, func("n2", "run", "pkg_b::run", "b/lib.rs", 3)).unwrap();
    let cut = SymbolQueryParams { limit: Some(200), ..by_id("hub") };

    let shapes: Vec<(&str, (Value, Value))> = vec![
        (
            "definition node",
            typed_and_rendered(&query::definition(&ctx(node), definition(Some("n1"), None)).unwrap()),
        ),
        (
            "definition candidates",
            typed_and_rendered(&query::definition(&ctx(ambiguous), definition(None, Some("run"))).unwrap()),
        ),
        (
            "references rows",
            typed_and_rendered(&query::references(&ctx(call_graph()), by_id("target")).unwrap()),
        ),
        (
            "references rows cut",
            typed_and_rendered(&query::references(&ctx(hub_with_many_callers()), cut.clone()).unwrap()),
        ),
        ("callers rows", typed_and_rendered(&query::callers(&ctx(call_graph()), by_id("target")).unwrap())),
        (
            "callers rows cut",
            typed_and_rendered(&query::callers(&ctx(hub_with_many_callers()), cut).unwrap()),
        ),
        (
            "implementations single hop",
            typed_and_rendered(
                &query::implementations(
                    &ctx(implementation_chain()),
                    implementations("interface", Some(false)),
                )
                .unwrap(),
            ),
        ),
        (
            "dependencies walk",
            typed_and_rendered(
                &query::dependencies(&ctx(import_graph()), dependencies("a.rs", Direction::Outgoing, None))
                    .unwrap(),
            ),
        ),
        (
            "dependencies walk truncated",
            typed_and_rendered(
                &query::dependencies(
                    &ctx(import_graph()),
                    dependencies("a.rs", Direction::Outgoing, Some(1)),
                )
                .unwrap(),
            ),
        ),
    ];
    for (shape, (typed, rendered)) in shapes {
        assert_eq!(typed, rendered, "{shape}: the typed value differs from the wire's");
    }
}

/// A page's JSON block is exactly the compact serialization `wire_len`
/// measured while cutting it: measured bytes are sent bytes. Control: send
/// pretty-printed JSON in `render`, or measure with `to_vec_pretty` in
/// `wire_len`.
#[test]
fn a_page_is_sent_as_the_compact_json_its_cut_measured() {
    fn assert_sent_as_measured<P: WirePage>(shape: &str, answer: &ToolAnswer<P>) {
        let ToolAnswer::Page(page) = answer else {
            panic!("{shape}: expected a page");
        };
        let sent = blocks(answer).pop().expect("a page has a JSON block");
        assert_eq!(sent, serde_json::to_string(page).unwrap(), "{shape}: not the compact serialization");
        assert_eq!(sent.len(), wire_len(page), "{shape}: sent bytes differ from measured bytes");
    }

    let cut = SymbolQueryParams { limit: Some(200), ..by_id("hub") };
    assert_sent_as_measured(
        "references rows cut",
        &ctx(hub_with_many_callers()).resolve_lazily(find_references::handle, cut.clone()).unwrap(),
    );
    assert_sent_as_measured(
        "callers rows cut",
        &ctx(hub_with_many_callers()).resolve_lazily(handle_callers, cut).unwrap(),
    );

    let mut conn = setup();
    let mut ids = Vec::new();
    for i in 0..120 {
        let name = format!("a_rather_long_function_name_in_the_outline_{i:03}");
        let id = format!("n{i:03}");
        upsert_node(&mut conn, func(&id, &name, &format!("pkg::{name}"), "big.rs", 1 + 4 * i)).unwrap();
        ids.push(id);
    }
    define(&mut conn, "big.rs", &ids.iter().map(String::as_str).collect::<Vec<_>>());
    let params = outline_params("big.rs", Some(200), Some(OutlineDetail::Full));
    assert_sent_as_measured("outline cut", &get_file_outline::handle(&ctx(conn), params).unwrap());

    assert_sent_as_measured(
        "dependencies walk truncated",
        &get_dependencies::handle(&ctx(import_graph()), dependencies("a.rs", Direction::Outgoing, Some(1)))
            .unwrap(),
    );

    let (_dir, search) = search_ctx(3);
    let coverage = Coverage::partial(&Phase::Embedding, 3, 10, None, Path::new("/wire-golden/project"));
    let params = SearchCodeParams { query: "reads a file".to_string(), ..Default::default() };
    let partial = search_code::handle(&search, params, coverage.as_ref()).unwrap();
    assert_eq!(blocks(&partial).len(), 2, "a partial page leads with its note");
    assert_sent_as_measured("search partial", &partial);
}

/// The anchor's redirect (`Redirect::widen`) renders byte for byte the same
/// under every anchored tool's page type, unset `isError` included. Control:
/// in `widen`, turn `Candidates` into a refusal, or render `Candidates` as
/// an error.
#[test]
fn the_anchor_redirect_renders_alike_under_every_tool() {
    let ambiguous = || {
        let mut conn = setup();
        upsert_node(&mut conn, func("n1", "run", "pkg_a::run", "a/lib.rs", 3)).unwrap();
        upsert_node(&mut conn, func("n2", "run", "pkg_b::run", "b/lib.rs", 3)).unwrap();
        conn
    };
    let file_name = || {
        let mut conn = setup();
        for (id, name, line) in [("s", "Stat", 10), ("o", "Open", 12)] {
            let mut node = NodeRecord::new(id, "Function", name, name, "fs.go", "go");
            node.start_line = line;
            node.end_line = line;
            node.exported = true;
            upsert_node(&mut conn, node).unwrap();
        }
        conn
    };
    let fixtures: [(&str, &dyn Fn() -> Connection); 2] = [("run", &ambiguous), ("fs", &file_name)];
    for (name, fixture) in fixtures {
        let expected =
            ctx(fixture()).resolve_lazily(find_definition::handle, definition(None, Some(name))).unwrap();
        assert!(matches!(expected, ToolAnswer::Candidates(_)), "{name}: find_definition must redirect");
        let expected = render(&expected);
        assert!(expected.starts_with("isError: Some(false)\n"), "{name}: {expected}");

        let implementations =
            FindImplementationsParams { symbol_name: Some(name.to_string()), ..Default::default() };
        let tools = [
            (
                "references",
                render(
                    &ctx(fixture()).resolve_lazily(find_references::handle, by_symbol_name(name)).unwrap(),
                ),
            ),
            (
                "callers",
                render(&ctx(fixture()).resolve_lazily(handle_callers, by_symbol_name(name)).unwrap()),
            ),
            (
                "callees",
                render(&ctx(fixture()).resolve_lazily(handle_callees, by_symbol_name(name)).unwrap()),
            ),
            (
                "implementations",
                render(
                    &ctx(fixture()).resolve_lazily(find_implementations::dispatch, implementations).unwrap(),
                ),
            ),
        ];
        for (tool, rendered) in tools {
            assert_eq!(rendered, expected, "{name}: {tool} renders the redirect differently");
        }
    }
}

/// `refusal` as its `NotIndexed` parts; panics on a text refusal.
fn not_indexed<P>(answer: ToolAnswer<P>) -> (String, String, Refusal) {
    let ToolAnswer::Refused(refusal) = answer else {
        panic!("expected a refusal");
    };
    let Refusal::NotIndexed { error, not_indexed } = &refusal else {
        panic!("expected a NotIndexed refusal, got text: {}", refusal.message());
    };
    (error.clone(), not_indexed.language.clone(), refusal.clone())
}

/// `answer`'s text refusal; panics on anything else.
fn text_refusal<P>(answer: ToolAnswer<P>) -> String {
    match answer {
        ToolAnswer::Refused(Refusal::Text(message)) => message,
        ToolAnswer::Refused(other) => panic!("expected a text refusal, got NotIndexed: {}", other.message()),
        _ => panic!("expected a refusal"),
    }
}

/// A miss on a path whose language is not indexed is the typed `NotIndexed`
/// refusal, whose `message()` is the prose `error` of the JSON body sent; a
/// miss on an indexed language stays a text refusal. Control: build the
/// outline miss as `Refusal::Text` of the JSON body.
#[test]
fn an_outline_miss_on_an_uncovered_path_is_a_typed_not_indexed_refusal() {
    let uncovered = QueryCtx { coverage: Some(python_absent()), ..ctx(setup()) };
    let answer = get_file_outline::handle(&uncovered, outline_params("tools/gen.py", None, None)).unwrap();
    let body: Value = serde_json::from_str(&blocks(&answer)[0]).unwrap();
    let (error, language, refusal) = not_indexed(answer);
    assert_eq!(language, "python");
    assert!(error.starts_with("g-mesh: no file 'tools/gen.py' found in the index"), "{error}");
    assert_eq!(refusal.message(), error);
    assert_eq!(body["error"], error.as_str(), "the sent body carries the same sentence: {body}");

    let plain = get_file_outline::handle(&ctx(setup()), outline_params("src/nope.rs", None, None)).unwrap();
    assert!(text_refusal(plain).contains("src/nope.rs"));
}

/// `get_dependencies` refuses a bad parameter combination and an ambiguous
/// anchor as text, and a miss on an uncovered path as `NotIndexed`. Control:
/// route a text refusal through `not_indexed::refusal`, or the uncovered
/// miss through `ToolAnswer::refused`.
#[test]
fn dependencies_refusals_are_text_except_the_not_indexed_miss() {
    let base = || GetDependenciesParams { file_path: None, ..dependencies("", Direction::Outgoing, None) };
    let refused = |params| text_refusal(get_dependencies::handle(&ctx(import_graph()), params).unwrap());
    assert!(refused(base()).contains("file_path"));
    let both = GetDependenciesParams {
        file_path: Some("a.rs".to_string()),
        module_id: Some("a.rs".to_string()),
        ..base()
    };
    assert!(refused(both).contains("not both"));
    let token_and_anchor = GetDependenciesParams {
        file_path: Some("a.rs".to_string()),
        resume_token: Some("whatever".to_string()),
        ..base()
    };
    assert!(refused(token_and_anchor).contains("resume_token"));

    let mut conn = setup();
    for path in ["packages/math/index.ts", "packages/math/sub/index.ts"] {
        upsert_node(&mut conn, NodeRecord::new(path, "File", path, path, path, "typescript")).unwrap();
    }
    let two_entry_points = QueryCtx { entry_points: vec!["index".to_string()], ..ctx(conn) };
    let message = text_refusal(
        get_dependencies::handle(&two_entry_points, dependencies("packages/math", Direction::Incoming, None))
            .unwrap(),
    );
    assert!(message.contains("packages/math/index.ts"), "{message}");
    assert!(message.contains("packages/math/sub/index.ts"), "{message}");

    let uncovered = QueryCtx { coverage: Some(python_absent()), ..ctx(setup()) };
    let answer =
        get_dependencies::handle(&uncovered, dependencies("tools/gen.py", Direction::Outgoing, None))
            .unwrap();
    let (error, language, refusal) = not_indexed(answer);
    assert_eq!(language, "python");
    assert!(error.starts_with("g-mesh: no file 'tools/gen.py' found in the index"), "{error}");
    assert_eq!(refusal.message(), error);
}
