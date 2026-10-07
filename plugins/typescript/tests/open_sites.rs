//! The open sites the structural pass leaves for a semantic tier, through the
//! full extractor: namespace member uses, overload calls and receiver calls.

mod common;

use common::{extract, extract_in, position, Graph};
use g_mesh_plugin_sdk::ids::edge_id;
use g_mesh_plugin_sdk::wire::EdgeKind::{self, Calls, References};
use g_mesh_plugin_sdk::wire::Position;
use g_mesh_plugin_sdk::OpenSiteKind::{OverloadCall, ReceiverCall, Reference};

const PATH: &str = "src/a.ts";

/// The module every namespace import in these tests reaches.
const B: &str =
    "export function m() {}\nexport function top() {}\nexport const v = 1;\nexport const r = 1;\n";

fn with_b(source: &str) -> Graph {
    extract_in(&[(PATH, source), ("src/b.ts", B)], PATH)
}

/// `needle`'s first occurrence, moved right by `offset` characters.
fn at(source: &str, needle: &str, offset: u32) -> Position {
    let start = position(source, needle, 0);
    Position { line: start.line, col: start.col + offset }
}

/// `items` in a fixed order, for comparing sites whose order the code does
/// not promise.
fn sorted<T: std::fmt::Debug>(mut items: Vec<T>) -> Vec<T> {
    items.sort_by_key(|item| format!("{item:?}"));
    items
}

// --- namespace member uses ------------------------------------------------------

#[test]
fn a_namespace_member_use_is_a_reference_site_that_calls_only_inside_a_function() {
    let source =
        "import * as ns from \"./b\";\nns.v;\nns.top();\nfunction g() {\n  ns.m();\n  return ns.r;\n}\n";
    let graph = with_b(source);
    let sites: Vec<_> = graph
        .sites(Reference)
        .into_iter()
        .map(|site| {
            (
                graph.label(&site.from_id),
                site.name.clone(),
                site.edge_kind,
                site.position,
                site.replaces.clone(),
            )
        })
        .collect();
    let expected = vec![
        ("<file>".to_string(), "v".to_string(), References, at(source, "ns.v", 3), None),
        ("<file>".to_string(), "top".to_string(), References, at(source, "ns.top", 3), None),
        ("g".to_string(), "m".to_string(), Calls, at(source, "ns.m", 3), None),
        ("g".to_string(), "r".to_string(), References, at(source, "ns.r", 3), None),
    ];
    assert_eq!(sorted(sites), sorted(expected));
    assert!(graph.sites(ReceiverCall).is_empty(), "{:?}", graph.site_summary(ReceiverCall));
}

#[test]
fn a_local_or_a_declaration_of_the_namespace_name_suppresses_its_site() {
    let local = with_b("import * as ns from \"./b\";\nfunction h(ns) { ns.m(); return ns.v; }\n");
    assert!(local.sites(Reference).is_empty(), "{:?}", local.site_summary(Reference));
    let declared = with_b("import * as ns from \"./b\";\nfunction ns() {}\nns.v;\n");
    assert!(declared.sites(Reference).is_empty(), "{:?}", declared.site_summary(Reference));
}

#[test]
fn a_namespace_import_of_no_project_file_leaves_no_reference_site() {
    let graph = with_b("import * as ext from \"lodash\";\next.v;\n");
    assert!(graph.sites(Reference).is_empty(), "{:?}", graph.site_summary(Reference));
}

#[test]
fn a_member_access_outside_a_call_is_recorded_and_its_object_still_walked() {
    let graph = with_b(
        "import * as ns from \"./b\";\n\
         function f() { return {}; }\n\
         const cfg = {};\n\
         function g() { const a = ns.v; const b = f().p; return cfg.q; }\n",
    );
    assert_eq!(graph.site_summary(Reference), vec![("g".to_string(), "v".to_string(), References)]);
    assert_edge!(graph, Calls, "g", "f");
    assert_edge!(graph, References, "g", "cfg");
}

// --- overload calls ------------------------------------------------------------

#[test]
fn each_call_of_an_overload_set_is_one_overload_call_site_refining_its_edge() {
    let source = "function o(a: string): void;\n\
                  function o(a: number): void;\n\
                  function o(a: any) {}\n\
                  function s() {}\n\
                  function g() { o(\"x\"); o(1); s(); }\n\
                  o(2);\n";
    let graph = extract(PATH, source);
    let calls = edge_id(&graph.id("g"), Calls, &graph.id("o"), None);
    assert!(graph.graph.edges.iter().any(|edge| edge.id == calls), "{:?}", graph.pairs(Calls));
    let sites: Vec<_> = graph
        .sites(OverloadCall)
        .into_iter()
        .map(|site| {
            (
                graph.label(&site.from_id),
                site.name.clone(),
                site.edge_kind,
                site.position,
                site.replaces.clone(),
            )
        })
        .collect();
    let expected = vec![
        ("g".to_string(), "o".to_string(), Calls, at(source, "o(\"x\")", 0), Some(calls.clone())),
        ("g".to_string(), "o".to_string(), Calls, at(source, "o(1)", 0), Some(calls.clone())),
    ];
    assert_eq!(sorted(sites), sorted(expected));
}

#[test]
fn a_call_of_an_imported_function_is_an_overload_call_site() {
    let source = "import { m } from \"./b\";\nfunction k() { m(); }\nm();\n";
    let graph = with_b(source);
    let pending = graph.id("pending_symbol:src/b.ts#m");
    let calls = edge_id(&graph.id("k"), Calls, &pending, None);
    let sites: Vec<_> = graph
        .sites(OverloadCall)
        .into_iter()
        .map(|site| (graph.label(&site.from_id), site.position, site.replaces.clone()))
        .collect();
    assert_eq!(sites, vec![("k".to_string(), at(source, "m();", 0), Some(calls))]);
}

// --- receiver calls ------------------------------------------------------------

#[test]
fn a_call_through_an_unknown_receiver_is_a_receiver_call_site() {
    let source = "class X { static m() {} }\n\
                  const a = { b: { m() {} } };\n\
                  function f() { return a; }\n\
                  function g(X) { a.b.m(); f().n(); unknown.u(); X.m(); }\n\
                  function h() { new Unknown(); new X(); }\n\
                  X.m();\n\
                  unknown.top();\n\
                  const z = unknown.w();\n";
    let graph = extract(PATH, source);
    let sites: Vec<_> = graph
        .sites(ReceiverCall)
        .into_iter()
        .map(|site| {
            (
                graph.label(&site.from_id),
                site.name.clone(),
                site.edge_kind,
                site.position,
                site.replaces.clone(),
            )
        })
        .collect();
    let site = |from: &str, name: &str, needle: &str, offset: u32| {
        (from.to_string(), name.to_string(), Calls, at(source, needle, offset), None)
    };
    let expected = vec![
        site("g", "m", "a.b.m()", 4),
        site("g", "n", "f().n()", 4),
        site("g", "u", "unknown.u", 8),
        site("g", "m", "X.m(); }", 2),
        site("<file>", "top", "unknown.top", 8),
        site("z", "w", "unknown.w", 8),
    ];
    assert_eq!(sorted(sites), sorted(expected));
    assert_edge!(graph, References, "<file>", "X.m");
    assert_no_edge!(graph, Calls, "g", "X.m");
}

#[test]
fn receiver_calls_are_folded_into_their_callers_untyped_calls() {
    let graph = extract(PATH, "function g(x) { x.m(); }\nunknown.top();\n");
    assert_eq!(graph.sites(ReceiverCall).len(), 2, "{:?}", graph.site_summary(ReceiverCall));
    let untyped: Vec<(&str, Vec<String>)> = graph
        .graph
        .nodes
        .iter()
        .filter(|node| !node.untyped_calls.is_empty())
        .map(|node| (node.name.as_str(), node.untyped_calls.clone()))
        .collect();
    let file_name = PATH.rsplit('/').next().unwrap_or(PATH);
    assert_eq!(untyped, vec![(file_name, vec!["top".to_string()]), ("g", vec!["m".to_string()])]);
}

// --- NUL -------------------------------------------------------------------------

#[test]
fn a_nul_keeps_names_and_the_edges_written_after_it() {
    let graph =
        extract(PATH, "import \"./x\0y\";\nconst a = \"\0\";\nfunction f() {}\nfunction g() { f(); }\n");
    assert_edge!(graph, EdgeKind::Imports, "<file>", "external_module:./x\0y");
    assert_edge!(graph, Calls, "g", "f");
}
