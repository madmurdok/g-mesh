//! The open sites the semantic tier answers beyond the namespace and overload
//! ones: `Reference` sites with `replaces` on every edge onto an import's
//! `pending_symbol` placeholder (a hop through a re-export or a `default`),
//! and `ReceiverCall` sites for a `this.m()` / `super.m()` that binds no
//! member declared in the calling file.

mod common;

use common::{extract_in, position, Graph};
use g_mesh_plugin_sdk::ids::edge_id;
use g_mesh_plugin_sdk::wire::EdgeKind::{self, Calls, References, SupertypeOf};
use g_mesh_plugin_sdk::wire::Position;
use g_mesh_plugin_sdk::OpenSiteKind::{OverloadCall, ReceiverCall, Reference};

const PATH: &str = "src/a.ts";

/// The modules the imports in these tests reach: a default function, a
/// default interface, a barrel re-exporting a named function, and a base
/// class.
const MODULES: [(&str, &str); 5] = [
    ("src/m.ts", "export default function X() {}\n"),
    ("src/t.ts", "export default interface T {}\n"),
    ("src/idx.ts", "export { n } from \"./n\";\n"),
    ("src/n.ts", "export function n() {}\n"),
    ("src/base.ts", "export class Base { m() {} }\n"),
];

fn graph_of(source: &str) -> Graph {
    let mut files = vec![(PATH, source)];
    files.extend(MODULES);
    extract_in(&files, PATH)
}

/// `needle`'s first occurrence, moved right by `offset` characters.
fn at(source: &str, needle: &str, offset: u32) -> Position {
    let start = position(source, needle, 0);
    Position { line: start.line, col: start.col + offset }
}

fn sorted<T: std::fmt::Debug>(mut items: Vec<T>) -> Vec<T> {
    items.sort_by_key(|item| format!("{item:?}"));
    items
}

/// A hop site as `(from, name, edge kind, position, replaces)`.
type Hop = (String, String, EdgeKind, Position, Option<String>);

/// Every `Reference` site that names an edge in `replaces`.
fn hops(graph: &Graph) -> Vec<Hop> {
    graph
        .sites(Reference)
        .into_iter()
        .filter(|site| site.replaces.is_some())
        .map(|site| {
            (
                graph.label(&site.from_id),
                site.name.clone(),
                site.edge_kind,
                site.position,
                site.replaces.clone(),
            )
        })
        .collect()
}

/// The hop site expected for the edge `from -kind-> to` (labels), written at
/// `position`.
fn hop(graph: &Graph, from: &str, name: &str, kind: EdgeKind, to: &str, position: Position) -> Hop {
    let edge = edge_id(&graph.id(from), kind, &graph.id(to), None);
    assert!(
        graph.graph.edges.iter().any(|e| e.id == edge),
        "no {kind:?} {from} -> {to}: {:?}",
        graph.pairs(kind)
    );
    (from.to_string(), name.to_string(), kind, position, Some(edge))
}

const DEFAULT_X: &str = "pending_symbol:src/m.ts#default";
const DEFAULT_T: &str = "pending_symbol:src/t.ts#default";

// --- hop sites ---------------------------------------------------------------

/// A call of a default import: a hop site on its `CALLS` edge, at the called
/// name, beside the `OverloadCall` site the same call already has.
#[test]
fn a_call_of_a_default_import_is_a_hop_site_beside_its_overload_call_site() {
    let source = "import X from \"./m\";\nfunction f() { X(); }\n";
    let graph = graph_of(source);
    let expected = hop(&graph, "f", "X", Calls, DEFAULT_X, at(source, "X();", 0));
    assert_eq!(hops(&graph), vec![expected.clone()]);

    let overloads: Vec<_> =
        graph.sites(OverloadCall).into_iter().map(|site| (site.position, site.replaces.clone())).collect();
    assert_eq!(overloads, vec![(expected.3, expected.4)], "the same call, the same edge");
}

/// A use that is not a call from a function - a top-level call, a read, a
/// type annotation, a top-level `new` - is a hop site on its `REFERENCES`
/// edge, from the enclosing symbol.
#[test]
fn a_use_outside_a_function_call_is_a_hop_site_on_its_references_edge() {
    let source = "import X from \"./m\";\n\
                  import T from \"./t\";\n\
                  import { Base } from \"./base\";\n\
                  X();\n\
                  const y = X;\n\
                  let v: T;\n\
                  new Base();\n";
    let graph = graph_of(source);
    let expected = vec![
        hop(&graph, "<file>", "X", References, DEFAULT_X, at(source, "X();", 0)),
        hop(&graph, "y", "X", References, DEFAULT_X, at(source, "= X", 2)),
        hop(&graph, "v", "T", References, DEFAULT_T, at(source, ": T", 2)),
        hop(&graph, "<file>", "Base", References, "pending_symbol:src/base.ts#Base", at(source, "Base()", 0)),
    ];
    assert_eq!(sorted(hops(&graph)), sorted(expected));
}

/// A named import through a barrel waits on the barrel's name, and gets the
/// same hop site.
#[test]
fn a_named_import_through_a_barrel_is_a_hop_site() {
    let source = "import { n } from \"./idx\";\nfunction f() { n(); }\n";
    let graph = graph_of(source);
    assert_eq!(
        hops(&graph),
        vec![hop(&graph, "f", "n", Calls, "pending_symbol:src/idx.ts#n", at(source, "n()", 0))]
    );
}

/// One hop site per edge, at its first use; every call stays an
/// `OverloadCall` site of its own.
#[test]
fn an_edge_used_twice_has_one_hop_site_at_its_first_use() {
    let source = "import X from \"./m\";\nfunction f() { X(1); X(2); }\n";
    let graph = graph_of(source);
    assert_eq!(hops(&graph), vec![hop(&graph, "f", "X", Calls, DEFAULT_X, at(source, "X(1)", 0))]);
    assert_eq!(graph.sites(OverloadCall).len(), 2, "{:?}", graph.site_summary(OverloadCall));
}

/// An edge onto a declaration of this file - a function, a method through
/// `this` or `super` - is no hop: there is nothing to follow.
#[test]
fn an_edge_onto_a_local_declaration_has_no_hop_site() {
    let source = "function g() {}\n\
                  function f() { g(); }\n\
                  const r = g;\n\
                  class P { m() {} a() { this.m(); } }\n\
                  class Q extends P { b() { super.m(); } }\n";
    let graph = graph_of(source);
    assert_edge!(graph, Calls, "f", "g");
    assert_edge!(graph, References, "r", "g");
    assert_edge!(graph, SupertypeOf, "Q", "P");
    assert_eq!(hops(&graph), vec![]);
}

/// `X.m()` and `new X()` on an import: the hop site is at the receiver `X`,
/// not at the member, on the `REFERENCES` edge from the enclosing function.
#[test]
fn a_qualified_call_or_new_on_an_import_is_a_hop_site_at_the_receiver() {
    let source = "import X from \"./m\";\n\
                  import { Base } from \"./base\";\n\
                  function f() { return X.m(); }\n\
                  function g() { return new Base(); }\n";
    let graph = graph_of(source);
    let expected = vec![
        hop(&graph, "f", "X", References, DEFAULT_X, at(source, "X.m()", 0)),
        hop(&graph, "g", "Base", References, "pending_symbol:src/base.ts#Base", at(source, "Base()", 0)),
    ];
    assert_eq!(sorted(hops(&graph)), sorted(expected));
}

/// Every heritage name onto an import - `extends`, `implements`, an
/// interface's `extends`, and the name of a generic `extends Box<T>` - is a
/// hop site on its `SUPERTYPE_OF` edge, at that name.
#[test]
fn a_heritage_name_onto_an_import_is_a_hop_site_on_its_supertype_edge() {
    let source = "import X from \"./m\";\n\
                  import T from \"./t\";\n\
                  class Sub extends X {}\n\
                  class Impl implements T {}\n\
                  interface I extends T {}\n\
                  class G extends X<number> {}\n";
    let graph = graph_of(source);
    let expected = vec![
        hop(&graph, "Sub", "X", SupertypeOf, DEFAULT_X, at(source, "X {}", 0)),
        hop(&graph, "Impl", "T", SupertypeOf, DEFAULT_T, at(source, "T {}", 0)),
        hop(&graph, "I", "T", SupertypeOf, DEFAULT_T, at(source, "T {}\nclass G", 0)),
        hop(&graph, "G", "X", SupertypeOf, DEFAULT_X, at(source, "X<number>", 0)),
    ];
    assert_eq!(sorted(hops(&graph)), sorted(expected));
}

// --- this / super ------------------------------------------------------------

/// `this.m()` and `super.m()` whose member no type of this file declares (the
/// base class is imported) are receiver-call sites at `m`, from the calling
/// method, with no edge to replace. `super()` names no member and is none.
#[test]
fn an_inherited_this_or_super_call_is_a_receiver_call_site() {
    let source = "import { Base } from \"./base\";\n\
                  class C extends Base {\n\
                  \x20 constructor() { super(); }\n\
                  \x20 a() { return this.m(); }\n\
                  \x20 b() { return super.m(); }\n\
                  }\n";
    let graph = graph_of(source);
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
    let expected = vec![
        ("C#a".to_string(), "m".to_string(), Calls, at(source, "this.m", 5), None),
        ("C#b".to_string(), "m".to_string(), Calls, at(source, "super.m", 6), None),
    ];
    assert_eq!(sorted(sites), sorted(expected));
    let untyped: Vec<_> = graph
        .graph
        .nodes
        .iter()
        .filter(|node| !node.untyped_calls.is_empty())
        .map(|node| (node.qualified_name.clone(), node.untyped_calls.clone()))
        .collect();
    assert_eq!(
        sorted(untyped),
        vec![("C#a".to_string(), vec!["m".to_string()]), ("C#b".to_string(), vec!["m".to_string()])]
    );
}

/// `this.m()` / `super.m()` that bind a member declared here are `CALLS`
/// edges, and no site.
#[test]
fn a_this_or_super_call_of_a_member_declared_here_is_an_edge_and_no_site() {
    let source = "class P { m() {} a() { this.m(); } }\nclass Q extends P { b() { super.m(); } }\n";
    let graph = graph_of(source);
    assert_edge!(graph, Calls, "P#a", "P#m");
    assert_edge!(graph, Calls, "Q#b", "P#m");
    assert!(graph.sites(ReceiverCall).is_empty(), "{:?}", graph.site_summary(ReceiverCall));
}

/// A constructor's `super(...)` with the base imported is no receiver call.
#[test]
fn a_super_constructor_call_is_not_a_receiver_call_site() {
    let source = "import { Base } from \"./base\";\nclass C extends Base { constructor() { super(); } }\n";
    let graph = graph_of(source);
    assert!(graph.sites(ReceiverCall).is_empty(), "{:?}", graph.site_summary(ReceiverCall));
}
