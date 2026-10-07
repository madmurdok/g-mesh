//! Imports, re-exports and computed specifiers, driven through the walk with
//! an injected resolver: source text in, placeholders and `IMPORTS` edges
//! out.

use g_mesh_plugin_sdk::ids::node_id;
use g_mesh_plugin_sdk::wire::{
    EdgeKind, NodeKind, PlaceholderTarget, Position, Range, TargetKey, TargetScope, WireNode,
};
use g_mesh_plugin_sdk::{CharColumns, FileGraph, RelPath};
use g_mesh_plugin_typescript::extractor::decls::Declarer;
use g_mesh_plugin_typescript::extractor::model::FileModel;
use g_mesh_plugin_typescript::extractor::{emit, grammar};

/// The file every test extracts, unless it needs a JavaScript one.
const PATH: &str = "src/a.ts";

/// One file's graph, with the questions the tests ask of it.
struct Graph {
    graph: FileGraph,
}

/// Extracts `source` as `path`, resolving each specifier listed in
/// `resolves` to its project file and every other one to nothing.
fn extract(path: &str, source: &str, resolves: &[(&str, &str)]) -> Graph {
    let rel = RelPath::new(path);
    let resolver = |specifier: &str, _from: &RelPath| {
        resolves.iter().find(|(listed, _)| *listed == specifier).map(|(_, file)| RelPath::new(file))
    };
    let columns = CharColumns::new(source);
    let grammar = grammar::grammar_for(&rel).expect("a TypeScript or JavaScript path");
    let tree = grammar::parse(grammar, source).expect("a parse");
    let root = tree.root_node();
    let (start, end) = (root.start_position(), root.end_position());
    let mut model = FileModel::new(path, columns.range((start.row, start.column), (end.row, end.column)));
    Declarer::new(source, &columns, &rel, &resolver, &mut model).run(root);
    let graph = emit::flush(model, "typescript", "tree-sitter", &rel, root.has_error());
    Graph { graph }
}

fn target(file: &str, name: &str) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: TargetScope::File(file.to_string()),
        key: TargetKey::Name(name.to_string()),
        from_container: None,
        key_path: None,
    }
}

fn range(line: u32, start_col: u32, end_col: u32) -> Range {
    Range { start: Position { line, col: start_col }, end: Position { line, col: end_col } }
}

impl Graph {
    fn file_id(&self) -> &str {
        let file = &self.graph.nodes[0];
        assert_eq!(file.kind, NodeKind::File);
        &file.id
    }

    fn of_kind(&self, native_kind: &str) -> Vec<&WireNode> {
        self.graph.nodes.iter().filter(|node| node.native_kind.as_deref() == Some(native_kind)).collect()
    }

    /// The one node of `native_kind` named `qualified_name`.
    fn placeholder(&self, native_kind: &str, qualified_name: &str) -> &WireNode {
        let found: Vec<_> = self
            .of_kind(native_kind)
            .into_iter()
            .filter(|node| node.qualified_name == qualified_name)
            .collect();
        match found.as_slice() {
            [node] => node,
            other => {
                panic!("{} {native_kind} nodes {qualified_name:?} in {:#?}", other.len(), self.graph.nodes)
            }
        }
    }

    fn imports_edges(&self) -> Vec<&g_mesh_plugin_sdk::wire::WireEdge> {
        self.graph.edges.iter().filter(|edge| edge.kind == EdgeKind::Imports).collect()
    }

    /// The names (specifiers) of the modules the file has an `IMPORTS` edge
    /// to, sorted.
    fn imported(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .imports_edges()
            .into_iter()
            .map(|edge| {
                assert_eq!(edge.from_id, self.file_id());
                let node = self.graph.nodes.iter().find(|node| node.id == edge.to_id).expect("edge target");
                node.name.as_str()
            })
            .collect();
        names.sort_unstable();
        names
    }

    fn has_edge_to(&self, id: &str) -> bool {
        self.graph.edges.iter().any(|edge| edge.to_id == id)
    }
}

// --- static imports ----------------------------------------------------------

#[test]
fn unresolved_import_is_an_external_module_named_by_the_specifier() {
    let graph = extract(PATH, "import { x } from \"lodash\";\n", &[]);
    let module = graph.placeholder("external_module", "lodash");
    assert_eq!(module.kind, NodeKind::Module);
    assert_eq!(module.name, "lodash");
    assert_eq!(module.target, None);
    assert_eq!(module.id, node_id(PATH, NodeKind::Module, "lodash", Some("external_module")));
    assert_eq!(module.range, range(0, 18, 26));
    let edges = graph.imports_edges();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].from_id, graph.file_id());
    assert_eq!(edges[0].to_id, module.id);
    assert!(!edges[0].resolved);
}

#[test]
fn resolved_import_is_a_resolved_module_addressed_by_the_bare_path() {
    let graph = extract(PATH, "import { x } from \"./b\";\n", &[("./b", "src/b.ts")]);
    assert!(graph.of_kind("external_module").is_empty());
    let module = graph.placeholder("resolved_module", "src/b.ts");
    assert_eq!(module.kind, NodeKind::Module);
    assert_eq!(module.name, "./b");
    assert_eq!(module.target, Some(target("src/b.ts", "*")));
    assert_eq!(module.id, node_id(PATH, NodeKind::Module, "src/b.ts", Some("resolved_module")));
    assert_eq!(graph.imported(), ["./b"]);
    // Whether the edge lands on an indexed file is core's call, not the
    // resolver's.
    assert!(!graph.imports_edges()[0].resolved);
}

#[test]
fn specifiers_resolving_to_one_file_share_one_node() {
    let graph = extract(
        PATH,
        "import \"./b\";\nimport { y } from \"./b.ts\";\n",
        &[("./b", "src/b.ts"), ("./b.ts", "src/b.ts")],
    );
    let modules = graph.of_kind("resolved_module");
    assert_eq!(modules.len(), 1, "{modules:#?}");
    assert_eq!(modules[0].name, "./b");
    assert_eq!(modules[0].range.start.line, 0);
    assert_eq!(graph.imports_edges().len(), 1);
}

#[test]
fn escaped_specifier_in_a_static_import_imports_nothing() {
    let graph = extract(
        PATH,
        "import x from \"./a\\u0041\";\nexport * from \"./b\\x41\";\nexport { y } from \"./c\\n\";\n",
        &[],
    );
    assert_eq!(graph.graph.nodes.len(), 1, "{:#?}", graph.graph.nodes);
    assert!(graph.graph.edges.is_empty());
}

// --- re-exports --------------------------------------------------------------

#[test]
fn named_reexport_is_a_reexport_placeholder_with_no_edge() {
    let graph = extract(PATH, "export { a as b, c } from \"./y\";\n", &[("./y", "src/y.ts")]);
    let aliased = graph.placeholder("reexport", "src/y.ts#a");
    assert_eq!(aliased.kind, NodeKind::Module);
    assert_eq!(aliased.name, "b");
    assert_eq!(aliased.target, Some(target("src/y.ts", "a")));
    assert!(!graph.has_edge_to(&aliased.id));
    let plain = graph.placeholder("reexport", "src/y.ts#c");
    assert_eq!(plain.name, "c");
    assert!(!graph.has_edge_to(&plain.id));
    // The module itself is imported.
    assert_eq!(graph.imported(), ["./y"]);
    assert_eq!(graph.of_kind("resolved_module").len(), 1);
}

#[test]
fn reexport_of_an_unresolved_specifier_records_no_placeholder() {
    let graph = extract(PATH, "export { a as b } from \"pkg\";\nexport * from \"other\";\n", &[]);
    assert!(graph.of_kind("reexport").is_empty());
    assert_eq!(graph.imported(), ["other", "pkg"]);
}

#[test]
fn whole_module_reexport_is_named_star() {
    let graph = extract(PATH, "export * from \"./y\";\n", &[("./y", "src/y.ts")]);
    let all = graph.placeholder("reexport", "src/y.ts#*");
    assert_eq!(all.name, "*");
    assert_eq!(all.target, Some(target("src/y.ts", "*")));
    assert!(!graph.has_edge_to(&all.id));
    assert_eq!(graph.imported(), ["./y"]);
}

#[test]
fn namespace_reexport_imports_the_module_and_reexports_nothing() {
    let graph = extract(PATH, "export * as N from \"./y\";\n", &[("./y", "src/y.ts")]);
    assert!(graph.of_kind("reexport").is_empty(), "{:#?}", graph.graph.nodes);
    assert_eq!(graph.imported(), ["./y"]);
    assert_eq!(graph.of_kind("resolved_module").len(), 1);
}

// --- require() and import() --------------------------------------------------

#[test]
fn literal_require_and_dynamic_import_each_import_their_specifier() {
    let graph = extract(
        PATH,
        "const m = require(\"./m\");\nasync function f() {\n  await import(\"./n\");\n}\n",
        &[("./n", "src/n.ts")],
    );
    assert_eq!(graph.imported(), ["./m", "./n"]);
    graph.placeholder("external_module", "./m");
    graph.placeholder("resolved_module", "src/n.ts");
}

#[test]
fn conditional_specifier_imports_each_branch_at_the_call_argument_span() {
    let graph = extract(PATH, "require(c ? \"./a\" : \"./b\");\n", &[]);
    assert_eq!(graph.imported(), ["./a", "./b"]);
    // Both span the whole conditional, not their own branch.
    assert_eq!(graph.placeholder("external_module", "./a").range, range(0, 8, 25));
    assert_eq!(graph.placeholder("external_module", "./b").range, range(0, 8, 25));
}

#[test]
fn nested_conditional_imports_every_branch() {
    let graph = extract(PATH, "import(dev ? \"./dev\" : alt ? \"./alt\" : \"./prod\");\n", &[]);
    assert_eq!(graph.imported(), ["./alt", "./dev", "./prod"]);
}

#[test]
fn conditional_with_one_dynamic_branch_imports_neither() {
    let graph =
        extract(PATH, "import(dev ? \"./dev\" : getPath());\nrequire(dev ? `./${x}` : \"./b\");\n", &[]);
    assert!(graph.imported().is_empty(), "{:?}", graph.imported());
}

#[test]
fn template_with_an_unknown_substitution_imports_nothing() {
    let graph = extract(
        PATH,
        "export async function load(code: string) {\n  await import(`./locales/${code}.json`);\n}\n",
        &[],
    );
    assert!(graph.imported().is_empty(), "{:?}", graph.imported());
}

#[test]
fn template_over_constants_folds_through_chained_constants() {
    let graph = extract(
        PATH,
        // `NAME` is declared below its use: folding waits for the whole file.
        "import(`${NAME}.js`);\nconst DIR = \"./p\";\nconst NAME = `${DIR}/${LEAF}`;\nconst LEAF = ALPHA;\nconst ALPHA = \"alpha\";\n",
        &[],
    );
    assert_eq!(graph.imported(), ["./p/alpha.js"]);
}

#[test]
fn let_and_var_bindings_do_not_fold() {
    let graph = extract(
        PATH,
        "let L = \"alpha\";\nvar V = \"beta\";\nimport(`./p/${L}.js`);\nimport(`./p/${V}.js`);\nimport(L);\n",
        &[],
    );
    assert!(graph.imported().is_empty(), "{:?}", graph.imported());
}

#[test]
fn self_and_mutually_referencing_constants_terminate_without_folding() {
    let graph = extract(
        PATH,
        "const A = `${A}x`;\nconst B = C;\nconst C = `${B}`;\nimport(A);\nimport(`./${B}`);\n",
        &[],
    );
    assert!(graph.imported().is_empty(), "{:?}", graph.imported());
}

#[test]
fn string_enum_member_folds_and_nothing_else_about_an_enum_does() {
    let graph = extract(
        PATH,
        "enum Plugin {\n  Foo = \"foo\",\n  Count = 2,\n  Bar = `bar-${suffix}`,\n}\n\
         export async function boot(which: Plugin) {\n\
         \x20 await import(`./p/${Plugin.Foo}.js`);\n\
         \x20 await import(`./p/${which}.js`);\n\
         \x20 await import(`./p/${Plugin.Count}.js`);\n\
         \x20 await import(`./p/${Plugin.Bar}.js`);\n\
         \x20 await import(`./p/${Plugin.Missing}.js`);\n\
         }\n",
        &[],
    );
    assert_eq!(graph.imported(), ["./p/foo.js"]);
}

#[test]
fn member_of_something_other_than_an_enum_does_not_fold() {
    let graph = extract(
        PATH,
        "class Plugin {\n  static Foo = \"foo\";\n}\nconst O = { Foo: \"foo\" };\nimport(`./p/${Plugin.Foo}.js`);\nimport(O.Foo);\n",
        &[],
    );
    assert!(graph.imported().is_empty(), "{:?}", graph.imported());
}

#[test]
fn empty_folded_specifier_imports_nothing() {
    let graph = extract(
        PATH,
        "const E = \"\";\nimport(E);\nimport(`${E}`);\nrequire(\"\");\nimport(c ? \"./a\" : \"\");\n",
        &[],
    );
    assert!(graph.imported().is_empty(), "{:?}", graph.imported());
}

// --- path.join / path.resolve --------------------------------------------------

#[test]
fn path_join_over_dirname_is_a_relative_specifier() {
    let graph = extract(
        PATH,
        "import * as path from \"node:path\";\nconst NAME = \"alpha\";\n\
         import(path.join(__dirname, \"./plugins\", `${NAME}.js`));\n\
         import(path.resolve(__dirname, \"..\", \"shared.js\"));\n",
        &[],
    );
    assert_eq!(graph.imported(), ["../shared.js", "./plugins/alpha.js", "node:path"]);
}

#[test]
fn path_join_normalizes_like_posix_and_keeps_a_trailing_separator() {
    let graph = extract(
        PATH,
        "import path from \"path\";\n\
         import(path.join(__dirname, \"a\", \"\", \"./b/../c\", \"d\"));\n\
         import(path.join(__dirname, \"x//y/\"));\n\
         import(path.join(__dirname, \"q/../..\", \"r\"));\n",
        &[],
    );
    assert_eq!(graph.imported(), ["../r", "./a/c/d", "./x/y/", "path"]);
}

#[test]
fn path_join_is_refused_for_shapes_it_cannot_read() {
    let graph = extract(
        PATH,
        "import * as path from \"node:path\";\n\
         import(path.join(__dirname, \"/abs\", \"x.js\"));\n\
         import(path.join(__dirname));\n\
         import(path.join(__dirname, \".\"));\n\
         import(path.join(__dirname, \"a/..\"));\n\
         import(path.join(__filename, \"x.js\"));\n\
         import(path.join(process.cwd(), \"x.js\"));\n\
         import(path.join(\"lib\", \"x.js\"));\n\
         import(path.join(__dirname, unknown));\n",
        &[],
    );
    assert_eq!(graph.imported(), ["node:path"]);
}

#[test]
fn path_module_is_recognised_however_it_was_bound() {
    let required = extract(
        "src/a.js",
        "const p = require(\"node:path\");\nimport(p.join(__dirname, \"plugins\", \"index.js\"));\n",
        &[],
    );
    assert_eq!(required.imported(), ["./plugins/index.js", "node:path"]);

    let required_bare =
        extract("src/a.js", "const p = require(\"path\");\nimport(p.resolve(__dirname, \"x.js\"));\n", &[]);
    assert_eq!(required_bare.imported(), ["./x.js", "path"]);

    let default = extract(
        PATH,
        "import nodePath from \"path\";\nimport(nodePath.join(__dirname, \"boot.js\"));\n",
        &[],
    );
    assert_eq!(default.imported(), ["./boot.js", "path"]);

    let namespace =
        extract(PATH, "import * as np from \"node:path\";\nimport(np.join(__dirname, \"boot.js\"));\n", &[]);
    assert_eq!(namespace.imported(), ["./boot.js", "node:path"]);
}

#[test]
fn path_call_on_a_receiver_not_bound_to_the_path_module_does_not_fold() {
    let impostor = extract(
        PATH,
        "import * as path from \"./mypath\";\nimport(path.join(__dirname, \"boot.js\"));\n",
        &[],
    );
    assert_eq!(impostor.imported(), ["./mypath"]);

    let unbound = extract(PATH, "import(path.join(__dirname, \"boot.js\"));\n", &[]);
    assert!(unbound.imported().is_empty(), "{:?}", unbound.imported());

    let named = extract(PATH, "import { join } from \"path\";\nimport(join(__dirname, \"boot.js\"));\n", &[]);
    assert_eq!(named.imported(), ["path"]);

    let mutable =
        extract("src/a.js", "let p = require(\"path\");\nimport(p.join(__dirname, \"boot.js\"));\n", &[]);
    assert_eq!(mutable.imported(), ["path"]);
}

// --- order ---------------------------------------------------------------------

#[test]
fn call_imports_precede_the_late_exports_edges() {
    let graph = extract(PATH, "const a = 1;\nexport { a };\nconst m = require(\"./m\");\n", &[]);
    let kinds: Vec<EdgeKind> = graph.graph.edges.iter().map(|edge| edge.kind).collect();
    let imports = kinds.iter().position(|kind| *kind == EdgeKind::Imports).expect("an IMPORTS edge");
    let exports = kinds.iter().position(|kind| *kind == EdgeKind::Exports).expect("an EXPORTS edge");
    assert!(imports < exports, "{kinds:?}");
}
