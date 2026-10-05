//! The declaration pass, driven through the extractor: source text in, nodes
//! and edges out.

use g_mesh_plugin_sdk::ids::{edge_id, node_id};
use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, PathSegment, Visibility, WireNode};
use g_mesh_plugin_sdk::{Extractor, FileGraph, RelPath};
use g_mesh_plugin_typescript::extractor::keys::is_placeholder_kind;
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project::TsProject;

/// One file's graph, with the questions the tests ask of it.
struct Graph {
    path: String,
    graph: FileGraph,
}

fn extract(path: &str, source: &str) -> Graph {
    let graph = TypeScriptExtractor.extract(&TsProject::default(), &RelPath::new(path), source);
    Graph { path: path.to_string(), graph }
}

impl Graph {
    fn qualified_names(&self) -> Vec<&str> {
        self.graph.nodes.iter().map(|node| node.qualified_name.as_str()).collect()
    }

    fn all(&self, qualified_name: &str) -> Vec<&WireNode> {
        self.graph.nodes.iter().filter(|node| node.qualified_name == qualified_name).collect()
    }

    fn node(&self, qualified_name: &str) -> &WireNode {
        match self.all(qualified_name).as_slice() {
            [node] => node,
            other => panic!("{} nodes {qualified_name:?} in {:?}", other.len(), self.qualified_names()),
        }
    }

    fn file_id(&self) -> &str {
        let file = &self.graph.nodes[0];
        assert_eq!(file.kind, NodeKind::File);
        &file.id
    }

    fn has_edge(&self, from: &str, kind: EdgeKind, to: &str) -> bool {
        self.graph.edges.iter().any(|edge| edge.from_id == from && edge.kind == kind && edge.to_id == to)
    }

    fn exports(&self, qualified_name: &str) -> bool {
        self.has_edge(self.file_id(), EdgeKind::Exports, &self.node(qualified_name).id)
    }

    /// Every declared node: neither the `File` node nor a placeholder.
    fn symbols(&self) -> Vec<&str> {
        self.graph
            .nodes
            .iter()
            .skip(1)
            .filter(|node| !is_placeholder_kind(node.native_kind.as_deref()))
            .map(|node| node.qualified_name.as_str())
            .collect()
    }

    fn assert_parsed_cleanly(&self) {
        assert!(
            self.graph.nodes.iter().all(|node| !node.has_syntax_errors),
            "{} parsed with syntax errors",
            self.path
        );
    }
}

fn seg(sep: Option<&str>, name: &str) -> PathSegment {
    PathSegment { sep: sep.map(str::to_string), name: name.to_string() }
}

// --- exports ---------------------------------------------------------------

#[test]
fn export_clause_after_the_fact_makes_the_name_public() {
    let graph = extract("src/a.ts", "const a = 1;\nconst b = 2;\nexport { a };\n");
    assert_eq!(graph.node("a").visibility, Visibility::Public);
    assert!(graph.exports("a"));
    assert_eq!(graph.node("b").visibility, Visibility::File);
    assert!(!graph.exports("b"));
}

#[test]
fn export_default_identifier_makes_the_name_public() {
    let graph = extract("src/a.ts", "function a() {}\nfunction b() {}\nexport default a;\n");
    assert_eq!(graph.node("a").visibility, Visibility::Public);
    assert!(graph.exports("a"));
    assert!(!graph.exports("b"));
}

#[test]
fn export_equals_makes_the_name_public() {
    let graph = extract("src/a.ts", "class a {}\nclass b {}\nexport = a;\n");
    assert_eq!(graph.node("a").visibility, Visibility::Public);
    assert!(graph.exports("a"));
    assert!(!graph.exports("b"));
}

#[test]
fn export_declaration_is_public_with_defines_and_exports() {
    let graph = extract("src/a.ts", "export function f() {}\nfunction g() {}\n");
    let f = graph.node("f");
    assert_eq!(f.visibility, Visibility::Public);
    assert!(graph.has_edge(graph.file_id(), EdgeKind::Defines, &f.id));
    assert!(graph.exports("f"));
    let g = graph.node("g");
    assert!(graph.has_edge(graph.file_id(), EdgeKind::Defines, &g.id));
    assert!(!graph.exports("g"));
}

#[test]
fn reexport_clause_with_from_publishes_no_local_name() {
    let graph = extract("src/a.ts", "const a = 1;\nexport { a } from \"./b\";\n");
    assert_eq!(graph.node("a").visibility, Visibility::File);
    assert!(!graph.exports("a"));
}

// --- members ---------------------------------------------------------------

const CLASS: &str = "\
class C {
  m() {}
  static s() {}
  get v() { return 1; }
  set v(x) {}
  #priv() {}
  fire = () => 1;
  static make = function () { return new C(); };
  data = 1;
}
";

#[test]
fn class_members_take_hash_for_instance_and_dot_for_static() {
    let graph = extract("src/a.ts", CLASS);
    for (qualified_name, native_kind) in [
        ("C#m", "method"),
        ("C.s", "method"),
        ("C##priv", "method"),
        ("C#fire", "arrow_function"),
        ("C.make", "function_expression"),
    ] {
        let node = graph.node(qualified_name);
        assert_eq!(node.kind, NodeKind::Function, "{qualified_name}");
        assert_eq!(node.native_kind.as_deref(), Some(native_kind), "{qualified_name}");
    }
    assert_eq!(graph.node("C##priv").name, "#priv");
    assert!(graph.all("C#data").is_empty(), "a data field is not a node");
}

#[test]
fn getter_and_setter_of_one_name_are_two_nodes() {
    let graph = extract("src/a.ts", CLASS);
    let mut kinds: Vec<&str> =
        graph.all("C#v").iter().map(|node| node.native_kind.as_deref().unwrap()).collect();
    kinds.sort();
    assert_eq!(kinds, vec!["getter", "setter"]);
    let ids: Vec<&str> = graph.all("C#v").iter().map(|node| node.id.as_str()).collect();
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn class_member_qualified_path_has_its_separator() {
    let graph = extract("src/a.ts", CLASS);
    let path = graph.node("C#m").qualified_path.clone().expect("a sendable path").0;
    assert_eq!(path, vec![seg(None, "C"), seg(Some("#"), "m")]);
}

#[test]
fn interface_methods_are_members() {
    let graph = extract("src/a.ts", "interface I {\n  m(): void;\n  p: number;\n}\n");
    let m = graph.node("I#m");
    assert_eq!(m.kind, NodeKind::Function);
    assert_eq!(m.native_kind.as_deref(), Some("method"));
    assert_eq!(graph.node("I").native_kind.as_deref(), Some("interface"));
    assert!(graph.all("I#p").is_empty());
}

#[test]
fn object_literal_methods_are_not_members() {
    let graph = extract("src/a.ts", "const o = {\n  m() {},\n  f: () => 1,\n};\n");
    assert_eq!(graph.symbols(), vec!["o"]);
    assert_eq!(graph.node("o").kind, NodeKind::Variable);
}

// --- variables -------------------------------------------------------------

#[test]
fn function_valued_bindings_are_functions_and_others_variables() {
    let source = "\
const f = () => 1;
const g = function () {};
const h = function* () {};
let x = 1;
var y;
const z = 'z';
";
    let graph = extract("src/a.ts", source);
    for (qualified_name, kind, native_kind) in [
        ("f", NodeKind::Function, "arrow_function"),
        ("g", NodeKind::Function, "function_expression"),
        ("h", NodeKind::Function, "generator_function"),
        ("x", NodeKind::Variable, "let"),
        ("y", NodeKind::Variable, "var"),
        ("z", NodeKind::Variable, "const"),
    ] {
        let node = graph.node(qualified_name);
        assert_eq!((node.kind, node.native_kind.as_deref()), (kind, Some(native_kind)), "{qualified_name}");
    }
}

#[test]
fn destructuring_declares_nothing() {
    let graph =
        extract("src/a.ts", "const { a, b: c } = obj;\nconst [d, ...e] = arr;\nlet { f = () => 1 } = o;\n");
    assert_eq!(graph.symbols(), Vec::<&str>::new());
}

// --- classes and namespaces ------------------------------------------------

#[test]
fn anonymous_default_class_is_named_default() {
    let graph = extract("src/a.ts", "export default class {\n  m() {}\n}\n");
    let class = graph.node("default");
    assert_eq!((class.kind, class.name.as_str()), (NodeKind::Type, "default"));
    assert_eq!(class.native_kind.as_deref(), Some("class"));
    assert_eq!(class.visibility, Visibility::Public);
    assert!(graph.exports("default"));
    graph.node("default#m");
}

#[test]
fn declare_module_string_is_an_ambient_module_named_by_its_value() {
    let graph = extract("types/x.d.ts", "declare module \"x\" {\n  export function f(): void;\n}\n");
    let module = graph.node("x");
    assert_eq!(module.kind, NodeKind::Module);
    assert_eq!(module.name, "x");
    assert_eq!(module.native_kind.as_deref(), Some("ambient_module"));
    assert_eq!(graph.node("x.f").kind, NodeKind::Function);
}

#[test]
fn dotted_namespace_name_is_one_segment() {
    let graph = extract("src/a.ts", "namespace Outer.Inner {\n  export function deepFn() {}\n}\n");
    let namespace = graph.node("Outer.Inner");
    assert_eq!(namespace.kind, NodeKind::Module);
    assert_eq!(namespace.name, "Outer.Inner");
    assert_eq!(namespace.native_kind.as_deref(), Some("namespace"));
    assert_eq!(namespace.qualified_path.clone().expect("a sendable path").0, vec![seg(None, "Outer.Inner")]);
    let deep = graph.node("Outer.Inner.deepFn");
    assert_eq!(
        deep.qualified_path.clone().expect("a sendable path").0,
        vec![seg(None, "Outer.Inner"), seg(Some("."), "deepFn")]
    );
}

// --- function bodies ----------------------------------------------------------

#[test]
fn nothing_inside_a_function_body_is_a_node() {
    let source = "\
function outer() {
  const inner = () => 1;
  let v = 1;
  function nested() {}
  class Local { m() {} }
  interface LI { n(): void }
  type LT = number;
  enum LE { A }
  namespace LN { export const q = 1; }
}
const arrow = () => {
  const deep = function () {};
  class Hidden {}
};
class K {
  m() { const local = 1; function helper() {} }
}
";
    let graph = extract("src/a.ts", source);
    assert_eq!(graph.symbols(), vec!["outer", "arrow", "K", "K#m"]);
}

// --- syntax: doc comments and signatures ----------------------------------------

#[test]
fn doc_comment_is_only_a_jsdoc_block_with_its_gutter_stripped() {
    let source = "\
/**
 * First line.
 *   Indented second.
 */
function documented() {}
// a line comment
function lineCommented() {}
/* a plain block */
function blockCommented() {}
/** one-liner */
class C {
  /** member doc */
  m() {}
}
";
    let graph = extract("src/a.ts", source);
    assert_eq!(graph.node("documented").doc_comment.as_deref(), Some("First line.\nIndented second."));
    assert_eq!(graph.node("lineCommented").doc_comment, None);
    assert_eq!(graph.node("blockCommented").doc_comment, None);
    assert_eq!(graph.node("C").doc_comment.as_deref(), Some("one-liner"));
    assert_eq!(graph.node("C#m").doc_comment.as_deref(), Some("member doc"));
}

#[test]
fn signature_collapses_whitespace() {
    let source = "function f<T>(\n    a: T,\n    b:   string\n)  :   Promise<\n  void\n> {}\n";
    let graph = extract("src/a.ts", source);
    assert_eq!(graph.node("f").signature.as_deref(), Some("f<T>( a: T, b: string ): Promise< void >"));
}

#[test]
fn signature_wraps_an_unparenthesized_arrow_parameter() {
    let graph = extract("src/a.js", "const g = x => x;\nconst h = async y => y;\n");
    assert_eq!(graph.node("g").signature.as_deref(), Some("g(x)"));
    assert_eq!(graph.node("h").signature.as_deref(), Some("async h(y)"));
}

// --- emit ------------------------------------------------------------------------

#[test]
fn syntax_error_marks_every_node() {
    let graph = extract("src/a.ts", "export function f() {}\nclass C { m() {} }\nfunction broken( {\n");
    assert!(graph.graph.nodes.len() >= 3, "{:?}", graph.qualified_names());
    assert!(graph.graph.nodes.iter().all(|node| node.has_syntax_errors), "{:#?}", graph.graph.nodes);
}

#[test]
fn clean_parse_marks_no_node() {
    extract("src/a.ts", "export function f() {}\nclass C { m() {} }\n").assert_parsed_cleanly();
}

#[test]
fn ids_equal_the_sdk_id_functions() {
    let graph = extract("src/a.ts", CLASS);
    assert!(graph.graph.nodes.len() > 5);
    for node in &graph.graph.nodes {
        let expected = node_id(&graph.path, node.kind, &node.qualified_name, node.native_kind.as_deref());
        assert_eq!(node.id, expected, "{}", node.qualified_name);
        assert_eq!(node.file_path, graph.path);
        assert_eq!(node.language, "typescript");
    }
    for edge in &graph.graph.edges {
        assert_eq!(edge.id, edge_id(&edge.from_id, edge.kind, &edge.to_id, None));
        assert_eq!(edge.to_declaration, None);
    }
}

// --- overloads (ADR 0024) ------------------------------------------------------------

#[test]
fn function_overloads_are_one_node_with_a_declaration_list() {
    let source = "\
/** Parses. */
export function parse(a: string): number;
export function parse(a: number): number;
export function parse(a: any): number {
  return 0;
}
";
    let graph = extract("src/overloads.ts", source);
    let parse = graph.node("parse");
    let declarations = parse.declarations.clone().expect("three declarations");
    let shape: Vec<(u32, u32, bool)> =
        declarations.iter().map(|d| (d.ordinal, d.start_line, d.has_body)).collect();
    assert_eq!(shape, vec![(0, 1, false), (1, 2, false), (2, 3, true)]);
    assert_eq!((parse.range.start.line, parse.range.end.line), (3, 5), "the implementation's range");
    assert_eq!(parse.signature.as_deref(), Some("parse(a: string): number"));
    assert_eq!(parse.doc_comment.as_deref(), Some("Parses."));
    assert_eq!(parse.visibility, Visibility::Public);
}

#[test]
fn ambient_overloads_in_a_declaration_file_are_one_node() {
    let source = "declare function g(a: string): void;\ndeclare function g(a: number): void;\n";
    let graph = extract("types/g.d.ts", source);
    let g = graph.node("g");
    let declarations = g.declarations.clone().expect("two declarations");
    let shape: Vec<(u32, u32, bool)> =
        declarations.iter().map(|d| (d.ordinal, d.start_line, d.has_body)).collect();
    assert_eq!(shape, vec![(0, 0, false), (1, 1, false)]);
    assert_eq!(g.range.start.line, 0, "with no body, the first declaration's range");
}

#[test]
fn method_overloads_are_one_node() {
    let source = "class C {\n  m(a: string): void;\n  m(a: number): void;\n  m(a: any) {}\n}\n";
    let graph = extract("src/a.ts", source);
    let m = graph.node("C#m");
    assert_eq!(m.declarations.as_ref().map(Vec::len), Some(3));
    assert_eq!(m.range.start.line, 3);
}

#[test]
fn a_single_declaration_carries_no_list() {
    let graph = extract("src/a.ts", "function f(a: string) {}\n");
    assert_eq!(graph.node("f").declarations, None);
}

// --- grammars --------------------------------------------------------------

const JSX: &str = "\
import React from 'react';
export const App = () => <div className=\"x\">{1 < 2 ? 'a' : 'b'}</div>;
export function Comp(props) {
  return <App {...props} />;
}
";

#[test]
fn javascript_grammar_parses_jsx_in_js_and_jsx_files() {
    for path in ["src/app.js", "src/app.jsx", "src/app.mjs"] {
        let graph = extract(path, JSX);
        graph.assert_parsed_cleanly();
        assert_eq!(graph.symbols(), vec!["App", "Comp"], "{path}");
    }
}

#[test]
fn tsx_grammar_parses_a_component() {
    let source = "\
interface Props { label: string }
export const Button = <T,>(props: Props & { extra?: T }) => <button>{props.label}</button>;
export default function Page(): JSX.Element {
  return <Button label=\"ok\" />;
}
";
    let graph = extract("src/page.tsx", source);
    graph.assert_parsed_cleanly();
    assert_eq!(graph.symbols(), vec!["Props", "Button", "Page"]);
}

#[test]
fn typescript_grammar_parses_angle_bracket_assertions() {
    let source = "\
export const n = <number>value;
export function id<T>(x: T): T { return x; }
export abstract class Base<T> implements I { abstract run(): T; }
export enum E { A, B }
export type Alias = { k: string };
";
    let graph = extract("src/a.mts", source);
    graph.assert_parsed_cleanly();
    assert_eq!(graph.symbols(), vec!["n", "id", "Base", "Base#run", "E", "Alias"]);
    assert_eq!(graph.node("Base").native_kind.as_deref(), Some("abstract_class"));
    assert_eq!(graph.node("Base#run").native_kind.as_deref(), Some("abstract_method"));
}

#[test]
fn unknown_extension_yields_the_file_node_alone() {
    let graph = extract("src/a.py", "function f() {}\n");
    assert_eq!(graph.qualified_names(), vec!["src/a.py"]);
    assert!(graph.graph.edges.is_empty());
}

/// A raw NUL inside a regex literal makes the grammar drop the declaration
/// that follows it; tsc accepts the file, so the declaration must survive.
#[test]
fn nul_in_the_source_does_not_end_the_parse() {
    let graph = extract("src/a.ts", "const a = \"\0\";\nconst r = /\0/;\nfunction f() {}\n");
    assert_eq!(graph.symbols(), vec!["a", "r", "f"]);
}
