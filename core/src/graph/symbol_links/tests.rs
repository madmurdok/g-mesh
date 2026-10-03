use super::*;
use crate::storage::schema;
use crate::storage::write::{apply_diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    // On, so that an edge left pointing at a node that is not there - the
    // exact failure this module exists to prevent - is a hard error here
    // rather than a silently dangling row.
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    conn
}

/// The replacement for GM-367's pinning test, which asserted that this
/// filter and the lookups' differed by exactly `external_module`. GM-372
/// removed the difference (see [`is_declaration`]), so what is worth
/// pinning is no longer the size of a gap between two lists - there is
/// one list now - but that the linker actually refuses every kind on it,
/// through the SQL the candidate lookup really runs.
///
/// Each kind gets a `public` node in the target file under the name the
/// placeholder is waiting for, which is the strongest form of the trap: a
/// visible, exactly-named, kind-compatible row that a `REFERENCES` edge
/// would land on if the `nativeKind` filter did not stop it. No shipped
/// plugin emits an import record `public` - they are all `file`-visible
/// and containerless, which is why nothing has ever landed on one - but
/// that is the plugins' convention and this is core's rule, so the test
/// states core's.
///
/// `external_module` is the arm that fails without GM-372's change; the
/// other four fail without the filter at all. The control is the test
/// below.
#[test]
fn no_kind_the_lookups_refuse_can_be_linked_onto() {
    for kind in NON_DECLARATION_NATIVE_KINDS {
        let mut conn = setup();
        let mut trap = symbol("target.ts", "mutate", MODULE_KIND, true);
        trap.id = format!("trap:{kind}");
        trap.native_kind = Some(kind.to_string());
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![symbol("caller.ts", "run", "Function", true), trap],
                ..Default::default()
            },
        )
        .unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "target.ts", "mutate");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary::default(), "a {kind} row is not a declaration to link onto");
        assert_eq!(
            edge_target(&conn, &edge),
            ("pending:caller.ts:target.ts#mutate".to_string(), false),
            "the {kind} row must leave the usage edge on its placeholder, unresolved"
        );
    }
}

/// The control for the test above: the same fixture with the `nativeKind`
/// taken off. A `Module` node is a perfectly good target for a
/// `REFERENCES` edge - a TS namespace is one - so this links in both
/// arms, which is what shows the refusals above come from the kind list
/// and not from the node's kind, its visibility, or the fixture being
/// unlinkable in the first place.
#[test]
fn a_module_that_is_not_an_address_is_still_linked_onto() {
    let mut conn = setup();
    let mut declaration = symbol("target.ts", "mutate", MODULE_KIND, true);
    declaration.id = "Module:target.ts:mutate".to_string();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![symbol("caller.ts", "run", "Function", true), declaration],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "target.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge), ("Module:target.ts:mutate".to_string(), true));
}

/// The other half of GM-372, and the direction that is easy to miss:
/// excluding a kind from the candidate set does not only withhold edges,
/// it lets one *land*. A file that both declares `mutate` and imports a
/// package spelled `mutate` offered two candidates to a `REFERENCES`
/// edge, and "several candidates" is a refusal (contract step 4) - so
/// before this change the import record cost the real declaration its
/// edge. Now there is one candidate and it is the right one.
#[test]
fn an_import_record_no_longer_makes_a_name_ambiguous() {
    let mut conn = setup();
    let mut import_record = symbol("target.ts", "mutate", MODULE_KIND, true);
    import_record.id = "external:target.ts:mutate".to_string();
    import_record.native_kind = Some(crate::graph::imports::EXTERNAL_MODULE_NATIVE_KIND.to_string());
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("target.ts", "mutate", "Function", true),
                import_record,
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "target.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(
        edge_target(&conn, &edge),
        ("Function:target.ts:mutate".to_string(), true),
        "the declaration answers; the import record of the same name is not a rival candidate"
    );
}

fn symbol(file: &str, name: &str, kind: &str, exported: bool) -> NodeRecord {
    let mut node = NodeRecord::new(format!("{kind}:{file}:{name}"), kind, name, name, file, "typescript");
    // Both fields, kept in lockstep by hand here the same way every
    // other `NodeRecord` constructor in this codebase has to - see
    // `NodeRecord.exported`'s own doc comment on why the database no
    // longer can disagree with `visibility`, but this in-memory struct
    // still technically could if only one of the two were set.
    node.exported = exported;
    node.visibility = if exported { "public" } else { "file" }.to_string();
    node
}

/// A `placeholder_targets` row as a plugin states it (`fromFile` is not
/// part of it: `apply_diff` fills that from the placeholder's own file).
fn target(
    scope_kind: &str,
    scope: &str,
    key_kind: &str,
    key: &str,
    from: Option<&str>,
) -> PlaceholderTargetRecord {
    PlaceholderTargetRecord {
        scope_kind: scope_kind.to_string(),
        scope: scope.to_string(),
        key_kind: key_kind.to_string(),
        key: key.to_string(),
        from_container: from.map(str::to_string),
        key_path: None,
    }
}

/// The plugin's own shape for a pending symbol: `filePath` is the
/// *importing* file (that is where the usage is written), and its target
/// is the export it is waiting for. `qualifiedName` still spells the v1
/// `<file>#<name>` address, as the TS plugin still sends it - nothing in
/// the linker reads it any more; the target is the address.
fn placeholder_node(importer: &str, target_file: &str, name: &str) -> NodeRecord {
    let mut node = NodeRecord::new(
        format!("pending:{importer}:{target_file}#{name}"),
        MODULE_KIND,
        name,
        format!("{target_file}#{name}"),
        importer,
        "typescript",
    );
    node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
    node.target = Some(target(SCOPE_FILE, target_file, KEY_NAME, name, None));
    node
}

/// The plugin's own shape for a re-export: it lives in the file that
/// *publishes* `published` (its `name`), and targets the name the file it
/// forwards to exports (`export { mutate as change } from "./target"` in
/// `index.ts` is `reexport_node("index.ts", "change", "target.ts",
/// "mutate")`) - the same split `protocol::types`' legacy derivation
/// makes.
fn reexport_node(file: &str, published: &str, target_file: &str, exported: &str) -> NodeRecord {
    let mut node = NodeRecord::new(
        format!("reexport:{file}:{target_file}#{exported}"),
        MODULE_KIND,
        published,
        format!("{target_file}#{exported}"),
        file,
        "typescript",
    );
    node.native_kind = Some(REEXPORT_NATIVE_KIND.to_string());
    node.target = Some(target(SCOPE_FILE, target_file, KEY_NAME, exported, None));
    node
}

/// `export * from "./target"` in `file`: it publishes every name the
/// target exports, so neither end of the address can be spelled out.
fn reexport_all(file: &str, target: &str) -> NodeRecord {
    reexport_node(file, REEXPORT_ALL_NAME, target, REEXPORT_ALL_NAME)
}

fn usage_edge(from: &str, kind: &str, placeholder: &NodeRecord) -> EdgeRecord {
    usage_edge_from(from, kind, placeholder, "tree-sitter")
}

fn usage_edge_from(from: &str, kind: &str, placeholder: &NodeRecord, source: &str) -> EdgeRecord {
    EdgeRecord::new(
        format!("edge:{from}:{kind}:{}", placeholder.id),
        from,
        placeholder.id.clone(),
        kind,
        source,
        false,
    )
}

/// `caller` (a symbol already in the index) uses `name` imported from
/// `target`, via a `kind` edge onto a fresh placeholder. The importing
/// file is read back out of the caller's id, which `symbol` builds as
/// `<kind>:<file>:<name>`.
fn seed_usage(conn: &mut Connection, caller: &str, kind: &str, target: &str, name: &str) -> String {
    let importer = caller.split(':').nth(1).expect("a caller id is <kind>:<file>:<name>");
    let placeholder = placeholder_node(importer, target, name);
    let edge = usage_edge(caller, kind, &placeholder);
    let edge_id = edge.id.clone();
    apply_diff(
        conn,
        &Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() },
    )
    .unwrap();
    edge_id
}

fn edge_target(conn: &Connection, edge_id: &str) -> (String, bool) {
    conn.query_row("SELECT toId, resolved FROM edges WHERE id = ?1", params![edge_id], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })
    .unwrap()
}

/// Which layer last answered for an edge - the other half of what a
/// semantic upgrade changes about a row.
fn edge_source(conn: &Connection, edge_id: &str) -> String {
    conn.query_row("SELECT source FROM edges WHERE id = ?1", params![edge_id], |row| row.get(0)).unwrap()
}

fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
}

/// One caller in `caller.ts` and one exported `mutate` in `target.ts`,
/// which is the whole bug in miniature.
fn seed_caller_and_target(conn: &mut Connection) {
    apply_diff(
        conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("target.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
}

#[test]
fn a_pending_call_becomes_an_edge_onto_the_exported_function() {
    let mut conn = setup();
    seed_caller_and_target(&mut conn);
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

    let summary = link_all(&mut conn).unwrap();

    assert_eq!(summary, LinkSummary { linked_edges: 1 });
    assert_eq!(
        edge_target(&conn, &edge),
        ("Function:target.ts:mutate".to_string(), true),
        "the call must land on the real function, and say so"
    );
}

/// The placeholder survives on purpose - a later edit to the same file
/// can add another usage edge onto it, and a deleted node would leave
/// that edge pointing at nothing.
#[test]
fn the_placeholder_survives_being_linked_away() {
    let mut conn = setup();
    seed_caller_and_target(&mut conn);
    seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

    link_all(&mut conn).unwrap();

    assert_eq!(count(&conn, "nodes"), 3, "the placeholder row must outlive the edge that was hanging on it");
}

#[test]
fn a_symbol_the_target_does_not_export_is_left_unresolved() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                // Same name, same file - but private to it.
                symbol("target.ts", "mutate", "Function", false),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

    let summary = link_all(&mut conn).unwrap();

    assert_eq!(summary, LinkSummary::default());
    assert!(!edge_target(&conn, &edge).1, "nothing was confirmed, so nothing is resolved");
}

#[test]
fn a_target_file_that_is_not_in_the_index_stays_a_placeholder() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff { upsert_nodes: vec![symbol("caller.ts", "run", "Function", true)], ..Default::default() },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "generated.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert_eq!(edge_target(&conn, &edge).0, "pending:caller.ts:generated.ts#mutate");
}

/// A `CALLS` edge is Function -> Function; an exported type of the same
/// name is not a thing you can call, so the edge waits rather than
/// landing on it.
#[test]
fn a_call_does_not_link_to_a_non_function_export() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("target.ts", "Shape", "Type", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "Shape");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert!(!edge_target(&conn, &edge).1);
}

/// The `find_implementations` case: a class in one file implementing an
/// interface imported from another.
#[test]
fn a_supertype_edge_links_to_the_exported_type() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("laser.ts", "LaserTrails", "Type", true),
                symbol("trail.ts", "Trail", "Type", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Type:laser.ts:LaserTrails", "SUPERTYPE_OF", "trail.ts", "Trail");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge), ("Type:trail.ts:Trail".to_string(), true));
}

/// One placeholder, two usages of different kinds, two different targets:
/// the class is what gets referenced, the function is what gets called.
#[test]
fn each_edge_kind_picks_the_export_that_fits_it() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("target.ts", "Widget", "Function", true),
                symbol("target.ts", "Widget", "Type", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let calls = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "Widget");
    let references = seed_usage(&mut conn, "Function:caller.ts:run", "SUPERTYPE_OF", "target.ts", "Widget");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
    assert_eq!(edge_target(&conn, &calls).0, "Function:target.ts:Widget");
    assert_eq!(edge_target(&conn, &references).0, "Type:target.ts:Widget");
}

/// A `REFERENCES` edge takes any kind of export - which is exactly why it
/// has to refuse when there are two of them.
#[test]
fn an_ambiguous_reference_is_left_unresolved() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("target.ts", "Widget", "Function", true),
                symbol("target.ts", "Widget", "Type", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "target.ts", "Widget");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert!(!edge_target(&conn, &edge).1, "a missing edge beats a wrong one");
}

#[test]
fn linking_twice_changes_nothing_the_second_time() {
    let mut conn = setup();
    seed_caller_and_target(&mut conn);
    seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert_eq!(count(&conn, "edges"), 1);
}

/// Several files calling the same exported symbol each have their own
/// placeholder (ids are per importing file), and all of them land.
#[test]
fn every_importer_is_linked_onto_the_one_definition() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("a.ts", "run", "Function", true),
                symbol("b.ts", "run", "Function", true),
                symbol("target.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    seed_usage(&mut conn, "Function:a.ts:run", "CALLS", "target.ts", "mutate");
    seed_usage(&mut conn, "Function:b.ts:run", "CALLS", "target.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
    let callers: Vec<String> = conn
        .prepare("SELECT fromId FROM edges WHERE toId = 'Function:target.ts:mutate' ORDER BY fromId")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(callers, vec!["Function:a.ts:run", "Function:b.ts:run"]);
}

/// The scoped pass must see the placeholders in the diff it is handed,
/// without a whole-index scan.
#[test]
fn a_diff_links_the_usages_it_brought_with_it() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff { upsert_nodes: vec![symbol("target.ts", "mutate", "Function", true)], ..Default::default() },
    )
    .unwrap();

    let placeholder = placeholder_node("caller.ts", "target.ts", "mutate");
    let edge = usage_edge("Function:caller.ts:run", "CALLS", &placeholder);
    let edge_id = edge.id.clone();
    let diff = Diff {
        upsert_nodes: vec![symbol("caller.ts", "run", "Function", true), placeholder],
        upsert_edges: vec![edge],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge_id).0, "Function:target.ts:mutate");
}

/// The cross-file half: a caller indexed while the symbol it calls did
/// not exist yet gets linked when that symbol finally shows up, rather
/// than waiting for the caller to be edited again.
#[test]
fn adding_an_export_links_the_usages_that_were_waiting_for_it() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff { upsert_nodes: vec![symbol("caller.ts", "run", "Function", true)], ..Default::default() },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "nothing exports it yet");

    let diff =
        Diff { upsert_nodes: vec![symbol("target.ts", "mutate", "Function", true)], ..Default::default() };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge), ("Function:target.ts:mutate".to_string(), true));
}

/// The reason placeholders are kept: a second usage in an already-linked
/// file arrives on its own, with the (unchanged) placeholder nowhere in
/// the diff, and still has to be linked.
#[test]
fn a_new_usage_of_an_already_linked_placeholder_is_linked_too() {
    let mut conn = setup();
    seed_caller_and_target(&mut conn);
    seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");
    link_all(&mut conn).unwrap();

    // A second function in the same file, calling the same import. The
    // placeholder it points at is unchanged, so the plugin does not
    // re-send it.
    let placeholder = placeholder_node("caller.ts", "target.ts", "mutate");
    let edge = usage_edge("Function:caller.ts:again", "CALLS", &placeholder);
    let edge_id = edge.id.clone();
    let diff = Diff {
        upsert_nodes: vec![symbol("caller.ts", "again", "Function", false)],
        upsert_edges: vec![edge],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge_id), ("Function:target.ts:mutate".to_string(), true));
}

/// A reindex resends a file's whole extraction with `resolved: false`,
/// which un-links every one of its edges - the next pass has to put them
/// back.
#[test]
fn a_full_reindex_of_the_importer_is_linked_again() {
    let mut conn = setup();
    seed_caller_and_target(&mut conn);
    let edge_id = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "target.ts", "mutate");
    link_all(&mut conn).unwrap();

    let placeholder = placeholder_node("caller.ts", "target.ts", "mutate");
    let diff = Diff {
        upsert_nodes: vec![
            symbol("caller.ts", "run", "Function", true),
            placeholder_node("caller.ts", "target.ts", "mutate"),
        ],
        upsert_edges: vec![usage_edge("Function:caller.ts:run", "CALLS", &placeholder)],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();
    assert!(!edge_target(&conn, &edge_id).1, "the resend reset it");

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge_id), ("Function:target.ts:mutate".to_string(), true));
}

// --- re-export chains -------------------------------------------------

/// The bug this whole chain-following exists for, in miniature: the
/// importer wrote `@pkg`, which resolves to the package's barrel, and the
/// function is one file further on.
#[test]
fn a_call_through_a_whole_module_reexport_lands_on_the_declaration() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("index.ts", "index.ts", "File", false),
                reexport_all("index.ts", "target.ts"),
                symbol("target.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge), ("Function:target.ts:mutate".to_string(), true));
}

#[test]
fn a_named_reexport_is_followed_to_the_file_that_declares_it() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_node("index.ts", "mutate", "target.ts", "mutate"),
                symbol("target.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, "Function:target.ts:mutate");
}

/// `export { mutate as change } from "./target"`: the importer knows the
/// published name, the target file only the original one, and the hop is
/// the only place the two are ever written down together.
#[test]
fn a_renaming_reexport_is_followed_under_the_name_the_target_declares() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_node("index.ts", "change", "target.ts", "mutate"),
                symbol("target.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "change");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, "Function:target.ts:mutate");
}

/// A barrel re-exporting a barrel, which real monorepos do: excalidraw's
/// own package entry points reach two hops deep.
#[test]
fn a_chain_of_barrels_is_followed_to_its_end() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_all("index.ts", "inner/index.ts"),
                reexport_node("inner/index.ts", "mutate", "inner/mutate.ts", "mutate"),
                symbol("inner/mutate.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, "Function:inner/mutate.ts:mutate");
}

/// A name a barrel both declares and re-exports is the barrel's own, as it
/// is in the language - the breadth-first walk answers from the shallowest
/// level that has anything.
#[test]
fn a_declaration_shadows_what_the_same_file_reexports() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("index.ts", "mutate", "Function", true),
                reexport_all("index.ts", "target.ts"),
                symbol("target.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, "Function:index.ts:mutate");
}

/// Two `export *` branches offering the same name: this pass calls it
/// ambiguous - a missing edge beats a wrong one, exactly as for a name one
/// file exports twice - and the semantic layer, which has the compiler's
/// own module resolution, settles it afterwards.
///
/// The two halves are asserted in order on purpose. The first is the
/// contract of the fast layer, which has to keep behaving exactly as it
/// did: whatever the checker later says, an index that has only been
/// through the structural pass must not claim to know which branch won.
#[test]
fn two_reexport_branches_offering_one_name_leave_the_edge_unresolved() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_all("index.ts", "a.ts"),
                reexport_all("index.ts", "b.ts"),
                symbol("a.ts", "mutate", "Function", true),
                symbol("b.ts", "mutate", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert!(!edge_target(&conn, &edge).1);
    assert_eq!(edge_source(&conn, &edge), "syntactic");

    // What the semantic pass answers, in the shape it answers it: the edge
    // re-sent under its own id (`plugins/typescript/src/semanticPass.ts`), which
    // is why this needed no storage path of its own - `apply_diff`'s
    // ON CONFLICT rewrites the row in place.
    //
    // `a.ts` and not `b.ts` because that is what TypeScript's own module
    // resolution hands a consumer, measured rather than assumed (tsc /
    // tsserver 5.9.3 on exactly this fixture): `tsc --noEmit` reports the
    // TS2308 ambiguity against the *second* `export *`, a barrel-level
    // diagnostic, while `definition` at the importer's `mutate` returns
    // exactly one location - the first branch's - and swapping the two
    // statements swaps the answer. See semanticPass.ts's module comment.
    let upgrade = Diff {
        upsert_edges: vec![EdgeRecord::new(
            edge.clone(),
            "Function:caller.ts:run",
            "Function:a.ts:mutate",
            "CALLS",
            "ts-compiler",
            true,
        )],
        ..Default::default()
    };
    apply_diff(&mut conn, &upgrade).unwrap();

    // Nothing left for this pass to link: the edge no longer hangs on the
    // placeholder, so the two layers cannot fight over it.
    assert_eq!(link_diff(&mut conn, &upgrade).unwrap(), LinkSummary::default());
    assert_eq!(edge_target(&conn, &edge), ("Function:a.ts:mutate".to_string(), true));
    assert_eq!(edge_source(&conn, &edge), "semantic");
}

/// `export * from "./x"` republishes every *named* export of `./x` and
/// never its default, so a chain reaching `default` through one ends there.
#[test]
fn a_whole_module_reexport_does_not_carry_a_default_export() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_all("index.ts", "target.ts"),
                symbol("target.ts", "default", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "default");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert!(!edge_target(&conn, &edge).1);

    // A *named* re-export of it is a different statement and does carry it.
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![reexport_node("index.ts", "default", "target.ts", "default")],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, "Function:target.ts:default");
}

/// Two barrels re-exporting each other, which a half-finished refactor
/// produces. Terminating at all is the assertion; the summary only says
/// nothing was invented on the way out.
#[test]
fn a_reexport_cycle_terminates_without_linking() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_all("a.ts", "b.ts"),
                reexport_all("b.ts", "a.ts"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "a.ts", "mutate");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert!(!edge_target(&conn, &edge).1);
}

/// The length guard: a chain of exactly `MAX_REEXPORT_DEPTH` hops still
/// resolves, one hop more is left alone rather than walked forever.
#[test]
fn a_chain_longer_than_the_depth_cap_is_left_unresolved() {
    fn chain(hops: usize) -> (Connection, String) {
        let mut conn = setup();
        let mut nodes = vec![symbol("caller.ts", "run", "Function", true)];
        for hop in 0..hops {
            nodes.push(reexport_all(&format!("barrel{hop}.ts"), &format!("barrel{}.ts", hop + 1)));
        }
        nodes.push(symbol(&format!("barrel{hops}.ts"), "mutate", "Function", true));
        apply_diff(&mut conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
        let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "barrel0.ts", "mutate");
        (conn, edge)
    }

    let (mut at_cap, edge) = chain(MAX_REEXPORT_DEPTH);
    assert_eq!(link_all(&mut at_cap).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&at_cap, &edge).0, format!("Function:barrel{MAX_REEXPORT_DEPTH}.ts:mutate"));

    let (mut past_cap, edge) = chain(MAX_REEXPORT_DEPTH + 1);
    assert_eq!(link_all(&mut past_cap).unwrap(), LinkSummary::default());
    assert!(!edge_target(&past_cap, &edge).1);
}

/// A re-export placeholder is not a definition of anything: it must never
/// be what a usage edge lands on, however well its name fits.
#[test]
fn a_usage_never_lands_on_the_reexport_placeholder_itself() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_node("index.ts", "mutate", "target.ts", "mutate"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "REFERENCES", "index.ts", "mutate");

    // target.ts is not in the index, so the chain runs out one hop in.
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert_eq!(edge_target(&conn, &edge).0, "pending:caller.ts:index.ts#mutate");
}

/// The cross-file half through a barrel: the declaration shows up after
/// everything else, and the usage waiting on the *barrel's* address - not
/// on this file's - still has to be linked.
#[test]
fn adding_a_declaration_links_the_usages_waiting_on_a_barrel_for_it() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                reexport_all("index.ts", "inner/index.ts"),
                reexport_all("inner/index.ts", "inner/mutate.ts"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let edge = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "nothing declares it yet");

    let diff = Diff {
        upsert_nodes: vec![symbol("inner/mutate.ts", "mutate", "Function", true)],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, "Function:inner/mutate.ts:mutate");
}

/// The other way round: everything is in the index and it is the *barrel*
/// that is written, which is what re-exporting an existing module from a
/// new index file looks like to the watcher.
#[test]
fn adding_a_barrel_links_the_usages_that_were_waiting_on_it() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("caller.ts", "run", "Function", true),
                symbol("target.ts", "mutate", "Function", true),
                symbol("target.ts", "change", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    let whole = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "mutate");
    let named = seed_usage(&mut conn, "Function:caller.ts:run", "CALLS", "index.ts", "renamed");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "there is no barrel yet");

    let diff = Diff {
        upsert_nodes: vec![
            reexport_all("index.ts", "target.ts"),
            reexport_node("index.ts", "renamed", "target.ts", "change"),
        ],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 2 });
    assert_eq!(edge_target(&conn, &whole).0, "Function:target.ts:mutate");
    assert_eq!(edge_target(&conn, &named).0, "Function:target.ts:change");
}

/// An import placeholder is a different handshake with a different owner
/// (`graph::imports`); this pass must not touch one.
#[test]
fn a_module_import_placeholder_is_left_exactly_as_it_was() {
    let mut conn = setup();
    let mut module = NodeRecord::new("mod:a.ts:b.ts", MODULE_KIND, "./b", "b.ts", "a.ts", "typescript");
    module.native_kind = Some("resolved_module".to_string());
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![symbol("a.ts", "a.ts", "File", false), module],
            upsert_edges: vec![EdgeRecord::new(
                "e_imports",
                "File:a.ts:a.ts",
                "mod:a.ts:b.ts",
                "IMPORTS",
                "tree-sitter",
                false,
            )],
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    assert_eq!(edge_target(&conn, "e_imports"), ("mod:a.ts:b.ts".to_string(), false));
}

// --- placeholders the semantic pass sends ------------------------------

/// `import * as ns from "./mod"` then `ns.someExport()`. The structural
/// pass emits nothing at all for that site - it never sees the bare name
/// `someExport`, only a property access - so the placeholder and its edge
/// arrive from the semantic pass instead, addressed at the declaration
/// `tsserver` bound. Nothing here treats them differently: the address is
/// the whole contract, and `source` is the plugin's to set.
#[test]
fn a_namespace_import_usage_from_the_semantic_pass_is_linked_like_any_other() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("app.ts", "run", "Function", true),
                symbol("mod.ts", "someExport", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();

    let placeholder = placeholder_node("app.ts", "mod.ts", "someExport");
    let edge = usage_edge_from("Function:app.ts:run", "CALLS", &placeholder, "ts-compiler");
    let edge_id = edge.id.clone();
    let diff = Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(
        edge_target(&conn, &edge_id),
        ("Function:mod.ts:someExport".to_string(), true),
        "the call must land on the real declaration, and say so"
    );
    // Repointing settles *what* the edge points at; it never overwrites who
    // worked it out. An edge a checker answered must keep saying so.
    assert_eq!(edge_source(&conn, &edge_id), "semantic");
}

/// The same usage where the module `ns` names is a barrel: the semantic
/// pass addresses whatever declaration site the checker reported, so a
/// re-export statement is a legitimate address and the chain walk finishes
/// from there exactly as it does for a named import.
#[test]
fn a_semantic_placeholder_addressed_at_a_barrel_is_followed_to_the_declaration() {
    let mut conn = setup();
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: vec![
                symbol("app.ts", "run", "Function", true),
                reexport_node("barrel.ts", "someExport", "impl.ts", "realName"),
                symbol("impl.ts", "realName", "Function", true),
            ],
            ..Default::default()
        },
    )
    .unwrap();

    let placeholder = placeholder_node("app.ts", "barrel.ts", "someExport");
    let edge = usage_edge_from("Function:app.ts:run", "CALLS", &placeholder, "ts-compiler");
    let edge_id = edge.id.clone();
    let diff = Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() };
    apply_diff(&mut conn, &diff).unwrap();

    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge_id).0, "Function:impl.ts:realName");
    assert_eq!(edge_source(&conn, &edge_id), "semantic");
}

#[test]
fn an_empty_diff_is_a_no_op() {
    let mut conn = setup();
    assert_eq!(link_diff(&mut conn, &Diff::default()).unwrap(), LinkSummary::default());
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
}

// --- structured targets: what the old `<file>#<name>` string could not say --

/// Where a declaration or a requester of a containered language sits.
#[derive(Clone, Copy)]
struct At<'a> {
    file: &'a str,
    language: &'a str,
    container: &'a str,
    parent: Option<&'a str>,
}

enum Vis<'a> {
    Public,
    File,
    Container(&'a str),
}

fn last_segment(qualified_name: &str) -> &str {
    qualified_name.rsplit(['.', ':']).next().expect("rsplit always yields one item")
}

/// A member of `at.container`, id `<kind>:<file>:<qualifiedName>`, named by
/// the qualifiedName's last segment (`Server.Close` is `Close`,
/// `mycrate::util::helper` is `helper`).
fn member(at: At, kind: &str, qualified_name: &str, visibility: Vis) -> NodeRecord {
    let mut node = NodeRecord::new(
        format!("{kind}:{}:{qualified_name}", at.file),
        kind,
        last_segment(qualified_name),
        qualified_name,
        at.file,
        at.language,
    );
    node.container = Some(at.container.to_string());
    node.container_parent = at.parent.map(str::to_string);
    match visibility {
        Vis::Public => {
            node.visibility = VISIBILITY_PUBLIC.to_string();
            node.exported = true;
        }
        Vis::File => node.visibility = VISIBILITY_FILE.to_string(),
        Vis::Container(key) => {
            node.visibility = VISIBILITY_CONTAINER.to_string();
            node.visibility_container = Some(key.to_string());
        }
    }
    node
}

/// A placeholder in `at.file`, asking from `at.container`, scoped to the
/// container `scope`. Its `qualifiedName` is deliberately *not* the v1
/// `<file>#<name>` shape: nothing may parse it.
fn container_placeholder(at: At, scope: &str, key_kind: &str, key: &str) -> NodeRecord {
    let id = format!("pending:{}:{scope}:{key_kind}:{key}", at.file);
    let mut node = NodeRecord::new(id.clone(), MODULE_KIND, last_segment(key), id, at.file, at.language);
    node.native_kind = Some(PENDING_SYMBOL_NATIVE_KIND.to_string());
    node.target = Some(target(SCOPE_CONTAINER, scope, key_kind, key, Some(at.container)));
    node
}

/// A re-export living in `at` (its file and its container) that publishes
/// `published`, forwarding to `key` in the container `scope` - a Rust
/// `pub use crate::util::helper;` inside `mod prelude`.
fn container_reexport(at: At, published: &str, scope: &str, key: &str) -> NodeRecord {
    let id = format!("reexport:{}:{scope}:{key}", at.file);
    let mut node = NodeRecord::new(id.clone(), MODULE_KIND, published, id, at.file, at.language);
    node.native_kind = Some(REEXPORT_NATIVE_KIND.to_string());
    node.container = Some(at.container.to_string());
    node.target = Some(target(SCOPE_CONTAINER, scope, KEY_NAME, key, Some(at.container)));
    node
}

/// `caller` (a node already in the index, or in `with`) uses `placeholder`
/// through a `kind` edge; everything in one diff, as one file's
/// extraction would send it. Returns the edge id.
fn use_through(
    conn: &mut Connection,
    with: Vec<NodeRecord>,
    caller: &str,
    kind: &str,
    placeholder: NodeRecord,
) -> String {
    let edge = usage_edge(caller, kind, &placeholder);
    let edge_id = edge.id.clone();
    let mut nodes = with;
    nodes.push(placeholder);
    apply_diff(conn, &Diff { upsert_nodes: nodes, upsert_edges: vec![edge], ..Default::default() }).unwrap();
    edge_id
}

fn upsert(conn: &mut Connection, nodes: Vec<NodeRecord>) {
    apply_diff(conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
}

const GO_UTIL: &str = "example.com/app/util";

fn go_util_file(file: &str) -> At<'_> {
    At { file, language: "go", container: GO_UTIL, parent: None }
}

fn go_cmd() -> At<'static> {
    At { file: "cmd/main.go", language: "go", container: "example.com/app/cmd", parent: None }
}

/// A Go package spread over two files, called from another package by a
/// container-scoped name: the edge lands wherever in the package the name
/// is declared, which a file-scoped address could only have said by
/// knowing the file - the one thing an importer of a package never writes
/// down.
#[test]
fn a_container_scoped_name_links_to_the_member_whichever_file_declares_it() {
    let mut conn = setup();
    upsert(
        &mut conn,
        vec![
            member(go_util_file("util/point.go"), "Function", "NewPoint", Vis::Public),
            member(go_util_file("util/rect.go"), "Function", "NewRect", Vis::Public),
        ],
    );
    let main = member(go_cmd(), "Function", "main", Vis::Public);
    let point = use_through(
        &mut conn,
        vec![main],
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "NewPoint"),
    );
    let rect = use_through(
        &mut conn,
        Vec::new(),
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "NewRect"),
    );

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
    assert_eq!(edge_target(&conn, &point), ("Function:util/point.go:NewPoint".to_string(), true));
    assert_eq!(edge_target(&conn, &rect), ("Function:util/rect.go:NewRect".to_string(), true));
}

/// `Server.Close` and `Client.Close` share a name; a name key cannot tell
/// them apart and refuses, a qualifiedName key names one and lands.
#[test]
fn a_qualified_name_key_disambiguates_two_same_named_methods() {
    let mut conn = setup();
    let server = go_util_file("util/server.go");
    upsert(
        &mut conn,
        vec![
            member(server, "Function", "Server.Close", Vis::Public),
            member(server, "Function", "Client.Close", Vis::Public),
            member(go_cmd(), "Function", "main", Vis::Public),
        ],
    );
    let by_name = use_through(
        &mut conn,
        Vec::new(),
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Close"),
    );
    let exact = use_through(
        &mut conn,
        Vec::new(),
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_QUALIFIED_NAME, "Client.Close"),
    );

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert!(!edge_target(&conn, &by_name).1, "two Close methods: a name key must refuse");
    assert_eq!(edge_target(&conn, &exact), ("Function:util/server.go:Client.Close".to_string(), true));
}

/// Go's unexported: visible to its own package, and only to it - under a
/// name key, and under a qualifiedName key too, which is what stops a
/// semantic tier's mistake from linking a private symbol from outside.
#[test]
fn a_go_unexported_symbol_links_within_its_package_and_not_from_outside() {
    let mut conn = setup();
    let helper = go_util_file("util/helper.go");
    let sibling = go_util_file("util/use.go");
    upsert(
        &mut conn,
        vec![
            member(helper, "Function", "helper", Vis::Container(GO_UTIL)),
            member(sibling, "Function", "useHelper", Vis::Public),
            member(go_cmd(), "Function", "main", Vis::Public),
        ],
    );
    let inside = use_through(
        &mut conn,
        Vec::new(),
        "Function:util/use.go:useHelper",
        "CALLS",
        container_placeholder(sibling, GO_UTIL, KEY_NAME, "helper"),
    );
    let outside = use_through(
        &mut conn,
        Vec::new(),
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "helper"),
    );
    let outside_exact = use_through(
        &mut conn,
        Vec::new(),
        "Function:cmd/main.go:main",
        "REFERENCES",
        container_placeholder(go_cmd(), GO_UTIL, KEY_QUALIFIED_NAME, "helper"),
    );

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &inside), ("Function:util/helper.go:helper".to_string(), true));
    assert!(!edge_target(&conn, &outside).1, "another package must not see an unexported symbol");
    assert!(!edge_target(&conn, &outside_exact).1, "not even by its exact qualifiedName");
}

/// The Rust crate the tests below share: `mycrate` with `mod util`
/// (holding a `pub(crate) fn helper` and a private `fn private_helper`)
/// and `mod net { mod http; }`, every module with a member so every parent
/// is recorded - as a Rust plugin emits each `mod child;` item as a member
/// of its parent (`graph::containers::parent_chain`).
fn rust_lib() -> At<'static> {
    At { file: "src/lib.rs", language: "rust", container: "mycrate", parent: None }
}
fn rust_net() -> At<'static> {
    At { file: "src/net/mod.rs", language: "rust", container: "mycrate::net", parent: Some("mycrate") }
}
fn rust_util() -> At<'static> {
    At { file: "src/util.rs", language: "rust", container: "mycrate::util", parent: Some("mycrate") }
}
fn rust_http() -> At<'static> {
    At {
        file: "src/net/http.rs",
        language: "rust",
        container: "mycrate::net::http",
        parent: Some("mycrate::net"),
    }
}
fn rust_prelude() -> At<'static> {
    At { file: "src/prelude.rs", language: "rust", container: "mycrate::prelude", parent: Some("mycrate") }
}
fn rust_other() -> At<'static> {
    At { file: "other/src/lib.rs", language: "rust", container: "othercrate", parent: None }
}

fn rust_lib_nodes() -> Vec<NodeRecord> {
    vec![
        member(rust_lib(), "Module", "mycrate::net", Vis::Public),
        member(rust_lib(), "Module", "mycrate::util", Vis::Public),
        member(rust_lib(), "Module", "mycrate::prelude", Vis::Public),
    ]
}
fn rust_net_nodes() -> Vec<NodeRecord> {
    vec![member(rust_net(), "Module", "mycrate::net::http", Vis::Public)]
}
fn rust_util_nodes() -> Vec<NodeRecord> {
    vec![
        member(rust_util(), "Function", "mycrate::util::helper", Vis::Container("mycrate")),
        member(rust_util(), "Function", "mycrate::util::private_helper", Vis::Container("mycrate::util")),
    ]
}
fn rust_get() -> NodeRecord {
    member(rust_http(), "Function", "mycrate::net::http::get", Vis::Container("mycrate::net::http"))
}

const RUST_GET: &str = "Function:src/net/http.rs:mycrate::net::http::get";
const RUST_RUN: &str = "Function:other/src/lib.rs:othercrate::run";
const RUST_HELPER: &str = "Function:src/util.rs:mycrate::util::helper";

#[test]
fn a_rust_pub_crate_symbol_is_visible_from_a_descendant_module_and_not_from_another_crate() {
    let mut conn = setup();
    let mut nodes = rust_lib_nodes();
    nodes.extend(rust_net_nodes());
    nodes.extend(rust_util_nodes());
    nodes.push(rust_get());
    nodes.push(member(rust_other(), "Function", "othercrate::run", Vis::Public));
    upsert(&mut conn, nodes);

    let descendant = use_through(
        &mut conn,
        Vec::new(),
        RUST_GET,
        "CALLS",
        container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "helper"),
    );
    let private_from_elsewhere = use_through(
        &mut conn,
        Vec::new(),
        RUST_GET,
        "CALLS",
        container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "private_helper"),
    );
    let other_crate = use_through(
        &mut conn,
        Vec::new(),
        RUST_RUN,
        "CALLS",
        container_placeholder(rust_other(), "mycrate::util", KEY_NAME, "helper"),
    );

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(
        edge_target(&conn, &descendant),
        (RUST_HELPER.to_string(), true),
        "pub(crate) is visible from mycrate::net::http, whose chain reaches mycrate"
    );
    assert!(
        !edge_target(&conn, &private_from_elsewhere).1,
        "a module-private item is not visible from outside its module"
    );
    assert!(!edge_target(&conn, &other_crate).1, "pub(crate) is not visible from another crate");
}

/// `pub use crate::util::helper;` inside `mod prelude`: a container's
/// re-export is followed like a barrel's, and the declaration at the end
/// is still checked against the original requester.
#[test]
fn a_container_reexport_is_followed_into_the_container_it_forwards_to() {
    let mut conn = setup();
    let mut nodes = rust_lib_nodes();
    nodes.extend(rust_net_nodes());
    nodes.extend(rust_util_nodes());
    nodes.push(rust_get());
    nodes.push(member(rust_other(), "Function", "othercrate::run", Vis::Public));
    nodes.push(member(rust_prelude(), "Function", "mycrate::prelude::marker", Vis::Public));
    nodes.push(container_reexport(rust_prelude(), "helper", "mycrate::util", "helper"));
    upsert(&mut conn, nodes);

    let inside = use_through(
        &mut conn,
        Vec::new(),
        RUST_GET,
        "CALLS",
        container_placeholder(rust_http(), "mycrate::prelude", KEY_NAME, "helper"),
    );
    let outside = use_through(
        &mut conn,
        Vec::new(),
        RUST_RUN,
        "CALLS",
        container_placeholder(rust_other(), "mycrate::prelude", KEY_NAME, "helper"),
    );

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &inside).0, RUST_HELPER);
    assert!(!edge_target(&conn, &outside).1, "re-exporting does not widen the declaration's own visibility");
}

/// The container counterpart of `adding_an_export_links_the_usages_that_
/// were_waiting_for_it`: a usage waits on a package, and the symbol shows
/// up in it later - first as a brand new node, then (second half) as an
/// existing node re-sent as a member.
#[test]
fn a_node_joining_a_container_links_the_placeholders_waiting_on_it() {
    let mut conn = setup();
    let edge = use_through(
        &mut conn,
        vec![member(go_cmd(), "Function", "main", Vis::Public)],
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Helper"),
    );
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "the package is empty");

    let diff = Diff {
        upsert_nodes: vec![member(go_util_file("util/helper.go"), "Function", "Helper", Vis::Public)],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();
    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge), ("Function:util/helper.go:Helper".to_string(), true));

    let waiting = use_through(
        &mut conn,
        Vec::new(),
        "Function:cmd/main.go:main",
        "CALLS",
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Moved"),
    );
    let mut loose = member(go_util_file("util/moved.go"), "Function", "Moved", Vis::Public);
    loose.container = None;
    upsert(&mut conn, vec![loose]);
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "not a member yet");

    let diff = Diff {
        upsert_nodes: vec![member(go_util_file("util/moved.go"), "Function", "Moved", Vis::Public)],
        ..Default::default()
    };
    apply_diff(&mut conn, &diff).unwrap();
    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &waiting).0, "Function:util/moved.go:Moved");
}

/// The parent chain completing after the requester: `mycrate::net::http`
/// is indexed before `mycrate::net` has a row, so its chain stops at
/// `mycrate::net` and `pub(crate)` is refused - correctly, for what the
/// index knew. When `net`'s file arrives the chain reaches `mycrate`, and
/// the placeholder in `http.rs`, which that diff does not mention at all,
/// has to be revisited.
#[test]
fn a_container_arriving_above_a_requester_links_what_its_chain_now_reaches() {
    let mut conn = setup();
    let mut nodes = rust_lib_nodes();
    nodes.extend(rust_util_nodes());
    upsert(&mut conn, nodes);
    let edge = use_through(
        &mut conn,
        vec![rust_get()],
        RUST_GET,
        "CALLS",
        container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "helper"),
    );
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "mycrate::net has no row yet");

    let diff = Diff { upsert_nodes: rust_net_nodes(), ..Default::default() };
    apply_diff(&mut conn, &diff).unwrap();
    assert_eq!(link_diff(&mut conn, &diff).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge).0, RUST_HELPER);
}

/// `file` visibility inside a container (C++'s `static` or anonymous
/// namespace in a namespace reopened across files): visible through the
/// container from its own file, not from a sibling file.
#[test]
fn a_file_private_member_is_visible_through_its_container_only_from_its_own_file() {
    let mut conn = setup();
    let a = At { file: "lib/a.cpp", language: "cpp", container: "llvm", parent: None };
    let b = At { file: "lib/b.cpp", language: "cpp", container: "llvm", parent: None };
    upsert(
        &mut conn,
        vec![
            member(a, "Function", "llvm::helper", Vis::File),
            member(a, "Function", "llvm::fromA", Vis::Public),
            member(b, "Function", "llvm::fromB", Vis::Public),
        ],
    );
    let same_file = use_through(
        &mut conn,
        Vec::new(),
        "Function:lib/a.cpp:llvm::fromA",
        "CALLS",
        container_placeholder(a, "llvm", KEY_NAME, "helper"),
    );
    let other_file = use_through(
        &mut conn,
        Vec::new(),
        "Function:lib/b.cpp:llvm::fromB",
        "CALLS",
        container_placeholder(b, "llvm", KEY_NAME, "helper"),
    );

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &same_file).0, "Function:lib/a.cpp:llvm::helper");
    assert!(!edge_target(&conn, &other_file).1);
}

/// TS equivalence at its one edge: a placeholder addressed at the
/// requester's *own* file (`import { render } from "./self"`, or the
/// semantic pass answering `ns.render` through a barrel that re-exports
/// the importer). The old lookup required `exported = 1`, so a
/// non-exported method `render` in the same file was never a candidate;
/// it must not become one - neither as an ambiguity beside the export,
/// nor as the only fit when there is no export.
#[test]
fn a_self_addressed_placeholder_sees_only_what_its_file_exports() {
    let mut conn = setup();
    let mut method = symbol("view.ts", "View.render", "Function", false);
    method.name = "render".to_string();
    upsert(
        &mut conn,
        vec![
            symbol("view.ts", "run", "Function", true),
            symbol("view.ts", "render", "Function", true),
            method,
            symbol("view.ts", "paint", "Function", false),
        ],
    );

    let exported = seed_usage(&mut conn, "Function:view.ts:run", "CALLS", "view.ts", "render");
    let private = seed_usage(&mut conn, "Function:view.ts:run", "CALLS", "view.ts", "paint");

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &exported).0, "Function:view.ts:render", "the export, not an ambiguity");
    assert!(!edge_target(&conn, &private).1, "a non-exported symbol never answers a file-scoped placeholder");
}

/// The target is the address: a placeholder whose `qualifiedName` still
/// looks like a v1 address but has no target row stays unlinked (reported,
/// not guessed from the string), and one whose `qualifiedName` says
/// something else entirely links by its target.
#[test]
fn the_target_row_is_the_address_and_the_qualified_name_is_never_parsed() {
    let mut conn = setup();
    seed_caller_and_target(&mut conn);

    let mut untargeted = placeholder_node("caller.ts", "target.ts", "mutate");
    untargeted.target = None;
    let untargeted_edge = use_through(&mut conn, Vec::new(), "Function:caller.ts:run", "CALLS", untargeted);

    let mut misleading = placeholder_node("caller.ts", "target.ts", "mutate");
    misleading.id = "pending:caller.ts:misleading".to_string();
    misleading.qualified_name = "elsewhere.ts#other".to_string();
    let misleading_edge =
        use_through(&mut conn, Vec::new(), "Function:caller.ts:run", "REFERENCES", misleading);

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert!(!edge_target(&conn, &untargeted_edge).1, "no target row: left alone, never guessed");
    assert_eq!(edge_target(&conn, &misleading_edge).0, "Function:target.ts:mutate");
}

// --- link_all == link_diff ----------------------------------------------

/// What an equivalence fixture expects of one usage edge: the node it
/// lands on, or `None` for staying on its placeholder.
type Expected = Vec<(String, Option<&'static str>)>;

/// One file's extraction per diff - TS barrels, a Go package used from
/// inside and outside, a Rust crate with a container re-export and a
/// second crate - plus what each usage edge must end up pointing at.
/// Every answer here only ever improves as files arrive, in any order: the
/// condition under which [`link_diff`] promises to agree with
/// [`link_all`] (see its doc on what is deliberately not covered).
fn equivalence_fixture() -> (Vec<Diff>, Expected) {
    let mut expected: Expected = Vec::new();
    let mut uses = |edges: &mut Vec<EdgeRecord>,
                    from: &str,
                    kind: &str,
                    to: &NodeRecord,
                    lands: Option<&'static str>| {
        let edge = usage_edge(from, kind, to);
        expected.push((edge.id.clone(), lands));
        edges.push(edge);
    };
    let diff = |upsert_nodes: Vec<NodeRecord>, upsert_edges: Vec<EdgeRecord>| Diff {
        upsert_nodes,
        upsert_edges,
        ..Default::default()
    };
    let mut diffs = Vec::new();

    // TS: a declaring file, a barrel (whole-module and renaming), a caller.
    diffs.push(diff(
        vec![
            symbol("target.ts", "mutate", "Function", true),
            symbol("target.ts", "helper", "Function", false),
        ],
        Vec::new(),
    ));
    diffs.push(diff(
        vec![
            reexport_all("index.ts", "target.ts"),
            reexport_node("index.ts", "change", "target.ts", "mutate"),
        ],
        Vec::new(),
    ));
    let ts = [
        placeholder_node("caller.ts", "index.ts", "mutate"),
        placeholder_node("caller.ts", "index.ts", "change"),
        placeholder_node("caller.ts", "target.ts", "helper"),
        placeholder_node("caller.ts", "target.ts", "mutate"),
    ];
    let mut edges = Vec::new();
    uses(&mut edges, "Function:caller.ts:run", "CALLS", &ts[0], Some("Function:target.ts:mutate"));
    uses(&mut edges, "Function:caller.ts:run", "CALLS", &ts[1], Some("Function:target.ts:mutate"));
    uses(&mut edges, "Function:caller.ts:run", "CALLS", &ts[2], None);
    uses(&mut edges, "Function:caller.ts:run", "REFERENCES", &ts[3], Some("Function:target.ts:mutate"));
    let mut nodes = vec![symbol("caller.ts", "run", "Function", true)];
    nodes.extend(ts);
    diffs.push(diff(nodes, edges));

    // Go: a package in three files, used from inside and from outside.
    let helper_go = go_util_file("util/helper.go");
    diffs.push(diff(
        vec![
            member(helper_go, "Function", "Helper", Vis::Public),
            member(helper_go, "Function", "helper", Vis::Container(GO_UTIL)),
        ],
        Vec::new(),
    ));
    let server_go = go_util_file("util/server.go");
    diffs.push(diff(
        vec![
            member(server_go, "Function", "Server.Close", Vis::Public),
            member(server_go, "Function", "Client.Close", Vis::Public),
        ],
        Vec::new(),
    ));
    let go = [
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Helper"),
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "helper"),
        container_placeholder(go_cmd(), GO_UTIL, KEY_QUALIFIED_NAME, "Server.Close"),
        container_placeholder(go_cmd(), GO_UTIL, KEY_NAME, "Close"),
    ];
    let main = "Function:cmd/main.go:main";
    let mut edges = Vec::new();
    uses(&mut edges, main, "CALLS", &go[0], Some("Function:util/helper.go:Helper"));
    uses(&mut edges, main, "CALLS", &go[1], None);
    uses(&mut edges, main, "CALLS", &go[2], Some("Function:util/server.go:Server.Close"));
    uses(&mut edges, main, "REFERENCES", &go[3], None);
    let mut nodes = vec![member(go_cmd(), "Function", "main", Vis::Public)];
    nodes.extend(go);
    diffs.push(diff(nodes, edges));
    let use_go = go_util_file("util/use.go");
    let sibling = container_placeholder(use_go, GO_UTIL, KEY_NAME, "helper");
    let mut edges = Vec::new();
    uses(
        &mut edges,
        "Function:util/use.go:useHelper",
        "CALLS",
        &sibling,
        Some("Function:util/helper.go:helper"),
    );
    diffs.push(diff(vec![member(use_go, "Function", "useHelper", Vis::Container(GO_UTIL)), sibling], edges));

    // Rust: one diff per module file, plus a second crate.
    diffs.push(diff(rust_lib_nodes(), Vec::new()));
    diffs.push(diff(rust_net_nodes(), Vec::new()));
    diffs.push(diff(rust_util_nodes(), Vec::new()));
    diffs.push(diff(
        vec![
            member(rust_prelude(), "Function", "mycrate::prelude::marker", Vis::Public),
            container_reexport(rust_prelude(), "helper", "mycrate::util", "helper"),
        ],
        Vec::new(),
    ));
    let rust = [
        container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "helper"),
        container_placeholder(rust_http(), "mycrate::util", KEY_NAME, "private_helper"),
        container_placeholder(rust_http(), "mycrate::prelude", KEY_NAME, "helper"),
    ];
    let mut edges = Vec::new();
    uses(&mut edges, RUST_GET, "CALLS", &rust[0], Some(RUST_HELPER));
    uses(&mut edges, RUST_GET, "CALLS", &rust[1], None);
    uses(&mut edges, RUST_GET, "CALLS", &rust[2], Some(RUST_HELPER));
    let mut nodes = vec![rust_get()];
    nodes.extend(rust);
    diffs.push(diff(nodes, edges));
    let other = container_placeholder(rust_other(), "mycrate::util", KEY_NAME, "helper");
    let mut edges = Vec::new();
    uses(&mut edges, RUST_RUN, "CALLS", &other, None);
    diffs.push(diff(vec![member(rust_other(), "Function", "othercrate::run", Vis::Public), other], edges));

    // Rust members through re-exports: the head and its field in one diff,
    // the method as a later edit of its own, each re-exporting module, and
    // users through a named, an aliased, a glob and a chained re-export, plus
    // one whose plugin sent no `keyPath`.
    diffs.push(diff(vec![gm472_type(), gm472_field()], Vec::new()));
    diffs.push(diff(vec![gm472_method()], Vec::new()));
    diffs.push(diff(gm472_named(), Vec::new()));
    diffs.push(diff(vec![gm472_glob()], Vec::new()));
    diffs.push(diff(vec![gm472_outer()], Vec::new()));
    for (file, user, module, head, paths) in [
        ("src/user_named.rs", "krate::user_named", "krate::named", "T", true),
        ("src/user_renamed.rs", "krate::user_renamed", "krate::named", "Renamed", true),
        ("src/user_glob.rs", "krate::user_glob", "krate::glob", "T", true),
        ("src/user_outer.rs", "krate::user_outer", "krate::outer", "T", true),
        ("src/user_keyless.rs", "krate::user_keyless", "krate::named", "T", false),
    ] {
        let nodes = gm472_user(file, user, module, head, paths);
        let (field, method) = if paths { (Some(GM472_F), Some(GM472_M)) } else { (None, None) };
        let mut edges = Vec::new();
        uses(&mut edges, &nodes[0].id, "REFERENCES", &nodes[1], field);
        uses(&mut edges, &nodes[0].id, "CALLS", &nodes[2], method);
        diffs.push(diff(nodes, edges));
    }

    // Last, always: a second usage in caller.ts of an existing placeholder,
    // which the diff does not re-send.
    let reused = placeholder_node("caller.ts", "index.ts", "mutate");
    let mut edges = Vec::new();
    uses(&mut edges, "Function:caller.ts:again", "CALLS", &reused, Some("Function:target.ts:mutate"));
    diffs.push(diff(vec![symbol("caller.ts", "again", "Function", false)], edges));

    (diffs, expected)
}

/// Every usage edge (every edge but core's `DEFINES`), as `(id, toId,
/// resolved)` in id order.
fn usage_edges(conn: &Connection) -> Vec<(String, String, bool)> {
    conn.prepare("SELECT id, toId, resolved FROM edges WHERE kind <> 'DEFINES' ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The same end state built two ways - every diff applied and then one
/// `link_all`, versus `link_diff` after each diff - must link the same
/// edges, whatever order the files arrive in. The orders are the fixture's
/// own, its reverse, and 100 deterministic shuffles; the fixture's last
/// diff (a new usage onto an existing placeholder) always goes last, since
/// that placeholder has to exist first.
#[test]
fn link_all_and_link_diff_agree_on_the_same_end_state() {
    let (diffs, expected) = equivalence_fixture();
    let count = diffs.len();

    let mut bulk = setup();
    for diff in &diffs {
        apply_diff(&mut bulk, diff).unwrap();
    }
    link_all(&mut bulk).unwrap();
    let reference = usage_edges(&bulk);

    // The reference has to be the right answer itself, or agreeing with
    // it proves nothing.
    assert_eq!(reference.len(), expected.len());
    for (id, to, resolved) in &reference {
        let (_, lands) = expected.iter().find(|(edge, _)| edge == id).expect("every edge is expected");
        match lands {
            Some(target) => assert_eq!((to.as_str(), *resolved), (*target, true), "{id}"),
            None => {
                assert!(!resolved && to.starts_with("pending:"), "{id} must stay unresolved, is on {to}")
            }
        }
    }

    let movable = count - 1;
    let mut orders: Vec<Vec<usize>> = vec![(0..movable).collect(), (0..movable).rev().collect()];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..100 {
        let mut order: Vec<usize> = (0..movable).collect();
        for i in (1..movable).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            order.swap(i, (state % (i as u64 + 1)) as usize);
        }
        orders.push(order);
    }

    for mut order in orders {
        order.push(movable);
        let mut diffs: Vec<Option<Diff>> = equivalence_fixture().0.into_iter().map(Some).collect();
        let mut incremental = setup();
        for &index in &order {
            let diff = diffs[index].take().unwrap();
            apply_diff(&mut incremental, &diff).unwrap();
            link_diff(&mut incremental, &diff).unwrap();
        }
        assert_eq!(usage_edges(&incremental), reference, "diff order {order:?}");
    }
}

// --- GM-472: members reached through a re-export ---------------------------
//
// The rows below are what the Rust plugin sends for the GM-472 fixture,
// asserted at the source level by `plugins/rust/src/extractor/tests.rs`'
// `gm472_a_member_used_through_a_pub_use_is_addressed_at_the_reexporting_module`:
// `krate::a` declares `T`, its field `T.f` and its method `T::m`; `named`
// republishes `T` (also as `Renamed`), `glob` does `pub use crate::a::*`, and
// `outer` globs `named`. Each user addresses `f` and `m` by qualifiedName, with
// its `keyPath`, in the module its `use` named. See
// docs/architecture/gm-472-reexport-links.md.

const GM472_T: &str = "Type:src/a.rs:a::T";
const GM472_F: &str = "Variable:src/a.rs:a::T.f";
const GM472_M: &str = "Function:src/a.rs:a::T::m";

fn gm472_at(file: &'static str, module: &'static str) -> At<'static> {
    At { file, language: "rust", container: module, parent: Some("krate") }
}

/// A path from its first name and each later `(sep, name)`.
fn gm472_path(first: &str, rest: &[(&str, &str)]) -> QualifiedPath {
    rest.iter().fold(QualifiedPath::root(first), |path, (sep, name)| path.child(*sep, *name))
}

/// A re-export with an explicit id, for two `pub use` items in one module
/// forwarding the same target under two names.
fn gm472_reexport(at: At, published: &str, scope: &str, key: &str) -> NodeRecord {
    let mut node = container_reexport(at, published, scope, key);
    node.id = format!("{}:as:{published}", node.id);
    node.qualified_name = node.id.clone();
    node
}

/// A declaration of `krate::<module>` with its `qualifiedPath`, as the Rust
/// plugin sends it: `<module>`, then each `(sep, name)` of `rest`.
fn gm472_member(at: At, kind: &str, native_kind: &str, rest: &[(&str, &str)]) -> NodeRecord {
    let path = gm472_path(at.container.trim_start_matches("krate::"), rest);
    let mut node = member(at, kind, &path.display(), Vis::Public);
    node.native_kind = Some(native_kind.to_string());
    node.qualified_path = Some(path);
    node
}

fn gm472_type() -> NodeRecord {
    gm472_member(gm472_at("src/a.rs", "krate::a"), "Type", "struct", &[("::", "T")])
}
fn gm472_field() -> NodeRecord {
    gm472_member(gm472_at("src/a.rs", "krate::a"), "Variable", "field", &[("::", "T"), (".", "f")])
}
fn gm472_method() -> NodeRecord {
    gm472_member(gm472_at("src/a.rs", "krate::a"), "Function", "method", &[("::", "T"), ("::", "m")])
}

fn gm472_declarations() -> Vec<NodeRecord> {
    vec![gm472_type(), gm472_field(), gm472_method()]
}

fn gm472_named() -> Vec<NodeRecord> {
    vec![
        gm472_reexport(gm472_at("src/named.rs", "krate::named"), "T", "krate::a", "T"),
        gm472_reexport(gm472_at("src/named.rs", "krate::named"), "Renamed", "krate::a", "T"),
    ]
}
fn gm472_glob() -> NodeRecord {
    gm472_reexport(gm472_at("src/glob.rs", "krate::glob"), REEXPORT_ALL_NAME, "krate::a", REEXPORT_ALL_NAME)
}
fn gm472_outer() -> NodeRecord {
    gm472_reexport(
        gm472_at("src/outer.rs", "krate::outer"),
        REEXPORT_ALL_NAME,
        "krate::named",
        REEXPORT_ALL_NAME,
    )
}

fn gm472_reexports() -> Vec<NodeRecord> {
    let mut nodes = gm472_named();
    nodes.push(gm472_glob());
    nodes.push(gm472_outer());
    nodes
}

/// One user file's rows: `run`, and its placeholders for `<head>.f` and
/// `<head>::m` addressed in `module` - with their `keyPath` when `paths`.
fn gm472_user(
    file: &'static str,
    user: &'static str,
    module: &str,
    head: &str,
    paths: bool,
) -> Vec<NodeRecord> {
    let at = gm472_at(file, user);
    let run = member(at, "Function", &format!("{}::run", user.trim_start_matches("krate::")), Vis::Public);
    let prefix = module.trim_start_matches("krate::");
    let placeholder = |sep: &str, name: &str| {
        let path = gm472_path(prefix, &[("::", head), (sep, name)]);
        let mut node = container_placeholder(at, module, KEY_QUALIFIED_NAME, &path.display());
        if paths {
            node.target.as_mut().unwrap().key_path = Some(path);
        }
        node
    };
    vec![run, placeholder(".", "f"), placeholder("::", "m")]
}

/// `gm472_user`'s rows as one diff: `run` REFERENCES the field and CALLS the
/// method. Returns the diff and `(field edge, method edge)`.
fn gm472_user_diff(
    file: &'static str,
    user: &'static str,
    module: &str,
    head: &str,
    paths: bool,
) -> (Diff, (String, String)) {
    let nodes = gm472_user(file, user, module, head, paths);
    let field = usage_edge(&nodes[0].id, "REFERENCES", &nodes[1]);
    let method = usage_edge(&nodes[0].id, "CALLS", &nodes[2]);
    let ids = (field.id.clone(), method.id.clone());
    (Diff { upsert_nodes: nodes, upsert_edges: vec![field, method], ..Default::default() }, ids)
}

/// Applies one user file, with paths. Returns `(field edge, method edge)`.
fn gm472_use(
    conn: &mut Connection,
    file: &'static str,
    user: &'static str,
    module: &str,
    head: &str,
) -> (String, String) {
    let (diff, edges) = gm472_user_diff(file, user, module, head, true);
    apply_diff(conn, &diff).unwrap();
    edges
}

/// Applies `diff` and links it incrementally.
fn gm472_apply_and_link_diff(conn: &mut Connection, diff: &Diff) -> LinkSummary {
    apply_diff(conn, diff).unwrap();
    link_diff(conn, diff).unwrap()
}

fn gm472_assert_linked(conn: &Connection, (field, method): &(String, String)) {
    assert_eq!(edge_target(conn, field), (GM472_F.to_string(), true), "{field}");
    assert_eq!(edge_target(conn, method), (GM472_M.to_string(), true), "{method}");
}

fn gm472_assert_unresolved(conn: &Connection, (field, method): &(String, String)) {
    assert!(!edge_target(conn, field).1, "{field}");
    assert!(!edge_target(conn, method).1, "{method}");
}

/// Control for everything below: the same rows addressed straight at the
/// declaring module link, so an unresolved edge further down is the
/// re-export hop and nothing else about the fixture.
#[test]
fn gm472_control_a_member_addressed_at_its_own_module_links() {
    let mut conn = setup();
    upsert(&mut conn, gm472_declarations());
    let edges = gm472_use(&mut conn, "src/user_direct.rs", "krate::user_direct", "krate::a", "T");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
    gm472_assert_linked(&conn, &edges);
}

/// A named `pub use`, its `as` alias, a glob, and a two-hop glob-over-named
/// chain: all eight edges land on `a::T.f` / `a::T::m`, each by its own
/// separator - the field never on the method or the reverse.
///
/// Control: make `Resolver::resolve` return the empty walk result instead of
/// calling `through_head` - no edge links.
#[test]
fn gm472_members_through_a_reexport_link_to_the_declaration() {
    let mut conn = setup();
    let mut nodes = gm472_declarations();
    nodes.extend(gm472_reexports());
    upsert(&mut conn, nodes);
    let edges = [
        gm472_use(&mut conn, "src/user_named.rs", "krate::user_named", "krate::named", "T"),
        gm472_use(&mut conn, "src/user_renamed.rs", "krate::user_renamed", "krate::named", "Renamed"),
        gm472_use(&mut conn, "src/user_glob.rs", "krate::user_glob", "krate::glob", "T"),
        gm472_use(&mut conn, "src/user_outer.rs", "krate::user_outer", "krate::outer", "T"),
    ];

    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 8 });
    for pair in &edges {
        gm472_assert_linked(&conn, pair);
    }
}

/// A plugin that sends no `keyPath` keeps today's behaviour: its key is
/// looked up whole and never split, so nothing links through a re-export.
///
/// Control: the same rows with their paths link (the test above), so the
/// missing path is the only thing stopping them.
#[test]
fn gm472_a_key_without_a_key_path_is_not_split() {
    let mut conn = setup();
    let mut nodes = gm472_declarations();
    nodes.extend(gm472_reexports());
    upsert(&mut conn, nodes);
    let (diff, edges) = gm472_user_diff("src/user_named.rs", "krate::user_named", "krate::named", "T", false);
    apply_diff(&mut conn, &diff).unwrap();
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    gm472_assert_unresolved(&conn, &edges);
}

/// The incremental half, declarations last: the head and its members
/// arriving after the usage and the re-exports wake the waiting placeholders
/// at the re-exporting module, through a named and a two-hop chain alike.
///
/// Control: drop the `waiting_on_a_head` call from `link_diff` - 0 linked.
#[test]
fn gm472_a_late_declaration_links_through_a_reexport() {
    let mut conn = setup();
    gm472_apply_and_link_diff(&mut conn, &Diff { upsert_nodes: gm472_reexports(), ..Default::default() });
    let named = gm472_use(&mut conn, "src/user_named.rs", "krate::user_named", "krate::named", "T");
    let outer = gm472_use(&mut conn, "src/user_outer.rs", "krate::user_outer", "krate::outer", "T");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 }, "nothing to link yet");

    let declarations = Diff { upsert_nodes: gm472_declarations(), ..Default::default() };
    assert_eq!(gm472_apply_and_link_diff(&mut conn, &declarations), LinkSummary { linked_edges: 4 });
    gm472_assert_linked(&conn, &named);
    gm472_assert_linked(&conn, &outer);
}

/// The incremental half, re-export last: a `pub use` appearing in the
/// module the usage names makes its member placeholders answerable.
///
/// Control: drop the `waiting_on_a_head` call from `link_diff` - 0 linked.
#[test]
fn gm472_a_late_named_reexport_links_the_members_behind_it() {
    let mut conn = setup();
    upsert(&mut conn, gm472_declarations());
    let edges = gm472_use(&mut conn, "src/user_named.rs", "krate::user_named", "krate::named", "T");
    link_all(&mut conn).unwrap();
    gm472_assert_unresolved(&conn, &edges);

    let named = Diff { upsert_nodes: gm472_named(), ..Default::default() };
    assert_eq!(gm472_apply_and_link_diff(&mut conn, &named), LinkSummary { linked_edges: 2 });
    gm472_assert_linked(&conn, &edges);
}

/// The incremental half, member last: an edit that adds only the method
/// (`T` itself unchanged, so not in the diff) links the call waiting on it
/// through the re-export.
///
/// Control: drop the `heads_of_members` extension of the name seeds in
/// `link_diff` - 0 linked.
#[test]
fn gm472_a_late_member_links_through_a_reexport_of_its_unchanged_head() {
    let mut conn = setup();
    let mut nodes = vec![gm472_type(), gm472_field()];
    nodes.extend(gm472_reexports());
    upsert(&mut conn, nodes);
    let (field, method) = gm472_use(&mut conn, "src/user_glob.rs", "krate::user_glob", "krate::glob", "T");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 }, "the field only");

    let added = Diff { upsert_nodes: vec![gm472_method()], ..Default::default() };
    assert_eq!(gm472_apply_and_link_diff(&mut conn, &added), LinkSummary { linked_edges: 1 });
    gm472_assert_linked(&conn, &(field, method));
}

/// Two globs re-exporting each other terminate and link nothing; the same
/// cycle with a way out to `krate::a` terminates and links through it.
///
/// Control (second half): make `Resolver::resolve` skip `through_head` - the
/// edges through the cycle stay unresolved.
#[test]
fn gm472_a_glob_cycle_terminates_and_links_only_what_leaves_it() {
    let cycle = |exit: bool| {
        let mut conn = setup();
        let mut nodes = gm472_declarations();
        let (cyc_a, cyc_b) =
            (gm472_at("src/cyc_a.rs", "krate::cyc_a"), gm472_at("src/cyc_b.rs", "krate::cyc_b"));
        nodes.push(gm472_reexport(cyc_a, REEXPORT_ALL_NAME, "krate::cyc_b", REEXPORT_ALL_NAME));
        nodes.push(gm472_reexport(cyc_b, REEXPORT_ALL_NAME, "krate::cyc_a", REEXPORT_ALL_NAME));
        if exit {
            nodes.push(gm472_reexport(cyc_b, REEXPORT_ALL_NAME, "krate::a", REEXPORT_ALL_NAME));
        }
        upsert(&mut conn, nodes);
        let edges = gm472_use(&mut conn, "src/user_cyc.rs", "krate::user_cyc", "krate::cyc_a", "T");
        let summary = link_all(&mut conn).unwrap();
        (conn, edges, summary)
    };

    let (conn, edges, summary) = cycle(false);
    assert_eq!(summary, LinkSummary { linked_edges: 0 });
    gm472_assert_unresolved(&conn, &edges);

    let (conn, edges, summary) = cycle(true);
    assert_eq!(summary, LinkSummary { linked_edges: 2 });
    gm472_assert_linked(&conn, &edges);
}

/// Two globs offering two different `T`s are ambiguous - Rust itself rejects
/// the use - and the head links nothing, even though only `a::T` has an `m`.
///
/// Control: in `through_head`, take the member of every head instead of
/// requiring exactly one - the call lands on `a::T::m`.
#[test]
fn gm472_two_globs_offering_one_head_stay_unresolved() {
    let mut conn = setup();
    let mut nodes = gm472_declarations();
    nodes.push(gm472_member(gm472_at("src/b.rs", "krate::b"), "Type", "struct", &[("::", "T")]));
    let both = gm472_at("src/both.rs", "krate::both");
    nodes.push(gm472_reexport(both, REEXPORT_ALL_NAME, "krate::a", REEXPORT_ALL_NAME));
    nodes.push(gm472_reexport(both, REEXPORT_ALL_NAME, "krate::b", REEXPORT_ALL_NAME));
    upsert(&mut conn, nodes);
    let edges = gm472_use(&mut conn, "src/user_both.rs", "krate::user_both", "krate::both", "T");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    gm472_assert_unresolved(&conn, &edges);
}

/// The head alone, by name through the same chain, links onto `a::T` - the
/// walk the head of a qualifiedName key is given.
#[test]
fn gm472_the_head_by_name_links_through_the_same_chain() {
    let mut conn = setup();
    let mut nodes = gm472_declarations();
    nodes.extend(gm472_reexports());
    upsert(&mut conn, nodes);
    let at = gm472_at("src/user_outer.rs", "krate::user_outer");
    let run = member(at, "Function", "user_outer::run", Vis::Public);
    let head = use_through(
        &mut conn,
        vec![run],
        "Function:src/user_outer.rs:user_outer::run",
        "REFERENCES",
        container_placeholder(at, "krate::outer", KEY_NAME, "T"),
    );
    link_all(&mut conn).unwrap();
    assert_eq!(edge_target(&conn, &head), (GM472_T.to_string(), true));
}

// --- a type member and a free declaration of one name in one module --------
//
// Members live in their module's container beside the free declarations, so
// a name key addressed at the module finds both. A name in a module scope
// never denotes a member: the one non-member is linked, and anything else
// stays ambiguous.

const A_FREE_F: &str = "Function:src/a.rs:a::f";
const A_FREE_M: &str = "Function:src/a.rs:a::m";

fn module_a() -> At<'static> {
    gm472_at("src/a.rs", "krate::a")
}

/// `fn <name>` declared at the top of `krate::a`, in `at`'s file.
fn free_fn(at: At, name: &str) -> NodeRecord {
    gm472_member(at, "Function", "function", &[("::", name)])
}

/// `krate::user::run`'s `kind` usage of `name`, addressed at `scope` by name.
/// Returns the edge id.
fn use_by_name(conn: &mut Connection, kind: &str, scope: &str, name: &str) -> String {
    let at = gm472_at("src/user.rs", "krate::user");
    let run = member(at, "Function", "user::run", Vis::Public);
    use_through(
        conn,
        vec![run],
        "Function:src/user.rs:user::run",
        kind,
        container_placeholder(at, scope, KEY_NAME, name),
    )
}

/// Control for the rest of this section: with no member of the name around,
/// the free fn links on its own.
#[test]
fn a_free_fn_with_no_same_named_member_links() {
    let mut conn = setup();
    upsert(&mut conn, vec![gm472_type(), free_fn(module_a(), "f")]);
    let edge = use_by_name(&mut conn, "REFERENCES", "krate::a", "f");
    link_all(&mut conn).unwrap();
    assert_eq!(edge_target(&conn, &edge), (A_FREE_F.to_string(), true));
}

/// A field `T.f` and `fn f`: the `REFERENCES` a `use a::f` sends lands on
/// the fn. Control: return `Ok(None)` at the top of
/// `Resolver::sole_non_member` - the edge stays on its placeholder.
#[test]
fn a_name_beside_a_same_named_field_lands_on_the_free_fn() {
    let mut conn = setup();
    upsert(&mut conn, vec![gm472_type(), gm472_field(), free_fn(module_a(), "f")]);
    let edge = use_by_name(&mut conn, "REFERENCES", "krate::a", "f");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge), (A_FREE_F.to_string(), true));
}

/// A method `T::m` and `fn m`: both a call and a reference by name land on
/// the fn, and the method keeps nothing it did not have. Control: as above.
#[test]
fn a_name_beside_a_same_named_method_lands_on_the_free_fn() {
    let mut conn = setup();
    upsert(&mut conn, vec![gm472_type(), gm472_method(), free_fn(module_a(), "m")]);
    let call = use_by_name(&mut conn, "CALLS", "krate::a", "m");
    let reference = use_by_name(&mut conn, "REFERENCES", "krate::a", "m");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 2 });
    assert_eq!(edge_target(&conn, &call), (A_FREE_M.to_string(), true));
    assert_eq!(edge_target(&conn, &reference), (A_FREE_M.to_string(), true));
}

/// The incremental pass runs the same rule. Control: as above.
#[test]
fn a_diff_links_a_name_beside_a_same_named_method_to_the_free_fn() {
    let mut conn = setup();
    upsert(&mut conn, vec![gm472_type(), gm472_method(), free_fn(module_a(), "m")]);
    let at = gm472_at("src/user.rs", "krate::user");
    let run = member(at, "Function", "user::run", Vis::Public);
    let placeholder = container_placeholder(at, "krate::a", KEY_NAME, "m");
    let edge = usage_edge(&run.id, "CALLS", &placeholder);
    let edge_id = edge.id.clone();
    let diff = Diff { upsert_nodes: vec![run, placeholder], upsert_edges: vec![edge], ..Default::default() };
    assert_eq!(gm472_apply_and_link_diff(&mut conn, &diff), LinkSummary { linked_edges: 1 });
    assert_eq!(edge_target(&conn, &edge_id), (A_FREE_M.to_string(), true));
}

/// A name reached through a `pub use` hop (a `name` key walked into the
/// declaring module) gets the same rule at the depth it lands. Control: as
/// above.
#[test]
fn a_reexported_name_beside_a_same_named_method_lands_on_the_free_fn() {
    let mut conn = setup();
    upsert(
        &mut conn,
        vec![
            gm472_type(),
            gm472_method(),
            free_fn(module_a(), "m"),
            gm472_reexport(gm472_at("src/named.rs", "krate::named"), "m", "krate::a", "m"),
        ],
    );
    let edge = use_by_name(&mut conn, "CALLS", "krate::named", "m");
    link_all(&mut conn).unwrap();
    assert_eq!(edge_target(&conn, &edge), (A_FREE_M.to_string(), true));
}

/// The receiver type is looked up in the member's container, not its file:
/// a Go method may be declared in another file of its type's package.
/// Control: look the type up by the candidate's file (`type_in_file`) for
/// every candidate - the edge stays on its placeholder.
#[test]
fn a_member_whose_type_is_in_another_file_of_the_container_is_still_a_member() {
    let mut conn = setup();
    let types = gm472_member(gm472_at("src/a_types.rs", "krate::a"), "Type", "struct", &[("::", "T")]);
    upsert(&mut conn, vec![types, gm472_method(), free_fn(module_a(), "m")]);
    let edge = use_by_name(&mut conn, "CALLS", "krate::a", "m");
    link_all(&mut conn).unwrap();
    assert_eq!(edge_target(&conn, &edge), (A_FREE_M.to_string(), true));
}

/// A field and a method of one name are both members: nothing singles one
/// out, and the edge stays unresolved.
#[test]
fn a_name_matching_only_members_stays_unresolved() {
    let mut conn = setup();
    let getter = gm472_member(module_a(), "Function", "method", &[("::", "T"), ("::", "f")]);
    upsert(&mut conn, vec![gm472_type(), gm472_field(), getter]);
    let edge = use_by_name(&mut conn, "REFERENCES", "krate::a", "f");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    assert!(!edge_target(&conn, &edge).1, "a field and a method of one name: a name key must refuse");
}

/// Two free fns of one name beside a member are still two candidates.
/// Control: link the first non-member instead of requiring exactly one in
/// `Resolver::sole_non_member` - the edge lands on one of them.
#[test]
fn two_free_fns_of_one_name_stay_unresolved_beside_a_member() {
    let mut conn = setup();
    upsert(
        &mut conn,
        vec![
            gm472_type(),
            gm472_method(),
            free_fn(module_a(), "m"),
            free_fn(gm472_at("src/a_cfg.rs", "krate::a"), "m"),
        ],
    );
    let edge = use_by_name(&mut conn, "CALLS", "krate::a", "m");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    assert!(!edge_target(&conn, &edge).1, "two free fns: a name key must refuse");
}

/// A candidate is a member only when its path's parent is a declared
/// `Type`: with `a::T` declared as a function, `a::T::m` is as good as
/// `a::m`. Control: drop the `kind` condition from `type_in_container` - the
/// edge lands on `a::m`.
#[test]
fn a_path_parent_that_is_not_a_type_does_not_make_a_member() {
    let mut conn = setup();
    let not_a_type = free_fn(module_a(), "T");
    upsert(&mut conn, vec![not_a_type, gm472_method(), free_fn(module_a(), "m")]);
    let edge = use_by_name(&mut conn, "CALLS", "krate::a", "m");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    assert!(!edge_target(&conn, &edge).1);
}

/// A trait-impl method's own path parent is no type (`a::<T as Tr>::m`);
/// its alias `a::T::m` makes it a member. Beside an inherent `T::m` there
/// is then no non-member, and the edge stays unresolved. Control: drop the
/// alias-suffix loop from `Resolver::is_type_member` - the edge lands on
/// the trait-impl method.
#[test]
fn a_trait_impl_method_is_a_member_through_its_alias() {
    let mut conn = setup();
    let mut trait_impl =
        gm472_member(module_a(), "Function", "trait_impl_method", &[("::", "<T as Tr>"), ("::", "m")]);
    trait_impl.alias_paths = vec![gm472_path("a", &[("::", "T"), ("::", "m")])];
    upsert(&mut conn, vec![gm472_type(), gm472_method(), trait_impl]);
    let edge = use_by_name(&mut conn, "CALLS", "krate::a", "m");
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    assert!(!edge_target(&conn, &edge).1, "two methods of `T`: a name key must refuse");
}

/// A `qualifiedName` key gets no tie-break: a member and a non-member under
/// one qualifiedName stay ambiguous. Control: drop the `Key::Name` check at
/// the top of `Resolver::sole_non_member` - the edge lands on the
/// non-member.
#[test]
fn a_qualified_name_key_matching_a_member_and_a_non_member_stays_unresolved() {
    let mut conn = setup();
    let mut no_path = member(gm472_at("src/a_other.rs", "krate::a"), "Function", "a::T::m", Vis::Public);
    no_path.qualified_path = None;
    upsert(&mut conn, vec![gm472_type(), gm472_method(), no_path]);
    let at = gm472_at("src/user.rs", "krate::user");
    let run = member(at, "Function", "user::run", Vis::Public);
    let edge = use_through(
        &mut conn,
        vec![run],
        "Function:src/user.rs:user::run",
        "CALLS",
        container_placeholder(at, "krate::a", KEY_QUALIFIED_NAME, "a::T::m"),
    );
    assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 0 });
    assert!(!edge_target(&conn, &edge).1, "a qualifiedName key must refuse two declarations");
}
