//! `deps::export` over hand-built indexes, and its pure helpers.
//! What a real plugin produces is pinned end to end in
//! `core/tests/it/cli_deps.rs`; these pin the shapes no fixture project of
//! the bundled plugins reaches cheaply (a container defined by several files,
//! duplicate import edges, a Python-relative `external_module`).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;
use crate::graph::containers::container_id;
use crate::graph::imports::RESOLVED_MODULE_NATIVE_KIND;
use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};

fn file_id(path: &str) -> String {
    format!("file:{path}")
}

fn file(path: &str, language: &str) -> NodeRecord {
    NodeRecord::new(file_id(path), "File", path, path, path, language)
}

/// An ordinary declaration in `path` that is a member of container `key`.
fn member(path: &str, language: &str, key: &str) -> NodeRecord {
    let mut node =
        NodeRecord::new(format!("{path}#{key}"), "Function", "f", format!("{key}.f"), path, language);
    node.container = Some(key.to_string());
    node
}

/// `path`'s own whole-file `Module` node, as a member of container `key`
/// (a Python sub-module of package `key`).
fn submodule(path: &str, language: &str, key: &str, qualified_name: &str) -> NodeRecord {
    let mut node =
        NodeRecord::new(format!("module:{path}"), "Module", qualified_name, qualified_name, path, language);
    node.container = Some(key.to_string());
    node
}

fn import_file(id: &str, from: &str, to: &str, specifier: &str) -> EdgeRecord {
    let mut edge = EdgeRecord::new(id, file_id(from), file_id(to), "IMPORTS", "syntactic", true);
    edge.specifier = Some(specifier.to_string());
    edge
}

fn import_container(id: &str, from: &str, language: &str, key: &str) -> EdgeRecord {
    let mut edge =
        EdgeRecord::new(id, file_id(from), container_id(language, key), "IMPORTS", "syntactic", true);
    edge.specifier = Some(key.to_string());
    edge
}

/// A placeholder `Module` node for `specifier` imported by `from`.
fn placeholder(from: &str, language: &str, specifier: &str, native_kind: &str) -> NodeRecord {
    let mut node =
        NodeRecord::new(format!("ph:{from}:{specifier}"), "Module", specifier, specifier, from, language);
    node.native_kind = Some(native_kind.to_string());
    node
}

fn import_placeholder(id: &str, from: &str, specifier: &str) -> EdgeRecord {
    let mut edge =
        EdgeRecord::new(id, file_id(from), format!("ph:{from}:{specifier}"), "IMPORTS", "syntactic", false);
    edge.specifier = Some(specifier.to_string());
    edge
}

fn indexed(languages: &[&str]) -> BTreeMap<String, LanguageOutcome> {
    languages.iter().map(|language| (language.to_string(), LanguageOutcome::Indexed { files: 0 })).collect()
}

fn index(
    nodes: Vec<NodeRecord>,
    edges: Vec<EdgeRecord>,
    outcomes: &BTreeMap<String, LanguageOutcome>,
) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    // Containers materialize from their members, so edges into them go in a
    // second diff.
    apply_diff(&mut conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
    apply_diff(&mut conn, &Diff { upsert_edges: edges, ..Default::default() }).unwrap();
    schema::record_language_outcomes(&conn, outcomes).unwrap();
    conn
}

fn report(conn: &Connection) -> Value {
    let (report, _) = export(conn, Path::new("/project"), &HashMap::new()).unwrap();
    serde_json::to_value(&report).unwrap()
}

fn edge(from: &str, to: &str, kind: &str, language: &str, module: Option<&str>) -> Value {
    json!({ "from": from, "to": to, "kind": kind, "language": language, "module": module })
}

#[test]
fn a_container_defined_by_files_in_one_directory_is_one_dir_edge_never_onto_the_importers_own_directory() {
    let conn = index(
        vec![
            file("main.go", "go"),
            file("pkg/a.go", "go"),
            file("pkg/b.go", "go"),
            file("pkg/c.go", "go"),
            member("pkg/a.go", "go", "ex/pkg"),
            member("pkg/b.go", "go", "ex/pkg"),
        ],
        vec![
            import_container("main-pkg", "main.go", "go", "ex/pkg"),
            // A file inside the package's directory importing it: a `dir`
            // edge onto its own directory says nothing and is dropped.
            import_container("c-pkg", "pkg/c.go", "go", "ex/pkg"),
        ],
        &indexed(&["go"]),
    );
    let report = report(&conn);
    assert_eq!(report["edges"], json!([edge("main.go", "pkg", "dir", "go", Some("ex/pkg"))]), "{report}");
    assert_eq!(report["coverage"]["go"]["edges"], 1, "{report}");
    assert_eq!(
        report["coverage"]["go"]["unresolved"], 0,
        "a dropped own-dir edge is not unresolved: {report}"
    );
}

#[test]
fn a_container_defined_in_several_directories_is_one_file_edge_per_file() {
    let conn = index(
        vec![
            file("main.go", "go"),
            file("a/x.go", "go"),
            file("b/y.go", "go"),
            member("a/x.go", "go", "split"),
            member("b/y.go", "go", "split"),
        ],
        vec![import_container("main-split", "main.go", "go", "split")],
        &indexed(&["go"]),
    );
    let report = report(&conn);
    assert_eq!(
        report["edges"],
        json!([
            edge("main.go", "a/x.go", "file", "go", Some("split")),
            edge("main.go", "b/y.go", "file", "go", Some("split")),
        ]),
        "{report}"
    );
}

#[test]
fn a_self_import_is_dropped_and_duplicate_imports_collapse_to_one_edge() {
    let conn = index(
        vec![file("a.ts", "typescript"), file("b.ts", "typescript")],
        vec![
            import_file("self", "a.ts", "a.ts", "./a"),
            import_file("b-1", "a.ts", "b.ts", "./b"),
            import_file("b-2", "a.ts", "b.ts", "./b.js"),
        ],
        &indexed(&["typescript"]),
    );
    let report = report(&conn);
    assert_eq!(report["edges"], json!([edge("a.ts", "b.ts", "file", "typescript", None)]), "{report}");
    assert_eq!(report["coverage"]["typescript"]["edges"], 1, "counted after the collapse: {report}");
}

#[test]
fn a_relative_external_module_is_unresolved_a_bare_one_external_each_counted_once_per_importer() {
    let conn = index(
        vec![
            file("a.ts", "typescript"),
            placeholder("a.ts", "typescript", "react", EXTERNAL_MODULE_NATIVE_KIND),
            placeholder("a.ts", "typescript", "./missing", EXTERNAL_MODULE_NATIVE_KIND),
            placeholder("a.ts", "typescript", "../up", EXTERNAL_MODULE_NATIVE_KIND),
            placeholder("a.ts", "typescript", ".", EXTERNAL_MODULE_NATIVE_KIND),
            placeholder("a.ts", "typescript", "..", EXTERNAL_MODULE_NATIVE_KIND),
            file("p.py", "python"),
            placeholder("p.py", "python", "os", EXTERNAL_MODULE_NATIVE_KIND),
            placeholder("p.py", "python", ".sibling", EXTERNAL_MODULE_NATIVE_KIND),
            placeholder("p.py", "python", "pkg.gone", RESOLVED_MODULE_NATIVE_KIND),
        ],
        vec![
            import_placeholder("react-1", "a.ts", "react"),
            // The same package and the same missing file imported twice.
            import_placeholder("react-2", "a.ts", "react"),
            import_placeholder("missing-1", "a.ts", "./missing"),
            import_placeholder("missing-2", "a.ts", "./missing"),
            import_placeholder("up", "a.ts", "../up"),
            import_placeholder("dot", "a.ts", "."),
            import_placeholder("dotdot", "a.ts", ".."),
            import_placeholder("os", "p.py", "os"),
            import_placeholder("sibling", "p.py", ".sibling"),
            import_placeholder("gone", "p.py", "pkg.gone"),
        ],
        &indexed(&["python", "typescript"]),
    );
    let report = report(&conn);
    assert_eq!(report["edges"], json!([]), "a placeholder is never an edge: {report}");
    assert_eq!(report["coverage"]["typescript"]["external"], 1, "{report}");
    assert_eq!(report["coverage"]["typescript"]["unresolved"], 4, "./missing, ../up, . and ..: {report}");
    assert_eq!(report["coverage"]["python"]["external"], 1, "{report}");
    assert_eq!(report["coverage"]["python"]["unresolved"], 2, ".sibling and pkg.gone: {report}");
    assert_eq!(report["unresolved"], json!({ "python": 2, "typescript": 4 }), "{report}");
}

#[test]
fn a_package_whose_members_are_only_submodules_maps_to_its_init_file_else_to_its_directory() {
    // An empty `pkg/__init__.py`: a File node and nothing else, so no file
    // defines container `pkg`; its members are its sub-modules' own nodes.
    let nodes = |with_init: bool| {
        let mut nodes = vec![
            file("main.py", "python"),
            file("pkg/a.py", "python"),
            file("pkg/b.py", "python"),
            submodule("pkg/a.py", "python", "pkg", "pkg.a"),
            submodule("pkg/b.py", "python", "pkg", "pkg.b"),
        ];
        if with_init {
            nodes.push(file("pkg/__init__.py", "python"));
        }
        nodes
    };
    let edges = || vec![import_container("main-pkg", "main.py", "python", "pkg")];

    let package = report(&index(nodes(true), edges(), &indexed(&["python"])));
    assert_eq!(
        package["edges"],
        json!([edge("main.py", "pkg/__init__.py", "file", "python", Some("pkg"))]),
        "{package}"
    );

    let namespace = report(&index(nodes(false), edges(), &indexed(&["python"])));
    assert_eq!(
        namespace["edges"],
        json!([edge("main.py", "pkg", "dir", "python", Some("pkg"))]),
        "{namespace}"
    );
}

#[test]
fn the_top_level_keys_and_version_are_pinned() {
    let report = report(&index(vec![file("a.ts", "typescript")], vec![], &indexed(&["typescript"])));
    let keys: Vec<&str> = report.as_object().unwrap().keys().map(String::as_str).collect();
    let mut expected = vec!["coverage", "edges", "languages", "project_root", "unresolved", "version"];
    let mut keys_sorted = keys.clone();
    keys_sorted.sort_unstable();
    expected.sort_unstable();
    assert_eq!(keys_sorted, expected, "adding or removing a top-level key changes format version 1");
    assert_eq!(DEPS_FORMAT_VERSION, 1);
    assert_eq!(report["version"], 1);
    assert_eq!(report["project_root"], "/project");
    assert_eq!(report["languages"], json!(["typescript"]));
    assert_eq!(report["unresolved"], json!({}), "an indexed language with nothing unresolved is not listed");
}

#[test]
fn absent_and_failed_languages_are_in_coverage_only_and_nothing_indexed_lists_no_language() {
    let outcomes: BTreeMap<String, LanguageOutcome> = [
        ("rust".to_string(), LanguageOutcome::PluginAbsent { files: Some(3) }),
        ("go".to_string(), LanguageOutcome::Failed { error: "the plugin crashed\nexit 1".to_string() }),
    ]
    .into_iter()
    .collect();
    let report = report(&index(vec![], vec![], &outcomes));
    assert_eq!(report["languages"], json!([]), "{report}");
    assert_eq!(report["coverage"]["rust"], json!({ "status": "plugin_absent", "files": 3 }), "{report}");
    assert_eq!(report["coverage"]["go"]["status"], "failed", "{report}");
    assert!(report["coverage"]["go"]["error"].as_str().is_some_and(|e| !e.contains('\n')), "{report}");
    assert_eq!(report["unresolved"], json!({}), "{report}");
    assert_eq!(report["edges"], json!([]), "{report}");
}

#[test]
fn relative_specifiers_are_dot_slash_dot_dot_and_for_python_any_leading_dot() {
    for (language, specifier, relative) in [
        ("typescript", "./x", true),
        ("typescript", "../x", true),
        ("typescript", ".", true),
        ("typescript", "..", true),
        ("typescript", "react", false),
        ("typescript", "@scope/pkg", false),
        ("typescript", ".hidden", false),
        ("python", ".", true),
        ("python", ".sibling", true),
        ("python", "..parent", true),
        ("python", "os", false),
        ("rust", "std", false),
    ] {
        assert_eq!(is_relative_specifier(language, specifier), relative, "{language} {specifier:?}");
    }
}

#[test]
fn targets_of_and_common_dir_follow_the_directory_rule() {
    let paths = |list: &[&str]| list.iter().map(|path| path.to_string()).collect::<Vec<_>>();
    assert_eq!(targets_of(&[]), vec![]);
    assert_eq!(targets_of(&paths(&["a/x.go"])), vec![("a/x.go".to_string(), "file")]);
    assert_eq!(targets_of(&paths(&["a/x.go", "a/y.go"])), vec![("a".to_string(), "dir")]);
    assert_eq!(targets_of(&paths(&["x.go", "y.go"])), vec![(".".to_string(), "dir")], "the root is `.`");
    assert_eq!(
        targets_of(&paths(&["a/x.go", "b/y.go"])),
        vec![("a/x.go".to_string(), "file"), ("b/y.go".to_string(), "file")]
    );

    assert_eq!(common_dir(&[]), None);
    assert_eq!(common_dir(&paths(&["pkg/a.py", "pkg/sub/b.py"])), Some("pkg".to_string()));
    assert_eq!(common_dir(&paths(&["pkg/a.py", "other/b.py"])), Some(".".to_string()));
    assert_eq!(common_dir(&paths(&["a.py"])), Some(".".to_string()));
    assert_eq!(parent_dir("a/b/c.rs"), "a/b");
    assert_eq!(parent_dir("c.rs"), ".");
}
