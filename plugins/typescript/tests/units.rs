//! Unit tests for the pieces under the declaration pass: grammar routing, the
//! manifest pins, keys, and the draft model.

use std::collections::BTreeSet;

use g_mesh_plugin_sdk::ids::node_id;
use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind, PathSegment, Position, Range};
use g_mesh_plugin_sdk::RelPath;
use g_mesh_plugin_typescript::extractor::grammar::{grammar_for, Grammar, EXTENSIONS, GRAMMARS};
use g_mesh_plugin_typescript::extractor::keys::{
    is_sendable_path, join_path, qualified_in, qualify, MemberSeparator,
};
use g_mesh_plugin_typescript::extractor::model::{FileModel, NodeParams};
use g_mesh_plugin_typescript::project::{EXCLUDE_DIRS, WATCH_FILES};

fn manifest() -> toml::Value {
    toml::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"))).unwrap()
}

fn string_list(value: &toml::Value) -> Vec<&str> {
    value.as_array().expect("a list").iter().map(|item| item.as_str().expect("a string")).collect()
}

// --- grammar routing ---------------------------------------------------------

#[test]
fn grammar_for_routes_each_extension_to_its_grammar() {
    let cases = [
        ("src/a.ts", Grammar::TypeScript),
        ("src/a.mts", Grammar::TypeScript),
        ("src/a.cts", Grammar::TypeScript),
        ("types/a.d.ts", Grammar::TypeScript),
        ("src/a.tsx", Grammar::Tsx),
        ("src/a.js", Grammar::JavaScript),
        ("src/a.jsx", Grammar::JavaScript),
        ("src/a.mjs", Grammar::JavaScript),
        ("src/a.cjs", Grammar::JavaScript),
    ];
    for (path, grammar) in cases {
        assert_eq!(grammar_for(&RelPath::new(path)), Some(grammar), "{path}");
    }
}

#[test]
fn grammar_for_matches_extensions_case_insensitively() {
    assert_eq!(grammar_for(&RelPath::new("a.TS")), Some(Grammar::TypeScript));
    assert_eq!(grammar_for(&RelPath::new("a.Tsx")), Some(Grammar::Tsx));
    assert_eq!(grammar_for(&RelPath::new("A.JSX")), Some(Grammar::JavaScript));
    assert_eq!(grammar_for(&RelPath::new("a.MJS")), Some(Grammar::JavaScript));
}

#[test]
fn grammar_for_other_extensions_is_none() {
    for path in ["a.py", "a.json", "a.d", "Makefile", "a.tsx.bak", "a.ts/", "dir.ts/readme"] {
        assert_eq!(grammar_for(&RelPath::new(path)), None, "{path}");
    }
}

#[test]
fn extensions_equal_plugin_toml_languages_extensions() {
    let manifest = manifest();
    let listed = string_list(&manifest["plugin"]["languages"]["extensions"]);
    assert_eq!(listed, EXTENSIONS, "plugin.toml's extensions must equal grammar::EXTENSIONS");
}

#[test]
fn every_extension_has_exactly_one_grammar() {
    for extension in EXTENSIONS {
        let owners = GRAMMARS.iter().filter(|(_, extensions)| extensions.contains(&extension)).count();
        assert_eq!(owners, 1, "{extension} is owned by {owners} grammars");
    }
    let routed: BTreeSet<&str> =
        GRAMMARS.iter().flat_map(|(_, extensions)| extensions.iter().copied()).collect();
    let claimed: BTreeSet<&str> = EXTENSIONS.into_iter().collect();
    assert_eq!(routed, claimed, "GRAMMARS routes exactly the extensions the plugin claims");
}

#[test]
fn plugin_toml_grammars_equal_grammars() {
    let manifest = manifest();
    let table = manifest["plugin"]["grammars"].as_table().expect("[plugin.grammars] is a table");
    let listed: BTreeSet<(&str, Vec<&str>)> =
        table.iter().map(|(name, extensions)| (name.as_str(), string_list(extensions))).collect();
    let routed: BTreeSet<(&str, Vec<&str>)> =
        GRAMMARS.iter().map(|(grammar, extensions)| (grammar.name(), extensions.to_vec())).collect();
    assert_eq!(listed, routed, "plugin.toml's [plugin.grammars] must equal grammar::GRAMMARS");
}

// --- project ---------------------------------------------------------------

#[test]
fn plugin_toml_exclude_dirs_equal_exclude_dirs() {
    let manifest = manifest();
    let listed = string_list(&manifest["plugin"]["workspace"]["exclude_dirs"]);
    assert_eq!(listed, EXCLUDE_DIRS, "plugin.toml's exclude_dirs must equal project::EXCLUDE_DIRS");
}

#[test]
fn plugin_toml_watch_files_equal_watch_files() {
    let manifest = manifest();
    let listed = string_list(&manifest["plugin"]["workspace"]["watch_files"]);
    assert_eq!(listed, WATCH_FILES, "plugin.toml's watch_files must equal project::WATCH_FILES");
}

// --- keys ------------------------------------------------------------------

fn seg(sep: Option<&str>, name: &str) -> PathSegment {
    PathSegment { sep: sep.map(str::to_string), name: name.to_string() }
}

#[test]
fn qualified_in_at_the_root_is_the_bare_name_with_no_separator() {
    let qualified = qualified_in(&[], "Store", MemberSeparator::Hash);
    assert_eq!(qualified.qualified_name, "Store");
    assert_eq!(qualified.qualified_path, vec![seg(None, "Store")]);
}

#[test]
fn qualified_in_uses_hash_for_instance_and_dot_for_static_and_namespace_members() {
    let store = [seg(None, "Store")];
    let pick = qualified_in(&store, "pick", MemberSeparator::Hash);
    assert_eq!(pick.qualified_name, "Store#pick");
    assert_eq!(pick.qualified_path, vec![seg(None, "Store"), seg(Some("#"), "pick")]);
    assert_eq!(qualified_in(&store, "drop", MemberSeparator::Dot).qualified_name, "Store.drop");

    let namespace = [seg(None, "NS"), seg(Some("."), "Inner")];
    assert_eq!(qualified_in(&namespace, "f", MemberSeparator::Dot).qualified_name, "NS.Inner.f");
}

#[test]
fn qualified_in_private_member_keeps_its_hash_in_the_name() {
    let private = qualified_in(&[seg(None, "C")], "#priv", MemberSeparator::Hash);
    assert_eq!(private.qualified_name, "C##priv");
    assert_eq!(private.qualified_path.last(), Some(&seg(Some("#"), "#priv")));
}

#[test]
fn join_path_concatenates_separators_and_names() {
    let path = [seg(None, "Outer.Inner"), seg(Some("."), "C"), seg(Some("#"), "m")];
    assert_eq!(join_path(&path), "Outer.Inner.C#m");
    assert_eq!(join_path(&[]), "");
}

#[test]
fn qualify_joins_under_a_prefix_and_is_bare_at_the_root() {
    assert_eq!(qualify("", "f", MemberSeparator::Dot), "f");
    assert_eq!(qualify("NS", "f", MemberSeparator::Dot), "NS.f");
    assert_eq!(qualify("C", "m", MemberSeparator::Hash), "C#m");
}

#[test]
fn is_sendable_path_accepts_a_well_formed_path() {
    assert!(is_sendable_path(&[seg(None, "C")], "C"));
    assert!(is_sendable_path(&[seg(None, "C"), seg(Some("#"), "#priv")], "#priv"));
}

#[test]
fn is_sendable_path_rejects_an_empty_path_or_an_empty_name() {
    assert!(!is_sendable_path(&[], ""));
    assert!(!is_sendable_path(&[seg(None, "")], ""));
    assert!(!is_sendable_path(&[seg(None, ""), seg(Some("."), "f")], "f"));
}

#[test]
fn is_sendable_path_rejects_a_separator_on_the_first_segment() {
    assert!(!is_sendable_path(&[seg(Some("."), "C")], "C"));
}

#[test]
fn is_sendable_path_rejects_a_missing_or_empty_separator_later() {
    assert!(!is_sendable_path(&[seg(None, "C"), seg(None, "m")], "m"));
    assert!(!is_sendable_path(&[seg(None, "C"), seg(Some(""), "m")], "m"));
}

#[test]
fn is_sendable_path_rejects_unit_separator_and_nul() {
    assert!(!is_sendable_path(&[seg(None, "a\u{1f}b")], "a\u{1f}b"));
    assert!(!is_sendable_path(&[seg(None, "a\0b")], "a\0b"));
    assert!(!is_sendable_path(&[seg(None, "C"), seg(Some("\u{1f}"), "m")], "m"));
    assert!(!is_sendable_path(&[seg(None, "C"), seg(Some("\0"), "m")], "m"));
}

#[test]
fn is_sendable_path_rejects_a_last_name_other_than_name() {
    assert!(!is_sendable_path(&[seg(None, "C"), seg(Some("#"), "m")], "n"));
}

// --- model -----------------------------------------------------------------

const PATH: &str = "src/a.ts";

fn range(start_line: u32, end_line: u32) -> Range {
    Range { start: Position { line: start_line, col: 0 }, end: Position { line: end_line, col: 1 } }
}

fn model() -> FileModel {
    FileModel::new(PATH, range(0, 20))
}

fn function(name: &str, at: Range) -> NodeParams {
    let mut params = NodeParams::new(NodeKind::Function, name, name, at);
    params.native_kind = Some("function".to_string());
    params
}

/// One declaration of `f`, starting on `line`.
fn declaration(line: u32, has_body: bool, signature: Option<&str>, doc: Option<&str>) -> NodeParams {
    let mut params = function("f", range(line, line + 1));
    params.has_body = has_body;
    params.signature = signature.map(str::to_string);
    params.doc_comment = doc.map(str::to_string);
    params
}

#[test]
fn file_model_starts_with_the_file_node() {
    let model = model();
    let file = model.node(0);
    assert_eq!(file.kind, NodeKind::File);
    assert_eq!(file.name, "a.ts");
    assert_eq!(file.qualified_name, PATH);
    assert_eq!(model.file_id(), node_id(PATH, NodeKind::File, PATH, None));
}

#[test]
fn add_node_same_id_twice_is_one_node() {
    let mut model = model();
    let first = model.add_node(function("f", range(1, 2)));
    let second = model.add_node(function("f", range(3, 4)));
    assert_eq!(first, second);
    let (nodes, _) = model.into_parts();
    assert_eq!(nodes.len(), 2, "the File node and one `f`");
    assert_eq!(nodes[1].range, range(1, 2), "a redeclaration does not move the node by itself");
}

#[test]
fn add_node_exported_redeclaration_makes_the_node_public() {
    let mut model = model();
    let index = model.add_node(function("f", range(1, 2)));
    assert!(!model.node(index).exported);
    let mut exported = function("f", range(3, 4));
    exported.exported = true;
    model.add_node(exported);
    assert!(model.node(index).exported);
    model.add_node(function("f", range(5, 6)));
    assert!(model.node(index).exported, "a later unexported declaration does not take it back");
}

#[test]
fn fill_declaration_lists_sorts_declarations_by_start() {
    let mut model = model();
    let index = model.add_node(declaration(9, true, Some("f(a)"), None));
    model.add_node(declaration(1, false, Some("f(b)"), None));
    model.add_node(declaration(5, false, Some("f(c)"), None));
    model.fill_declaration_lists();
    let declarations = model.node(index).declarations.clone().expect("three declarations get a list");
    let order: Vec<(u32, u32, Option<&str>, bool)> =
        declarations.iter().map(|d| (d.ordinal, d.start_line, d.signature.as_deref(), d.has_body)).collect();
    assert_eq!(
        order,
        vec![(0, 1, Some("f(b)"), false), (1, 5, Some("f(c)"), false), (2, 9, Some("f(a)"), true)]
    );
    assert_eq!((declarations[2].end_line, declarations[2].end_col), (10, 1));
}

#[test]
fn fill_declaration_lists_takes_the_range_of_the_first_declaration_with_a_body() {
    let mut model = model();
    let index = model.add_node(declaration(1, false, None, None));
    model.add_node(declaration(7, true, None, None));
    model.add_node(declaration(3, true, None, None));
    model.fill_declaration_lists();
    assert_eq!(model.node(index).range, range(3, 4));
}

#[test]
fn fill_declaration_lists_without_a_body_takes_the_first_range() {
    let mut model = model();
    let index = model.add_node(declaration(6, false, None, None));
    model.add_node(declaration(2, false, None, None));
    model.fill_declaration_lists();
    assert_eq!(model.node(index).range, range(2, 3));
}

#[test]
fn fill_declaration_lists_takes_the_signature_of_the_first_bodiless_declaration() {
    let mut model = model();
    let index = model.add_node(declaration(1, true, Some("f(impl)"), None));
    model.add_node(declaration(8, false, Some("f(late)"), None));
    model.add_node(declaration(4, false, Some("f(early)"), None));
    model.fill_declaration_lists();
    assert_eq!(model.node(index).signature.as_deref(), Some("f(early)"));
}

#[test]
fn fill_declaration_lists_takes_the_doc_of_the_first_declaration_that_has_one() {
    let mut model = model();
    let index = model.add_node(declaration(9, true, None, Some("late doc")));
    model.add_node(declaration(1, false, None, None));
    model.add_node(declaration(4, false, None, Some("early doc")));
    model.fill_declaration_lists();
    assert_eq!(model.node(index).doc_comment.as_deref(), Some("early doc"));
}

#[test]
fn fill_declaration_lists_leaves_a_single_declaration_without_a_list() {
    let mut model = model();
    let index = model.add_node(declaration(3, false, Some("f()"), Some("doc")));
    model.fill_declaration_lists();
    let node = model.node(index);
    assert_eq!(node.declarations, None);
    assert_eq!(node.range, range(3, 4));
    assert_eq!(node.signature.as_deref(), Some("f()"));
    assert_eq!(model.node(0).declarations, None, "the File node gets no list either");
}

#[test]
fn placeholders_record_no_declarations() {
    let mut model = model();
    let mut placeholder = NodeParams::new(NodeKind::Module, "x", "src/b.ts#x", range(1, 1));
    placeholder.native_kind = Some("pending_symbol".to_string());
    let index = model.add_node(placeholder.clone());
    model.add_node(placeholder);
    model.fill_declaration_lists();
    assert_eq!(model.node(index).declarations, None);
}

#[test]
fn add_edge_drops_an_edge_with_a_missing_end() {
    let mut model = model();
    let file = model.file_id().to_string();
    let f = model.add_node(function("f", range(1, 2)));
    let f = model.node(f).id.clone();
    model.add_edge(&file, EdgeKind::Defines, "no-such-node");
    model.add_edge("no-such-node", EdgeKind::Calls, &f);
    let (_, edges) = model.into_parts();
    assert!(edges.is_empty(), "{edges:?}");
}

#[test]
fn add_edge_deduplicates_by_id() {
    let mut model = model();
    let file = model.file_id().to_string();
    let f = model.add_node(function("f", range(1, 2)));
    let f = model.node(f).id.clone();
    model.add_edge(&file, EdgeKind::Defines, &f);
    model.add_edge(&file, EdgeKind::Defines, &f);
    model.add_edge(&file, EdgeKind::Exports, &f);
    let (_, edges) = model.into_parts();
    let kinds: Vec<EdgeKind> = edges.iter().map(|edge| edge.kind).collect();
    assert_eq!(kinds, vec![EdgeKind::Defines, EdgeKind::Exports]);
}

#[test]
fn add_edge_onto_a_placeholder_is_unresolved() {
    let mut model = model();
    let file = model.file_id().to_string();
    let f = model.add_node(function("f", range(1, 2)));
    let f = model.node(f).id.clone();
    let mut placeholder = NodeParams::new(NodeKind::Module, "x", "src/b.ts#x", range(1, 1));
    placeholder.native_kind = Some("pending_symbol".to_string());
    let placeholder = model.add_node(placeholder);
    let placeholder = model.node(placeholder).id.clone();
    model.add_edge(&file, EdgeKind::Defines, &f);
    model.add_edge(&f, EdgeKind::Calls, &placeholder);
    let (_, edges) = model.into_parts();
    let resolved: Vec<(EdgeKind, bool)> = edges.iter().map(|edge| (edge.kind, edge.resolved)).collect();
    assert_eq!(resolved, vec![(EdgeKind::Defines, true), (EdgeKind::Calls, false)]);
}

fn declare(model: &mut FileModel, kind: NodeKind, qualified_name: &str) -> usize {
    let name = qualified_name.rsplit('.').next().unwrap();
    model.declare_symbol(NodeParams::new(kind, name, qualified_name, range(1, 2)))
}

#[test]
fn lookup_by_name_searches_the_innermost_namespace_outwards() {
    let mut model = model();
    let root_f = declare(&mut model, NodeKind::Function, "f");
    let ns_f = declare(&mut model, NodeKind::Function, "NS.f");
    let root_h = declare(&mut model, NodeKind::Function, "h");
    let deep_g = declare(&mut model, NodeKind::Function, "NS.Inner.g");

    assert_eq!(model.lookup_by_name("f", "NS.Inner", None), Some(ns_f), "NS.Inner.f, then NS.f");
    assert_eq!(model.lookup_by_name("f", "", None), Some(root_f));
    assert_eq!(model.lookup_by_name("h", "NS.Inner", None), Some(root_h), "falls back to the root");
    assert_eq!(model.lookup_by_name("g", "NS.Inner", None), Some(deep_g));
    assert_eq!(model.lookup_by_name("g", "NS", None), None, "never searches inwards");
    assert_eq!(model.lookup_by_name("missing", "NS.Inner", None), None);
}

#[test]
fn lookup_by_name_with_a_kind_skips_other_kinds() {
    let mut model = model();
    let root_f = declare(&mut model, NodeKind::Variable, "f");
    declare(&mut model, NodeKind::Function, "NS.f");
    assert_eq!(model.lookup_by_name("f", "NS", Some(NodeKind::Variable)), Some(root_f));
    assert_eq!(model.lookup_by_name("f", "NS", Some(NodeKind::Type)), None);
}
