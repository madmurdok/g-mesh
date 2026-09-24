use std::collections::BTreeMap;

use super::*;
use crate::graph::imports::{EXTERNAL_MODULE_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND};
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::storage::schema;
use crate::storage::write::{apply_diff, EdgeRecord};

/// Both connection shapes this module has to be right on: `foreign_keys`
/// on, which is how every other storage test runs and which turns a
/// dangling edge into an error, and off, which is how the daemon actually
/// runs and where nothing cascades - so an explicit delete missed here
/// leaves an orphan behind instead of failing.
fn setup(foreign_keys: bool) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", if foreign_keys { "ON" } else { "OFF" }).unwrap();
    schema::apply(&conn).unwrap();
    conn
}

fn member(id: &str, language: &str, container: Option<&str>, parent: Option<&str>) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", id, id, format!("src/{id}.x"), language);
    node.container = container.map(str::to_string);
    node.container_parent = parent.map(str::to_string);
    node
}

fn upsert(nodes: Vec<NodeRecord>) -> Diff {
    Diff { upsert_nodes: nodes, ..Default::default() }
}

fn delete(ids: &[&str]) -> Diff {
    Diff { delete_node_ids: ids.iter().map(|id| id.to_string()).collect(), ..Default::default() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    language: String,
    key: String,
    node_id: String,
    parent_key: Option<String>,
    member_count: i64,
}

fn rows(conn: &Connection) -> Vec<Row> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT language, key, nodeId, parentKey, memberCount FROM containers ORDER BY language, key",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok(Row {
            language: row.get(0)?,
            key: row.get(1)?,
            node_id: row.get(2)?,
            parent_key: row.get(3)?,
            member_count: row.get(4)?,
        })
    })
    .unwrap()
    .collect::<rusqlite::Result<_>>()
    .unwrap()
}

fn row(conn: &Connection, language: &str, key: &str) -> Option<Row> {
    rows(conn).into_iter().find(|row| row.language == language && row.key == key)
}

/// The members a container's `DEFINES` edges reach, sorted.
fn edge_members(conn: &Connection, language: &str, key: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare_cached("SELECT toId FROM edges WHERE fromId = ?1 AND kind = 'DEFINES' ORDER BY toId")
        .unwrap();
    stmt.query_map(params![container_id(language, key)], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.prepare_cached(sql).unwrap().query_row([], |row| row.get(0)).unwrap()
}

/// Every invariant that holds after *any* sequence of diffs, checked
/// against the tables alone - no model of what the diffs meant. `Err`
/// names the first one broken.
fn check_invariants(conn: &Connection) -> Result<(), String> {
    for row in rows(conn) {
        if row.node_id != container_id(&row.language, &row.key) {
            return Err(format!("{row:?}: nodeId is not container_id(language, key)"));
        }
        let node: Option<(String, Option<String>, String, String, String, i64)> = conn
            .query_row(
                "SELECT kind, nativeKind, filePath, language, name, exported FROM nodes WHERE id = ?1",
                params![row.node_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()
            .unwrap();
        let expected = (
            MODULE_KIND.to_string(),
            Some(CONTAINER_NATIVE_KIND.to_string()),
            String::new(),
            row.language.clone(),
            row.key.clone(),
            1,
        );
        if node.as_ref() != Some(&expected) {
            return Err(format!("{row:?}: container node is {node:?}, expected {expected:?}"));
        }
        let defines = edge_members(conn, &row.language, &row.key).len() as i64;
        if row.member_count != defines {
            return Err(format!("{row:?}: memberCount != {defines} DEFINES edges"));
        }
        if row.member_count == 0 {
            return Err(format!("{row:?}: a container with zero members exists"));
        }
    }

    let orphan_nodes = count(
        conn,
        "SELECT COUNT(*) FROM nodes n WHERE n.nativeKind = 'container' \
         AND NOT EXISTS (SELECT 1 FROM containers c WHERE c.nodeId = n.id)",
    );
    if orphan_nodes != 0 {
        return Err(format!("{orphan_nodes} container node(s) without a containers row"));
    }

    // Every DEFINES edge out of a container lands on a node that really is
    // a member of exactly that container, under the id core computes...
    let mut stmt = conn
        .prepare_cached(
            "SELECT e.id, c.language, c.key, e.toId, n.language, n.container, n.nativeKind \
             FROM edges e JOIN containers c ON c.nodeId = e.fromId LEFT JOIN nodes n ON n.id = e.toId",
        )
        .unwrap();
    /// `(edge id, container language, container key, member id, and the
    /// member's own language/container/nativeKind - NULL if it is gone)`.
    type ContainerEdge = (String, String, String, String, Option<String>, Option<String>, Option<String>);
    let edges: Vec<ContainerEdge> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut attached: HashSet<(String, Key)> = HashSet::new();
    for (edge_id, language, key, to_id, n_language, n_container, n_native_kind) in edges {
        attached.insert((to_id.clone(), (language.clone(), key.clone())));
        let Some(n_language) = n_language else {
            return Err(format!(
                "DEFINES edge {edge_id} from {language}/{key} points at missing node {to_id}"
            ));
        };
        let actual = membership(&n_language, n_container.as_deref(), n_native_kind.as_deref());
        if actual != Some((language.clone(), key.clone())) {
            return Err(format!(
                "edge from {language}/{key} reaches {to_id}, whose membership is {actual:?}"
            ));
        }
        if edge_id != defines_edge_id(&container_id(&language, &key), &to_id) {
            return Err(format!("edge {edge_id} into {to_id} does not carry the computed id"));
        }
    }

    // ...and every member has one.
    let mut stmt = conn.prepare_cached("SELECT id, language, container, nativeKind FROM nodes").unwrap();
    let nodes: Vec<(String, String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for (id, language, container, native_kind) in nodes {
        let Some(key) = membership(&language, container.as_deref(), native_kind.as_deref()) else {
            continue;
        };
        if !attached.contains(&(id.clone(), key.clone())) {
            return Err(format!("member {id} of {key:?} has no DEFINES edge from a live container"));
        }
    }

    let dangling = count(
        conn,
        "SELECT COUNT(*) FROM edges e WHERE NOT EXISTS (SELECT 1 FROM nodes n WHERE n.id = e.fromId) \
         OR NOT EXISTS (SELECT 1 FROM nodes n WHERE n.id = e.toId)",
    );
    if dangling != 0 {
        return Err(format!("{dangling} edge(s) with a missing endpoint"));
    }
    Ok(())
}

#[test]
fn a_first_member_materializes_its_container() {
    let mut conn = setup(true);
    apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("github.com/x/app"), None)])).unwrap();

    assert_eq!(
        rows(&conn),
        vec![Row {
            language: "go".to_string(),
            key: "github.com/x/app".to_string(),
            node_id: container_id("go", "github.com/x/app"),
            parent_key: None,
            member_count: 1,
        }]
    );
    assert_eq!(edge_members(&conn, "go", "github.com/x/app"), vec!["a"]);
    let (source, engine, resolved): (String, String, bool) = conn
        .query_row("SELECT source, engine, resolved FROM edges WHERE kind = 'DEFINES'", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!((source.as_str(), engine.as_str(), resolved), ("syntactic", CORE_ENGINE, true));
    check_invariants(&conn).unwrap();
}

/// Idempotency, the acceptance criterion's first trap: an already-member
/// node upserted again with the same container - which every reparse that
/// touches it does - must not count it twice.
#[test]
fn re_upserting_a_member_does_not_count_it_again() {
    let mut conn = setup(true);
    for _ in 0..3 {
        apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("pkg"), None)])).unwrap();
    }
    assert_eq!(row(&conn, "go", "pkg").unwrap().member_count, 1);
    check_invariants(&conn).unwrap();
}

/// The second trap: a plugin re-sends a file with only what changed, so an
/// unchanged member is absent from the diff - and absent is not deleted.
#[test]
fn a_file_re_sent_without_an_unchanged_member_keeps_it() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![member("a", "go", Some("pkg"), None), member("b", "go", Some("pkg"), None)]),
    )
    .unwrap();

    let mut changed = member("a", "go", Some("pkg"), None);
    changed.signature = Some("func a(x int)".to_string());
    apply_diff(&mut conn, &upsert(vec![changed])).unwrap();

    assert_eq!(row(&conn, "go", "pkg").unwrap().member_count, 2);
    assert_eq!(edge_members(&conn, "go", "pkg"), vec!["a", "b"]);
    check_invariants(&conn).unwrap();
}

#[test]
fn a_move_leaves_the_old_container_and_joins_the_new_one_in_the_same_diff() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![member("a", "go", Some("old"), None), member("b", "go", Some("old"), None)]),
    )
    .unwrap();

    apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("new"), None)])).unwrap();

    assert_eq!(row(&conn, "go", "old").unwrap().member_count, 1);
    assert_eq!(row(&conn, "go", "new").unwrap().member_count, 1);
    assert_eq!(edge_members(&conn, "go", "old"), vec!["b"]);
    assert_eq!(edge_members(&conn, "go", "new"), vec!["a"]);
    check_invariants(&conn).unwrap();
}

/// Moves "counted exactly once": a swap is two moves in one diff, and the
/// same id upserted twice in one diff (a burst) is one move, to wherever
/// its last record puts it.
#[test]
fn swaps_and_repeated_records_in_one_diff_count_each_member_once() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![member("a", "go", Some("left"), None), member("b", "go", Some("right"), None)]),
    )
    .unwrap();

    apply_diff(
        &mut conn,
        &upsert(vec![
            member("a", "go", Some("right"), None),
            member("b", "go", Some("left"), None),
            member("b", "go", Some("elsewhere"), None),
            member("b", "go", Some("left"), None),
        ]),
    )
    .unwrap();

    assert_eq!(edge_members(&conn, "go", "left"), vec!["b"]);
    assert_eq!(edge_members(&conn, "go", "right"), vec!["a"]);
    assert_eq!(row(&conn, "go", "elsewhere"), None, "a container only a superseded record named");
    assert_eq!(row(&conn, "go", "left").unwrap().member_count, 1);
    assert_eq!(row(&conn, "go", "right").unwrap().member_count, 1);
    check_invariants(&conn).unwrap();
}

/// A container that one diff empties and refills never reaches zero, so
/// it is never deleted - and so nothing pointing at it loses its edge.
#[test]
fn a_container_emptied_and_refilled_in_one_diff_survives_with_its_incoming_edges() {
    for foreign_keys in [true, false] {
        let mut conn = setup(foreign_keys);
        apply_diff(&mut conn, &upsert(vec![member("a", "go", Some("pkg"), None)])).unwrap();
        let importer = importer_of(&mut conn, "go", "pkg");

        let mut diff = delete(&["a"]);
        diff.upsert_nodes.push(member("b", "go", Some("pkg"), None));
        apply_diff(&mut conn, &diff).unwrap();

        assert_eq!(edge_members(&conn, "go", "pkg"), vec!["b"]);
        assert_eq!(count(&conn, &format!("SELECT COUNT(*) FROM edges WHERE id = '{importer}'")), 1);
        check_invariants(&conn).unwrap();
    }
}

/// Stands in for GM-267's `File -IMPORTS-> container` edge: a file node
/// and an edge into the container node, written the way any diff writes
/// them. Returns the edge id.
fn importer_of(conn: &mut Connection, language: &str, key: &str) -> String {
    let file = NodeRecord::new("importer", "File", "main.go", "main.go", "main.go", language);
    let edge =
        EdgeRecord::new("imports-pkg", "importer", container_id(language, key), "IMPORTS", "syntactic", true);
    apply_diff(conn, &Diff { upsert_nodes: vec![file], upsert_edges: vec![edge], ..Default::default() })
        .unwrap();
    "imports-pkg".to_string()
}

#[test]
fn deleting_the_last_member_deletes_the_container_and_every_edge_into_it() {
    for foreign_keys in [true, false] {
        let mut conn = setup(foreign_keys);
        apply_diff(
            &mut conn,
            &upsert(vec![member("a", "go", Some("pkg"), None), member("b", "go", Some("pkg"), None)]),
        )
        .unwrap();
        let importer = importer_of(&mut conn, "go", "pkg");

        apply_diff(&mut conn, &delete(&["a"])).unwrap();
        assert_eq!(row(&conn, "go", "pkg").unwrap().member_count, 1, "fk={foreign_keys}");

        apply_diff(&mut conn, &delete(&["b"])).unwrap();
        assert_eq!(rows(&conn), vec![], "fk={foreign_keys}");
        let node_id = container_id("go", "pkg");
        assert_eq!(count(&conn, &format!("SELECT COUNT(*) FROM nodes WHERE id = '{node_id}'")), 0);
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM edges WHERE fromId = '{node_id}' OR toId = '{node_id}'")
            ),
            0,
            "fk={foreign_keys}: DEFINES out of it and {importer} into it must both go"
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM nodes WHERE id = 'importer'"), 1, "the importer stays");
        check_invariants(&conn).unwrap();
    }
}

/// A placeholder is an address, not a declaration: counting one would
/// keep a linked-away `resolved_module` alive forever (its `DEFINES`
/// edge is an incident edge `graph::imports` waits to see gone).
///
/// `external_module` is in this list too (GM-376) even though no shipped
/// plugin ever sends one with a non-empty `container` - the case is only
/// reachable the way this test reaches it, by constructing the
/// `NodeRecord` directly rather than through a plugin's wire output. It
/// is the arm that fails without GM-376's change to [`membership`]: on
/// the four-kind copy `membership` used to spell, an `external_module`
/// row here was still counted, so `containers.memberCount` would disagree
/// with `graph::symbol_links::requesters_below_new_containers`'s own
/// count for it - the coupling that function's own comment names. The
/// other four kinds fail without any filter at all, so they are not the
/// control for this change; see
/// `a_module_that_is_not_an_address_is_still_a_member` for that.
#[test]
fn placeholders_and_empty_keys_are_never_members() {
    let mut conn = setup(true);
    let mut nodes = Vec::new();
    for native_kind in [
        PENDING_SYMBOL_NATIVE_KIND,
        REEXPORT_NATIVE_KIND,
        RESOLVED_MODULE_NATIVE_KIND,
        EXTERNAL_MODULE_NATIVE_KIND,
    ] {
        let mut placeholder = member(native_kind, "go", Some("pkg"), None);
        placeholder.kind = MODULE_KIND.to_string();
        placeholder.native_kind = Some(native_kind.to_string());
        nodes.push(placeholder);
    }
    nodes.push(member("empty", "go", Some(""), None));
    apply_diff(&mut conn, &upsert(nodes)).unwrap();

    assert_eq!(rows(&conn), vec![]);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM edges"), 0);
    check_invariants(&conn).unwrap();
}

/// The control for the test above: a `Module`-kind node that is not one
/// of the excluded native kinds - a TypeScript namespace is the real
/// example (`plugins/typescript/src/extract.ts` emits `kind: "Module",
/// nativeKind: "namespace"`) - is still a member, so the exclusion above
/// is about `nativeKind`, not about `kind == "Module"`.
#[test]
fn a_module_that_is_not_an_address_is_still_a_member() {
    let mut conn = setup(true);
    let mut namespace = member("ns", "typescript", Some("pkg"), None);
    namespace.kind = MODULE_KIND.to_string();
    namespace.native_kind = Some("namespace".to_string());
    apply_diff(&mut conn, &upsert(vec![namespace])).unwrap();

    assert_eq!(row(&conn, "typescript", "pkg").unwrap().member_count, 1);
    check_invariants(&conn).unwrap();
}

#[test]
fn the_same_key_in_two_languages_is_two_containers() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![member("a", "go", Some("shared"), None), member("b", "rust", Some("shared"), None)]),
    )
    .unwrap();
    assert_ne!(container_id("go", "shared"), container_id("rust", "shared"));
    assert_eq!(rows(&conn).len(), 2);
    apply_diff(&mut conn, &delete(&["a"])).unwrap();
    assert_eq!(row(&conn, "go", "shared"), None);
    assert_eq!(row(&conn, "rust", "shared").unwrap().member_count, 1);
    check_invariants(&conn).unwrap();
}

/// The id is read back from the documented preimage rather than written
/// down as a constant, so this checks the scheme, not a copy of its output.
#[test]
fn container_ids_hash_a_marked_preimage_in_the_plugin_id_family() {
    let expected: String = Sha256::digest(b"container go\0github.com/x/app")
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(container_id("go", "github.com/x/app"), expected);
    assert_eq!(expected.len(), 32, "same truncation as extract.ts's hash()");
    // The NUL separator is what keeps these apart; a space would not.
    assert_ne!(container_id("a b", "c"), container_id("a", "b c"));
    // The marker is what keeps a container id off every plugin node id:
    // extract.ts hashes `node <path> <kind> <qualifiedName> <nativeKind>`.
    let plugin_style: String = Sha256::digest(b"node  Module github.com/x/app container")
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_ne!(container_id("go", "github.com/x/app"), plugin_style);
}

/// TS sends no container, and its behaviour must not change: no rows, no
/// edges, no nodes beyond the diff's own, and the fast path taken.
#[test]
fn a_diff_with_no_containers_writes_nothing_extra_and_skips_the_hook() {
    let mut conn = setup(true);
    let diff = Diff {
        upsert_nodes: vec![
            NodeRecord::new("f", "File", "a.ts", "a.ts", "a.ts", "typescript"),
            NodeRecord::new("n", "Function", "run", "run", "a.ts", "typescript"),
        ],
        upsert_edges: vec![EdgeRecord::new("e", "f", "n", "DEFINES", "tree-sitter", false)],
        ..Default::default()
    };
    assert!(detach(&conn, &diff).unwrap().is_none());
    apply_diff(&mut conn, &diff).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM nodes"), 2);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM edges"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM containers"), 0);
    assert!(detach(&conn, &delete(&["n"])).unwrap().is_none());
}

#[test]
fn conflicting_parents_keep_the_last_one_sent() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![
            member("a", "rust", Some("c::m"), Some("c")),
            member("b", "rust", Some("c::m"), Some("bogus")),
        ]),
    )
    .unwrap();
    assert_eq!(row(&conn, "rust", "c::m").unwrap().parent_key.as_deref(), Some("bogus"));

    apply_diff(&mut conn, &upsert(vec![member("a", "rust", Some("c::m"), Some("c"))])).unwrap();
    assert_eq!(row(&conn, "rust", "c::m").unwrap().parent_key.as_deref(), Some("c"));

    // `None` is a value too: the container is now recorded as a root.
    apply_diff(&mut conn, &upsert(vec![member("b", "rust", Some("c::m"), None)])).unwrap();
    assert_eq!(row(&conn, "rust", "c::m").unwrap().parent_key, None);
    check_invariants(&conn).unwrap();
}

#[test]
fn parent_chain_walks_to_the_root_nearest_first() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![
            member("root", "rust", Some("krate"), None),
            member("mid", "rust", Some("krate::a"), Some("krate")),
            member("leaf", "rust", Some("krate::a::b"), Some("krate::a")),
            // Same keys in another language must not leak into the walk.
            member("go", "go", Some("krate::a"), Some("elsewhere")),
        ]),
    )
    .unwrap();

    assert_eq!(parent_chain(&conn, "rust", "krate::a::b").unwrap(), vec!["krate::a", "krate"]);
    assert_eq!(parent_chain(&conn, "rust", "krate").unwrap(), Vec::<String>::new());
    assert_eq!(parent_chain(&conn, "rust", "never-seen").unwrap(), Vec::<String>::new());
}

/// An ancestor with no members has no row, so nothing records *its*
/// parent: the walk names the gap (the row below it said so) and stops,
/// rather than guessing by splitting the key.
#[test]
fn parent_chain_names_a_memberless_ancestor_and_stops_there() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![
            member("root", "rust", Some("krate"), None),
            member("leaf", "rust", Some("krate::a::b"), Some("krate::a")),
        ]),
    )
    .unwrap();
    assert_eq!(parent_chain(&conn, "rust", "krate::a::b").unwrap(), vec!["krate::a"]);

    // The gap closes as soon as the ancestor gains a member.
    apply_diff(&mut conn, &upsert(vec![member("mid", "rust", Some("krate::a"), Some("krate"))])).unwrap();
    assert_eq!(parent_chain(&conn, "rust", "krate::a::b").unwrap(), vec!["krate::a", "krate"]);
}

#[test]
fn parent_chain_ends_at_a_cycle() {
    let mut conn = setup(true);
    apply_diff(
        &mut conn,
        &upsert(vec![member("x", "rust", Some("x"), Some("y")), member("y", "rust", Some("y"), Some("x"))]),
    )
    .unwrap();
    assert_eq!(parent_chain(&conn, "rust", "x").unwrap(), vec!["y"]);
}

/// The daemon's own write path: a real `FileChangeDiff`-shaped wire node
/// goes through `watcher::apply::to_node_record`, which must carry
/// `containerParent` through rather than drop it as it did before GM-265.
#[test]
fn the_wire_conversion_carries_container_parent_through_to_the_row() {
    use crate::protocol::types::{NodeKind, Position, Range, Visibility, WireNode};
    let wire = WireNode {
        id: "w".to_string(),
        kind: NodeKind::Function,
        name: "Run".to_string(),
        qualified_name: "Run".to_string(),
        file_path: "server/run.go".to_string(),
        range: Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } },
        signature: None,
        visibility: Visibility::Public,
        doc_comment: None,
        language: "go".to_string(),
        native_kind: None,
        has_syntax_errors: false,
        declarations: None,
        container: Some("github.com/x/app/server".to_string()),
        container_parent: Some("github.com/x/app".to_string()),
        target: None,
    };
    let mut conn = setup(false);
    apply_diff(&mut conn, &upsert(vec![crate::watcher::apply::to_node_record(wire)])).unwrap();
    assert_eq!(
        row(&conn, "go", "github.com/x/app/server").unwrap().parent_key.as_deref(),
        Some("github.com/x/app")
    );
}

// ---------------------------------------------------------------------
// Sequence test: random diffs against a model, invariants after each.
// ---------------------------------------------------------------------

/// splitmix64: deterministic, dependency-free, and good enough to spread
/// choices over a few dozen options. `proptest` is not a dependency of
/// this crate, and a seed printed on failure plus
/// [`SEED_ENV`] to replay it is all the shrinking this needs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn percent(&mut self, p: u64) -> bool {
        self.next() % 100 < p
    }
}

/// Replays one seed instead of the built-in set: `G_MESH_CONTAINERS_SEED=
/// 0x1234 cargo test ... containers::tests::`.
const SEED_ENV: &str = "G_MESH_CONTAINERS_SEED";

fn seeds(default: &[u64]) -> Vec<u64> {
    match std::env::var(SEED_ENV) {
        Ok(value) => {
            let value = value.trim();
            let parsed = match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16),
                None => value.parse(),
            };
            vec![parsed.unwrap_or_else(|_| panic!("{SEED_ENV}={value} is not a u64"))]
        }
        Err(_) => default.to_vec(),
    }
}

const LANGUAGES: [&str; 2] = ["go", "rust"];
/// Keys and their true parents. A space in one key, a three-deep chain,
/// and two roots.
const KEYS: [(&str, Option<&str>); 5] = [
    ("app", None),
    ("app/srv", Some("app")),
    ("app/srv/http", Some("app/srv")),
    ("lib", None),
    ("lib util", Some("lib")),
];
const FILES: usize = 6;
const SLOTS: usize = 5;

#[derive(Debug, Clone, PartialEq)]
struct Spec {
    language: String,
    container: Option<String>,
    parent: Option<String>,
    native_kind: Option<String>,
    signature: u64,
}

impl Spec {
    fn random(rng: &mut Rng, file: usize) -> Self {
        let (key, parent) = KEYS[rng.below(KEYS.len())];
        Spec {
            // Mostly the file's own language; sometimes the other one, so
            // an id changing language is exercised too.
            language: LANGUAGES[if rng.percent(90) { file % 2 } else { (file + 1) % 2 }].to_string(),
            container: if rng.percent(85) { Some(key.to_string()) } else { None },
            parent: if rng.percent(92) { parent.map(str::to_string) } else { Some("bogus".to_string()) },
            native_kind: if rng.percent(10) { Some(PENDING_SYMBOL_NATIVE_KIND.to_string()) } else { None },
            signature: rng.next() % 3,
        }
    }

    fn membership(&self) -> Option<Key> {
        membership(&self.language, self.container.as_deref(), self.native_kind.as_deref())
    }
}

fn node_id(file: usize, slot: usize) -> String {
    format!("f{file}s{slot}")
}
fn file_node_id(file: usize) -> String {
    format!("file{file}")
}
fn plugin_edge_id(file: usize, slot: usize) -> String {
    format!("pe{file}s{slot}")
}

fn record(file: usize, slot: usize, spec: &Spec) -> NodeRecord {
    let id = node_id(file, slot);
    let kind = if spec.native_kind.is_some() { MODULE_KIND } else { "Function" };
    let mut node = NodeRecord::new(&id, kind, &id, &id, format!("src/f{file}.x"), &spec.language);
    node.container = spec.container.clone();
    node.container_parent = spec.parent.clone();
    node.native_kind = spec.native_kind.clone();
    node.signature = Some(format!("sig {}", spec.signature));
    node
}

fn file_record(file: usize) -> NodeRecord {
    NodeRecord::new(file_node_id(file), "File", format!("f{file}.x"), "", format!("src/f{file}.x"), "go")
}

fn plugin_edge(file: usize, slot: usize) -> EdgeRecord {
    EdgeRecord::new(
        plugin_edge_id(file, slot),
        file_node_id(file),
        node_id(file, slot),
        "DEFINES",
        "tree-sitter",
        true,
    )
}

/// What a plugin would believe about the project, and what the index
/// should therefore hold.
#[derive(Default)]
struct World {
    files: Vec<Option<BTreeMap<usize, Spec>>>,
    /// `(language, key) -> parent`, last writer wins, dropped at zero.
    parents: HashMap<Key, Option<String>>,
}

impl World {
    fn new() -> Self {
        World { files: vec![None; FILES], parents: HashMap::new() }
    }

    /// Records the parent claims of `diff`'s member records, in order,
    /// then forgets the parents of containers left with no members.
    fn note_parents(&mut self, diff: &Diff) {
        for node in &diff.upsert_nodes {
            if let Some(key) = record_membership(node) {
                self.parents.insert(key, node.container_parent.clone());
            }
        }
        let live = self.expected_members();
        self.parents.retain(|key, _| live.contains_key(key));
    }

    fn expected_members(&self) -> BTreeMap<Key, Vec<String>> {
        let mut members: BTreeMap<Key, Vec<String>> = BTreeMap::new();
        for (file, slots) in self.files.iter().enumerate() {
            for (slot, spec) in slots.iter().flatten() {
                if let Some(key) = spec.membership() {
                    members.entry(key).or_default().push(node_id(file, *slot));
                }
            }
        }
        for ids in members.values_mut() {
            ids.sort();
        }
        members
    }

    fn check(&self, conn: &Connection) -> Result<(), String> {
        check_invariants(conn)?;
        let expected = self.expected_members();
        let actual: BTreeMap<Key, Vec<String>> = rows(conn)
            .into_iter()
            .map(|row| {
                let members = edge_members(conn, &row.language, &row.key);
                ((row.language, row.key), members)
            })
            .collect();
        if actual != expected {
            return Err(format!(
                "membership differs from the model:\n  index: {actual:?}\n  model: {expected:?}"
            ));
        }
        for row in rows(conn) {
            let key = (row.language.clone(), row.key.clone());
            let expected_parent = self.parents.get(&key).cloned().flatten();
            if row.parent_key != expected_parent {
                return Err(format!("{row:?}: parentKey should be {expected_parent:?}"));
            }
        }
        let plugin_edges = count(conn, "SELECT COUNT(*) FROM edges WHERE engine = 'tree-sitter'") as usize;
        let expected_edges: usize = self.files.iter().flatten().map(BTreeMap::len).sum();
        if plugin_edges != expected_edges {
            return Err(format!("{plugin_edges} plugin edges in the index, {expected_edges} in the model"));
        }
        Ok(())
    }

    /// A plugin's reparse of `file`: slots appear, disappear, change
    /// container, kind or language, and only what changed is sent - plus
    /// a random share of unchanged nodes re-sent anyway, which a real
    /// plugin does whenever something upstream of the symbol moved.
    fn edit(&mut self, rng: &mut Rng, file: usize) -> Diff {
        let old = self.files[file].clone().unwrap_or_default();
        let mut new = old.clone();
        for slot in 0..SLOTS {
            match new.get(&slot).cloned() {
                None => {
                    if rng.percent(45) {
                        new.insert(slot, Spec::random(rng, file));
                    }
                }
                Some(mut spec) => match rng.below(100) {
                    0..=17 => {
                        new.remove(&slot);
                    }
                    18..=47 => {
                        let fresh = Spec::random(rng, file);
                        spec.container = fresh.container;
                        spec.parent = fresh.parent;
                        new.insert(slot, spec);
                    }
                    48..=52 => {
                        spec.native_kind = match spec.native_kind {
                            Some(_) => None,
                            None => Some(PENDING_SYMBOL_NATIVE_KIND.to_string()),
                        };
                        new.insert(slot, spec);
                    }
                    53..=55 => {
                        spec.language = if spec.language == "go" { "rust" } else { "go" }.to_string();
                        new.insert(slot, spec);
                    }
                    56..=65 => {
                        spec.signature += 1;
                        new.insert(slot, spec);
                    }
                    _ => {}
                },
            }
        }

        let mut diff = Diff::default();
        diff.upsert_nodes.push(file_record(file));
        for (slot, spec) in &new {
            let before = old.get(slot);
            if before != Some(spec) || rng.percent(25) {
                diff.upsert_nodes.push(record(file, *slot, spec));
            }
            if before.is_none() || rng.percent(10) {
                diff.upsert_edges.push(plugin_edge(file, *slot));
            }
        }
        for slot in old.keys().filter(|slot| !new.contains_key(slot)) {
            diff.delete_node_ids.push(node_id(file, *slot));
            diff.delete_edge_ids.push(plugin_edge_id(file, *slot));
        }
        self.files[file] = Some(new);
        diff
    }

    fn delete_file(&mut self, file: usize) -> Diff {
        let mut diff = Diff::default();
        if let Some(slots) = self.files[file].take() {
            for slot in slots.keys() {
                diff.delete_node_ids.push(node_id(file, *slot));
                diff.delete_edge_ids.push(plugin_edge_id(file, *slot));
            }
            diff.delete_node_ids.push(file_node_id(file));
        }
        diff
    }

    /// Rotates the containers of `file`'s nodes one step, so every member
    /// moves at once and several containers each lose one member and gain
    /// another inside a single diff.
    fn rotate(&mut self, file: usize) -> Diff {
        let mut diff = Diff::default();
        let Some(slots) = self.files[file].as_mut() else { return diff };
        let containers: Vec<(Option<String>, Option<String>)> =
            slots.values().map(|spec| (spec.container.clone(), spec.parent.clone())).collect();
        if containers.len() < 2 {
            return diff;
        }
        for (i, (slot, spec)) in slots.iter_mut().enumerate() {
            let (container, parent) = containers[(i + 1) % containers.len()].clone();
            spec.container = container;
            spec.parent = parent;
            diff.upsert_nodes.push(record(file, *slot, spec));
        }
        diff
    }

    /// One id sent several times in one diff - first to a random container,
    /// then to its final one - plus a delete-and-re-add of another id:
    /// both shapes a burst of merged diffs produces.
    fn repeat_records(&mut self, rng: &mut Rng, file: usize) -> Diff {
        let mut diff = Diff::default();
        let Some(slots) = self.files[file].as_mut() else { return diff };
        let present: Vec<usize> = slots.keys().copied().collect();
        if present.is_empty() {
            return diff;
        }
        let slot = present[rng.below(present.len())];
        let interim = Spec::random(rng, file);
        diff.upsert_nodes.push(record(file, slot, &interim));
        let fin = Spec::random(rng, file);
        diff.upsert_nodes.push(record(file, slot, &fin));
        slots.insert(slot, fin);

        let other = present[rng.below(present.len())];
        if other != slot {
            let spec = slots[&other].clone();
            diff.delete_edge_ids.push(plugin_edge_id(file, other));
            diff.delete_node_ids.push(node_id(file, other));
            diff.upsert_nodes.push(record(file, other, &spec));
            diff.upsert_edges.push(plugin_edge(file, other));
        }
        diff
    }
}

fn merge(mut into: Diff, other: Diff) -> Diff {
    into.upsert_nodes.extend(other.upsert_nodes);
    into.delete_node_ids.extend(other.delete_node_ids);
    into.upsert_edges.extend(other.upsert_edges);
    into.delete_edge_ids.extend(other.delete_edge_ids);
    into
}

/// 8 seeds x 250 diffs x both foreign-key settings = 4,000 diffs, each
/// followed by a full invariant check. Sized by what it exercises rather
/// than by a round number: the run prints how many containers it created
/// and garbage-collected, and asserts both are well into the hundreds.
const SEQUENCE_SEEDS: [u64; 8] = [1, 2, 3, 0xC0FFEE, 0xDEAD_BEEF, 42, 1729, 0x5EED_0265];
const SEQUENCE_STEPS: usize = 250;

/// The acceptance criterion: over hundreds of random upsert/delete/move
/// diffs across several files, containers and two languages, after every
/// single diff, `memberCount` equals the container's `DEFINES` edges,
/// no empty container exists, and membership and parents equal what a
/// model of the plugin's own view says they should be - on connections
/// with foreign keys both on and off.
#[test]
fn membership_invariants_hold_after_every_diff_of_a_random_sequence() {
    let (mut diffs, mut materialized, mut collected) = (0usize, 0usize, 0usize);
    for seed in seeds(&SEQUENCE_SEEDS) {
        for foreign_keys in [true, false] {
            let mut conn = setup(foreign_keys);
            let mut rng = Rng(seed);
            let mut world = World::new();
            for step in 0..SEQUENCE_STEPS {
                let file = rng.below(FILES);
                let (op, diff) = match rng.below(100) {
                    0..=49 => ("edit", world.edit(&mut rng, file)),
                    50..=59 => ("delete file", world.delete_file(file)),
                    60..=74 => {
                        let second = (file + 1 + rng.below(FILES - 1)) % FILES;
                        let first = world.edit(&mut rng, file);
                        ("burst of two files", merge(first, world.edit(&mut rng, second)))
                    }
                    75..=87 => ("rotate containers", world.rotate(file)),
                    _ => ("repeated records", world.repeat_records(&mut rng, file)),
                };
                world.note_parents(&diff);
                let context = format!(
                    "seed {seed:#x}, step {step} ({op} on f{file}), foreign_keys={foreign_keys} - replay \
                     with {SEED_ENV}={seed:#x}"
                );
                let before: BTreeSet<String> = rows(&conn).into_iter().map(|row| row.node_id).collect();
                if let Err(err) = apply_diff(&mut conn, &diff) {
                    panic!("{context}: apply_diff failed: {err:#}");
                }
                if let Err(message) = world.check(&conn) {
                    panic!("{context}: {message}");
                }
                let after: BTreeSet<String> = rows(&conn).into_iter().map(|row| row.node_id).collect();
                materialized += after.difference(&before).count();
                collected += before.difference(&after).count();
                diffs += 1;
            }
        }
    }
    // A sequence that never creates or empties a container proves
    // nothing about either; make sure this one does both, a lot.
    eprintln!(
        "containers sequence test: {diffs} diffs, {materialized} containers materialized, {collected} collected"
    );
    assert!(materialized > 100 && collected > 100, "{materialized} materialized, {collected} collected");
}

// ---------------------------------------------------------------------
// Batch boundaries.
// ---------------------------------------------------------------------

enum Item {
    Node(usize, usize, Spec),
    File(usize),
    Edge(usize, usize),
}

/// A bulk stream in a plugin's own order - each file's `File` node, its
/// declarations, then its edges - followed by a second pass that re-sends
/// some declarations with a different container or parent, so a cut can
/// fall between an id's two records as well as between a container's
/// members.
fn bulk_stream(rng: &mut Rng) -> Vec<Item> {
    let mut items = Vec::new();
    let mut sent = Vec::new();
    for file in 0..FILES {
        items.push(Item::File(file));
        let slots: Vec<usize> = (0..SLOTS).filter(|_| rng.percent(80)).collect();
        for slot in &slots {
            let spec = Spec::random(rng, file);
            sent.push((file, *slot, spec.clone()));
            items.push(Item::Node(file, *slot, spec));
        }
        for slot in slots {
            items.push(Item::Edge(file, slot));
        }
    }
    for (file, slot, mut spec) in sent {
        if rng.percent(30) {
            let fresh = Spec::random(rng, file);
            spec.container = fresh.container;
            spec.parent = fresh.parent;
            items.push(Item::Node(file, slot, spec));
        }
    }
    items
}

fn batch(items: &[Item]) -> Diff {
    let mut diff = Diff::default();
    for item in items {
        match item {
            Item::Node(file, slot, spec) => diff.upsert_nodes.push(record(*file, *slot, spec)),
            Item::File(file) => diff.upsert_nodes.push(file_record(*file)),
            Item::Edge(file, slot) => diff.upsert_edges.push(plugin_edge(*file, *slot)),
        }
    }
    diff
}

/// Everything the index holds, as sorted text: the whole `containers`
/// table, every node and every edge.
fn snapshot(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    for sql in [
        "SELECT language || '|' || key || '|' || nodeId || '|' || IFNULL(parentKey, '<null>') || '|' || memberCount FROM containers ORDER BY 1",
        "SELECT id || '|' || kind || '|' || name || '|' || filePath || '|' || language || '|' || IFNULL(nativeKind, '') || '|' || IFNULL(container, '') || '|' || visibility FROM nodes ORDER BY 1",
        "SELECT id || '|' || fromId || '|' || toId || '|' || kind || '|' || source || '|' || engine || '|' || resolved FROM edges ORDER BY 1",
    ] {
        let mut stmt = conn.prepare_cached(sql).unwrap();
        let rows: Vec<String> =
            stmt.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
        out.extend(rows);
        out.push("--".to_string());
    }
    out
}

fn apply_cut(stream: &[Item], cuts: &[usize], foreign_keys: bool) -> Vec<String> {
    let mut conn = setup(foreign_keys);
    let mut start = 0;
    for &end in cuts.iter().chain(std::iter::once(&stream.len())) {
        apply_diff(&mut conn, &batch(&stream[start..end])).unwrap();
        start = end;
    }
    check_invariants(&conn).unwrap();
    snapshot(&conn)
}

/// `daemon::bulk_index::commit` is one `apply_diff` per batch, and the
/// batch can end anywhere. The same stream committed whole, one item at a
/// time, in fixed-size chunks and at random cut points must leave
/// byte-identical containers, nodes and edges.
#[test]
fn bulk_batch_boundaries_do_not_change_the_result() {
    for seed in seeds(&[11, 12, 13, 0xB01C]) {
        let mut rng = Rng(seed);
        let stream = bulk_stream(&mut rng);
        let whole = apply_cut(&stream, &[], true);

        // The comparison has to be able to fail: the same stream minus
        // its re-sent tail is a different index.
        let first_pass =
            stream.iter().position(|item| matches!(item, Item::File(f) if *f == FILES - 1)).unwrap();
        let tail_start = stream
            .iter()
            .enumerate()
            .skip(first_pass)
            .find(|(_, item)| matches!(item, Item::Node(f, _, _) if *f < FILES - 1))
            .map(|(i, _)| i);
        if let Some(tail_start) = tail_start {
            assert_ne!(
                apply_cut(&stream[..tail_start], &[], true),
                whole,
                "seed {seed:#x}: the snapshot cannot tell two different streams apart"
            );
        }
        assert!(whole.iter().any(|line| line.contains("|container|")), "seed {seed:#x}: no container at all");

        let mut cut_sets: Vec<Vec<usize>> = Vec::new();
        for size in [1, 2, 3, 5, 8, 13] {
            cut_sets.push((size..stream.len()).step_by(size).collect());
        }
        for _ in 0..25 {
            let mut cuts: Vec<usize> =
                (0..1 + rng.below(6)).map(|_| 1 + rng.below(stream.len() - 1)).collect();
            cuts.sort_unstable();
            cuts.dedup();
            cut_sets.push(cuts);
        }
        for cuts in cut_sets {
            for foreign_keys in [true, false] {
                assert_eq!(
                    apply_cut(&stream, &cuts, foreign_keys),
                    whole,
                    "seed {seed:#x}, cuts {cuts:?}, foreign_keys={foreign_keys}: replay with {SEED_ENV}={seed:#x}"
                );
            }
        }
    }
}
