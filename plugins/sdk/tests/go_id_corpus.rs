//! `plugins/go/testdata/ids.json` is the id corpus the Go plugin's
//! `ids_test.go` checks `nodeIDFor`/`edgeIDFor` against. This test holds the
//! file equal to what `sdk::ids` computes for the same tuples, so a change to
//! either side's scheme fails one of the two tests instead of drifting apart.
//!
//! Regenerate the file with `G_MESH_BLESS=1 cargo test -p g-mesh-plugin-sdk
//! --test go_id_corpus`.

use std::fs;
use std::path::Path;

use g_mesh_plugin_sdk::ids::{edge_id, edge_kind_wire_value, node_id, node_kind_wire_value};
use g_mesh_plugin_sdk::wire::{EdgeKind, NodeKind};
use serde_json::{json, Value};

/// `(filePath, kind, qualifiedName, nativeKind)`.
const NODES: &[(&str, NodeKind, &str, Option<&str>)] = &[
    ("main.go", NodeKind::File, "main.go", None),
    ("pkg/a.go", NodeKind::Function, "pkg.Do", Some("method")),
    ("pkg/a.go", NodeKind::Type, "pkg.T", Some("")),
    ("", NodeKind::Variable, "", None),
    ("dir with space/file name.go", NodeKind::Module, "mod path", None),
    ("pkg/naïve.go", NodeKind::Type, "Naïve", Some("struct")),
    // These two join to the same string with a space separator.
    ("p", NodeKind::Function, "x y", Some("z")),
    ("p", NodeKind::Function, "x", Some("y z")),
];

/// `(fromId, kind, toId, toDeclaration)`.
const EDGES: &[(&str, EdgeKind, &str, Option<u32>)] = &[
    ("n1", EdgeKind::Defines, "n2", None),
    ("n1", EdgeKind::Calls, "n2", Some(0)),
    ("n1", EdgeKind::Calls, "n2", Some(2)),
    ("from id", EdgeKind::SupertypeOf, "to id", None),
    ("", EdgeKind::References, "", None),
    // These two join to the same string with a space separator.
    ("f", EdgeKind::Calls, "t 1", None),
    ("f", EdgeKind::Calls, "t", Some(1)),
];

fn corpus() -> String {
    let nodes: Vec<Value> = NODES
        .iter()
        .map(|&(path, kind, name, native)| {
            json!({
                "filePath": path,
                "kind": node_kind_wire_value(kind),
                "qualifiedName": name,
                "nativeKind": native.unwrap_or(""),
                "id": node_id(path, kind, name, native),
            })
        })
        .collect();
    let edges: Vec<Value> = EDGES
        .iter()
        .map(|&(from, kind, to, ordinal)| {
            json!({
                "fromId": from,
                "kind": edge_kind_wire_value(kind),
                "toId": to,
                "toDeclaration": ordinal,
                "id": edge_id(from, kind, to, ordinal),
            })
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({ "nodes": nodes, "edges": edges })).unwrap();
    text.push('\n');
    text
}

#[test]
fn the_go_id_corpus_matches_the_sdk_scheme() {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../go/testdata/ids.json"));
    let expected = corpus();
    if std::env::var_os("G_MESH_BLESS").is_some() {
        fs::write(path, &expected).unwrap();
    }
    let on_disk =
        fs::read_to_string(path).expect("plugins/go/testdata/ids.json is missing; run with G_MESH_BLESS=1");
    assert_eq!(on_disk, expected, "plugins/go/testdata/ids.json is stale; regenerate it with G_MESH_BLESS=1");
}
