use crate::mcp::query_shapes::QueryShapes;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::*;
use crate::daemon::bulk_index;
use crate::daemon::manifest::{read_manifest, Capabilities, DiscoveredPlugins};
use crate::embedding::EmbeddingPipeline;
use crate::graph::queries::{self, upsert_edge, upsert_node};
use crate::mcp::session_hints::SessionHints;
use crate::mcp::{find_callers_callees, find_references, SymbolQueryParams};
use crate::protocol::types::QualifiedPath;
use crate::storage::connection::{open, project_dir};
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::{EdgeRecord, PlaceholderTargetRecord};

const SHARED: &[(&str, &str)] = &[
    ("Cargo.toml", "[package]\nname = \"krate\"\nversion = \"0.1.0\"\n"),
    ("src/lib.rs", "pub mod ipc;\npub mod m;\npub mod user;\n"),
    ("src/m/mod.rs", "pub mod inner;\npub use inner::P;\n"),
    ("src/m/inner.rs", "pub struct P;\n\nimpl P {\n    pub fn load(c: u32) -> Self {\n        let _ = c;\n        P\n    }\n}\n"),
    (
        "src/ipc/mod.rs",
        "#[cfg(unix)]\npub mod unix;\n#[cfg(windows)]\npub mod windows;\n\n\
         #[cfg(unix)]\npub use unix::Listener;\n#[cfg(windows)]\npub use windows::Listener;\n",
    ),
    ("src/ipc/unix.rs", "pub struct Listener;\n\nimpl Listener {\n    pub fn bind() -> Self {\n        Listener\n    }\n}\n"),
    ("src/ipc/windows.rs", "pub struct Listener;\n\nimpl Listener {\n    pub fn bind() -> Self {\n        Listener\n    }\n}\n"),
];

/// `P::load` called only from a module whose glob reaches `P` through a
/// *sibling's* private `use`, which Rust refuses, and `Listener::bind`
/// through a type re-exported under two `#[cfg]` arms: the linker attaches
/// neither call to its declaration.
const UNLINKED_USER: &str = "use crate::ipc::Listener;\n\n\
     pub fn serve() {\n    Listener::bind();\n}\n\n\
     mod imports {\n    use crate::m::P;\n\n    mod child {}\n}\n\n\
     mod other {\n    use super::imports::*;\n\n    fn loads() {\n        P::load(1);\n    }\n}\n";

/// The same `P::load` call behind a top-level `use`, which links.
const LINKED_USER: &str = "use crate::m::P;\n\npub fn loads() {\n    P::load(1);\n}\n";

fn rust_only() -> DiscoveredPlugins {
    let plugins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins");
    let manifest = read_manifest(&plugins.join("rust")).expect("the checked-in rust plugin manifest");
    DiscoveredPlugins {
        manifests: HashMap::from([("rust".to_string(), manifest)]),
        routing: HashMap::from([(".rs".to_string(), "rust".to_string())]),
    }
}

/// A temp crate walked by the real rust plugin and linker. Its per-project
/// state directory is removed on drop.
struct Fixture {
    dir: tempfile::TempDir,
    store: Arc<IndexStore>,
}

impl Fixture {
    fn new(user: &str) -> Self {
        Self::with_files(user, &[])
    }

    /// The shared crate, `src/user.rs`, and `extra` files besides.
    fn with_files(user: &str, extra: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp project root");
        for (path, contents) in
            SHARED.iter().copied().chain([("src/user.rs", user)]).chain(extra.iter().copied())
        {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, contents).unwrap();
        }
        let conn = open(dir.path()).expect("failed to open the project index");
        schema::ensure_current(&conn, "unlinked-usages-test").expect("failed to prepare the index");
        let discovered = rust_only();
        let rules = crate::daemon::manifest::link_rules(discovered.manifests.values());
        let store = IndexStore::new(conn).with_link_rules(rules);
        let summary = bulk_index::run(dir.path(), &store, None, &discovered).expect("the bulk walk failed");
        assert!(summary.nodes > 0, "the walk produced no nodes");
        Self { dir, store: Arc::new(store) }
    }

    fn callers(&self, symbol: &str) -> serde_json::Value {
        json_body(
            &find_callers_callees::handle_callers(
                &self.store,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &HashMap::<String, Capabilities>::new(),
                &SessionHints::default(),
                by_name(symbol),
            )
            .unwrap(),
        )
    }

    fn references(&self, symbol: &str) -> serde_json::Value {
        json_body(
            &find_references::handle(
                &self.store,
                &EmbeddingPipeline::disabled(),
                QueryShapes::shipped(),
                &HashMap::<String, Capabilities>::new(),
                &SessionHints::default(),
                by_name(symbol),
            )
            .unwrap(),
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.dir.path()) {
            let _ = std::fs::remove_dir_all(state);
        }
    }
}

fn by_name(symbol: &str) -> SymbolQueryParams {
    SymbolQueryParams { symbol_name: Some(symbol.to_string()), ..Default::default() }
}

fn json_body(result: &CallToolResult) -> serde_json::Value {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

fn caller_names(body: &serde_json::Value) -> Vec<String> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["qualifiedName"].as_str().unwrap().to_string())
        .collect()
}

/// A call through a sibling's private `use` and a call through a cfg-twin
/// re-export both stay on placeholders. Each declaration's page has no row
/// for them, so it must say it is not exact.
///
/// The first half is also the end-to-end sibling case of
/// docs/architecture/gm-479-use-super-private-imports.md. Control: drop the
/// `restricted_to` check from `Resolver::walk` - `user::other::loads` becomes
/// a caller and the marker goes.
#[test]
fn calls_the_linker_left_on_placeholders_mark_the_callers_page_not_exact() {
    let fixture = Fixture::new(UNLINKED_USER);

    let load = fixture.callers("m::inner::P::load");
    assert_eq!(load["hasMore"], false, "{load}");
    assert!(caller_names(&load).is_empty(), "a sibling's private import is not followed: {load}");
    assert!(load["unlinkedUsages"]["count"].as_u64().unwrap() >= 1, "{load}");
    assert_eq!(load["unlinkedUsages"]["files"], serde_json::json!([{ "path": "src/user.rs", "refs": 1 }]));

    let bind = fixture.callers("ipc::unix::Listener::bind");
    assert!(caller_names(&bind).is_empty(), "the cfg-twin call is not linked: {bind}");
    assert!(bind["unlinkedUsages"]["count"].as_u64().unwrap() >= 1, "{bind}");
}

#[test]
fn calls_the_linker_left_on_placeholders_mark_the_references_page_not_exact() {
    let fixture = Fixture::new(UNLINKED_USER);

    let load = fixture.references("m::inner::P::load");
    assert!(load["unlinkedUsages"]["count"].as_u64().unwrap() >= 1, "{load}");
}

/// The control: the same call behind a top-level `use` links, so the row is
/// there and the marker is not.
#[test]
fn a_linked_call_is_a_row_and_carries_no_marker() {
    let fixture = Fixture::new(LINKED_USER);

    let callers = fixture.callers("m::inner::P::load");
    assert_eq!(caller_names(&callers), ["user::loads"], "{callers}");
    assert!(callers.get("unlinkedUsages").is_none(), "{callers}");

    let references = fixture.references("m::inner::P::load");
    assert!(references.get("unlinkedUsages").is_none(), "{references}");
}

/// `mod tests { use super::*; }` calling `<head>::load`, with `prelude` above
/// it in `src/user.rs`.
fn tests_calling(prelude: &str, head: &str) -> String {
    format!(
        "{prelude}\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    #[test]\n    fn loads() {{\n        \
         {head}::load(1);\n    }}\n}}\n"
    )
}

/// Asserts `symbol`'s callers are exactly `callers`, with no marker.
fn assert_linked_callers(fixture: &Fixture, symbol: &str, callers: &[&str]) {
    let page = fixture.callers(symbol);
    assert_eq!(caller_names(&page), callers, "{page}");
    assert!(page.get("unlinkedUsages").is_none(), "{page}");
}

/// A test module's `use super::*` reaches what its parent imported
/// privately, inline or as a file-backed `tests.rs`, and through an `as`
/// alias (docs/architecture/gm-479-use-super-private-imports.md).
///
/// Control: make `Declarer::use_leaf` emit no row for a private `use` (the
/// plugin before GM-479) - no caller, and the marker is back.
#[test]
fn a_test_module_calls_through_its_parents_private_import() {
    let inline = Fixture::new(&tests_calling("use crate::m::P;\n", "P"));
    assert_linked_callers(&inline, "m::inner::P::load", &["user::tests::loads"]);

    let aliased = Fixture::new(&tests_calling("use crate::m::P as Q;\n", "Q"));
    assert_linked_callers(&aliased, "m::inner::P::load", &["user::tests::loads"]);

    let file_backed = Fixture::with_files(
        "use crate::m::P;\n\n#[cfg(test)]\nmod tests;\n",
        &[("src/user/tests.rs", "use super::*;\n\n#[test]\nfn loads() {\n    P::load(1);\n}\n")],
    );
    assert_linked_callers(&file_backed, "m::inner::P::load", &["user::tests::loads"]);
}

/// A type the parent declares itself needs only the test module's glob.
///
/// Control: make `Declarer::use_leaf` emit no row for a private glob - no
/// caller.
#[test]
fn a_test_module_calls_a_type_its_parent_declares() {
    let fixture = Fixture::new(&tests_calling(
        "pub struct D;\n\nimpl D {\n    pub fn load(c: u32) {\n        let _ = c;\n    }\n}\n",
        "D",
    ));
    assert_linked_callers(&fixture, "user::D::load", &["user::tests::loads"]);
}

/// `pub mod x` declaring `<head>::load`, beside a `use self::x::*;` placed
/// below the parent's named `use` line `named`.
fn glob_of_x_declaring(named: &str, head: &str) -> String {
    format!(
        "{named}\nuse self::x::*;\n\npub mod x {{\n    pub struct {head};\n\n    impl {head} {{\n        \
         pub fn load(c: u32) {{\n            let _ = c;\n        }}\n    }}\n}}\n"
    )
}

/// The parent's explicit `use std::fmt::Error;` shadows its `use self::x::*;`
/// for the test module too: the call is not linked to `x::Error::load`, and
/// that page says it is not exact. Without the `use`, the glob links it.
///
/// Control: make `Declarer::use_leaf` emit no row for an external named
/// `use` - `user::tests::loads` becomes a caller of `user::x::Error::load`.
#[test]
fn an_external_named_use_shadows_the_parents_glob_for_a_test_module() {
    let fixture =
        Fixture::new(&tests_calling(&glob_of_x_declaring("use std::fmt::Error;", "Error"), "Error"));
    let page = fixture.callers("user::x::Error::load");
    assert!(caller_names(&page).is_empty(), "{page}");
    assert!(page["unlinkedUsages"]["count"].as_u64().unwrap() >= 1, "{page}");

    let without_the_use = Fixture::new(&tests_calling(&glob_of_x_declaring("", "Error"), "Error"));
    assert_linked_callers(&without_the_use, "user::x::Error::load", &["user::tests::loads"]);
}

/// The parent's explicit `use crate::m::P;` shadows its `use self::x::*;`
/// that also provides a `P`: the test module's call links `m`'s `P`.
///
/// Control: drop the named-shadowing step of `Resolver::walk` (the
/// `hops.retain(|hop| hop.named)` block) - two answers, no caller.
#[test]
fn a_project_named_use_shadows_the_parents_glob_for_a_test_module() {
    let fixture = Fixture::new(&tests_calling(&glob_of_x_declaring("use crate::m::P;", "P"), "P"));
    assert_linked_callers(&fixture, "m::inner::P::load", &["user::tests::loads"]);
    assert!(caller_names(&fixture.callers("user::x::P::load")).is_empty());
}

fn path(segments: &[&str]) -> QualifiedPath {
    QualifiedPath(
        segments
            .iter()
            .enumerate()
            .map(|(i, name)| PathSegment { sep: (i > 0).then(|| "::".to_string()), name: name.to_string() })
            .collect(),
    )
}

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn declaration(conn: &mut Connection, id: &str, kind: &str, segments: &[&str]) -> NodeRecord {
    let mut node =
        NodeRecord::new(id, kind, *segments.last().unwrap(), segments.join("::"), "decl.rs", "rust");
    node.qualified_path = Some(path(segments));
    upsert_node(conn, node).unwrap();
    queries::get_node(conn, id).unwrap().unwrap()
}

/// A placeholder named `name` waiting on `key` (a one-segment key is a
/// `name` key), called once from a function in `file`.
fn unlinked_call(conn: &mut Connection, id: &str, file: &str, key: &[&str]) {
    call_onto_placeholder(conn, id, file, key, false);
}

/// [`unlinked_call`] with the edge's `resolved` bit as given.
fn call_onto_placeholder(conn: &mut Connection, id: &str, file: &str, key: &[&str], resolved: bool) {
    let caller = format!("{id}_caller");
    upsert_node(conn, NodeRecord::new(&caller, "Function", "c", format!("c::{id}"), file, "rust")).unwrap();
    let mut node = NodeRecord::new(
        id,
        "Module",
        *key.last().unwrap(),
        format!("nowhere::{}", key.join("::")),
        file,
        "rust",
    );
    node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
    node.target = Some(PlaceholderTargetRecord {
        scope_kind: "container".to_string(),
        scope: "nowhere".to_string(),
        key_kind: if key.len() > 1 { "qualifiedName" } else { "name" }.to_string(),
        key: key.join("::"),
        from_container: None,
        key_path: (key.len() > 1).then(|| path(key)),
    });
    upsert_node(conn, node).unwrap();
    upsert_edge(conn, EdgeRecord::new(format!("{id}_e"), &caller, id, "CALLS", "tree-sitter", resolved))
        .unwrap();
}

/// A plugin that marks an edge onto a placeholder `resolved: true` breaks the
/// wire contract, but ingest stores the bit as sent. The edge is still on the
/// placeholder, so the call is still unlinked and still counts.
#[test]
fn an_edge_left_on_a_placeholder_counts_whatever_its_resolved_bit_says() {
    let mut conn = setup();
    declaration(&mut conn, "ty", "Type", &["m", "P"]);
    let anchor = declaration(&mut conn, "load", "Function", &["m", "P", "load"]);
    call_onto_placeholder(&mut conn, "marked", "a.rs", &["x", "P", "load"], true);
    let stored: bool =
        conn.query_row("SELECT resolved FROM edges WHERE id = 'marked_e'", [], |row| row.get(0)).unwrap();
    assert!(stored, "the fixture must reach the index with resolved = 1");

    assert_eq!(probe(&conn, &anchor, &["CALLS"], &[]).expect("a candidate").count, 1);
}

#[test]
fn a_qualified_key_counts_only_when_its_type_segment_is_the_anchors_type() {
    let mut conn = setup();
    declaration(&mut conn, "ty", "Type", &["m", "P"]);
    let anchor = declaration(&mut conn, "load", "Function", &["m", "P", "load"]);
    unlinked_call(&mut conn, "same_type", "a.rs", &["x", "P", "load"]);
    unlinked_call(&mut conn, "other_type", "b.rs", &["x", "Q", "load"]);

    let found = probe(&conn, &anchor, &["CALLS"], &[]).expect("the same-type call is a candidate");
    assert_eq!(found.count, 1);
    assert_eq!(found.file_paths().collect::<Vec<_>>(), ["a.rs"]);
}

#[test]
fn a_bare_name_key_never_counts_for_a_type_member() {
    let mut conn = setup();
    declaration(&mut conn, "ty", "Type", &["m", "P"]);
    let anchor = declaration(&mut conn, "load", "Function", &["m", "P", "load"]);
    unlinked_call(&mut conn, "bare", "a.rs", &["load"]);

    assert!(probe(&conn, &anchor, &["CALLS"], &[]).is_none());
}

#[test]
fn a_bare_name_key_counts_for_a_free_function() {
    let mut conn = setup();
    let anchor = declaration(&mut conn, "wait", "Function", &["common", "wait_for"]);
    unlinked_call(&mut conn, "bare", "t.rs", &["wait_for"]);

    assert_eq!(probe(&conn, &anchor, &["CALLS"], &[]).expect("a candidate").count, 1);
}

#[test]
fn the_file_scope_and_the_edge_kinds_narrow_the_candidates() {
    let mut conn = setup();
    declaration(&mut conn, "ty", "Type", &["m", "P"]);
    let anchor = declaration(&mut conn, "load", "Function", &["m", "P", "load"]);
    unlinked_call(&mut conn, "in_a", "a.rs", &["x", "P", "load"]);

    assert!(probe(&conn, &anchor, &["CALLS"], &["other.rs"]).is_none(), "outside the scope");
    assert!(probe(&conn, &anchor, &["REFERENCES"], &[]).is_none(), "not a walked kind");
    assert_eq!(probe(&conn, &anchor, &["CALLS"], &["a.rs"]).expect("in scope").count, 1);
}

#[test]
fn the_file_tally_is_capped_while_the_count_stays_whole() {
    let mut conn = setup();
    declaration(&mut conn, "ty", "Type", &["m", "P"]);
    let anchor = declaration(&mut conn, "load", "Function", &["m", "P", "load"]);
    let over = MAX_UNLINKED_FILE_TALLY + 3;
    for i in 0..over {
        unlinked_call(&mut conn, &format!("p{i}"), &format!("f{i:02}.rs"), &["x", "P", "load"]);
    }

    let found = probe(&conn, &anchor, &["CALLS"], &[]).expect("candidates");
    assert_eq!(found.count, over);
    assert_eq!(found.files.len(), MAX_UNLINKED_FILE_TALLY);
    assert!(found.files_truncated);
}

/// The candidate lookup seeks the partial placeholder-name index instead of
/// scanning every node of the language. Control: drop the index, or bind
/// the placeholder kind as a parameter (the plan falls back to a scan).
#[test]
fn the_candidate_lookup_seeks_the_placeholder_name_index() {
    let conn = setup();
    let plan: Vec<String> = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {}", candidate_sql(3, 2)))
        .unwrap()
        .query_map(rusqlite::params_from_iter(["x"; 7]), |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(plan[0].contains("USING INDEX idx_nodes_pending_name (name=?)"), "{plan:?}");
}
