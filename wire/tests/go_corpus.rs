//! `plugins/go/testdata/wire.json` holds one frame per wire shape, serialized
//! from the `wire` types. The Go plugin's `wire_corpus_test.go` decodes each
//! frame into its own types and encodes it back, so a field renamed or added
//! on either side fails a test. This test holds the file equal to what the
//! types produce.
//!
//! Regenerate the file with `G_MESH_BLESS=1 cargo test -p g-mesh-wire --test go_corpus`.

use std::fs;
use std::path::Path;

use g_mesh_wire::{
    AckResponse, ControlEnvelope, ControlErrorResponse, ControlMessage, EdgeKind, FileChangeDiff,
    FileChangeResponse, ImportMatch, ImportSelector, LinkedEdge, Matcher, NodeKind, PathScope, PathSegment,
    PlaceholderTarget, Position, QualifiedPath, Range, RequestId, ResolutionChangedResponse,
    ResolutionChangedResult, ResolutionDelta, ResolutionFactsTrailer, SourceTier, TargetKey, TargetScope,
    Visibility, WireDeclaration, WireEdge, WireNode, JSONRPC_VERSION,
};
use serde_json::{json, Value};

fn named<T: serde::Serialize>(name: &str, frame: &T) -> Value {
    json!({ "name": name, "frame": serde_json::to_value(frame).unwrap() })
}

fn range(line: u32) -> Range {
    Range { start: Position { line, col: 0 }, end: Position { line: line + 2, col: 1 } }
}

fn minimal_node() -> WireNode {
    WireNode {
        id: "n1".into(),
        kind: NodeKind::Function,
        name: "Do".into(),
        qualified_name: "pkg.Do".into(),
        file_path: "pkg/a.go".into(),
        range: range(3),
        signature: None,
        visibility: Visibility::File,
        doc_comment: None,
        language: "go".into(),
        native_kind: None,
        has_syntax_errors: false,
        declarations: None,
        container: None,
        container_parent: None,
        target: None,
        qualified_path: None,
        alias_paths: Vec::new(),
        untyped_calls: Vec::new(),
        generated: false,
    }
}

fn full_node() -> WireNode {
    let path = |names: &[&str]| {
        QualifiedPath(
            names
                .iter()
                .enumerate()
                .map(|(i, name)| PathSegment {
                    sep: (i > 0).then(|| ".".to_string()),
                    name: name.to_string(),
                })
                .collect(),
        )
    };
    WireNode {
        signature: Some("func Do(x int) error".into()),
        visibility: Visibility::Container("example.com/m/pkg".into()),
        doc_comment: Some("Do does it.".into()),
        native_kind: Some("method".into()),
        has_syntax_errors: true,
        declarations: Some(vec![
            WireDeclaration {
                ordinal: 0,
                start_line: 1,
                start_col: 0,
                end_line: 2,
                end_col: 1,
                signature: Some("func Do()".into()),
                has_body: true,
            },
            WireDeclaration {
                ordinal: 1,
                start_line: 4,
                start_col: 0,
                end_line: 4,
                end_col: 9,
                signature: None,
                has_body: false,
            },
        ]),
        container: Some("example.com/m/pkg".into()),
        container_parent: Some("example.com/m".into()),
        target: Some(PlaceholderTarget {
            scope: TargetScope::Container("example.com/m/other".into()),
            key: TargetKey::QualifiedName("other.Thing".into()),
            from_container: Some("example.com/m/pkg".into()),
            key_path: Some(path(&["other", "Thing"])),
            also_scopes: vec!["example.com/m/third".into()],
        }),
        qualified_path: Some(path(&["pkg", "Do"])),
        alias_paths: vec![path(&["alias", "Do"])],
        untyped_calls: vec!["Helper".into()],
        generated: true,
        ..minimal_node()
    }
}

fn edge(id: &str) -> WireEdge {
    WireEdge {
        id: id.into(),
        from_id: "n1".into(),
        to_id: "n2".into(),
        kind: EdgeKind::Calls,
        source: SourceTier::Syntactic,
        engine: "tree-sitter".into(),
        resolved: false,
        to_declaration: None,
        specifier: None,
    }
}

fn diff() -> FileChangeDiff {
    FileChangeDiff {
        upsert_nodes: vec![minimal_node()],
        delete_node_ids: vec!["gone".into()],
        upsert_edges: vec![edge("e1")],
        delete_edge_ids: vec!["gone-edge".into()],
        complete: false,
        affected: None,
    }
}

fn response(id: RequestId, diff: FileChangeDiff) -> FileChangeResponse {
    FileChangeResponse {
        jsonrpc: JSONRPC_VERSION.into(),
        id,
        result: diff,
        incomplete: false,
        incomplete_reason: None,
        unfinished_files: None,
    }
}

fn request(id: Option<RequestId>, message: ControlMessage) -> ControlEnvelope {
    ControlEnvelope { jsonrpc: JSONRPC_VERSION.into(), id, message }
}

fn corpus() -> String {
    let nodes = vec![named("minimal", &minimal_node()), named("every optional field", &full_node())];

    let edges = vec![
        named("minimal", &edge("e1")),
        named(
            "semantic with toDeclaration",
            &WireEdge {
                source: SourceTier::Semantic,
                engine: "gopls".into(),
                resolved: true,
                to_declaration: Some(1),
                ..edge("e2")
            },
        ),
        named(
            "import with specifier",
            &WireEdge {
                kind: EdgeKind::Imports,
                specifier: Some("example.com/m/other".into()),
                ..edge("e3")
            },
        ),
    ];

    let trailers = vec![
        named(
            "facts only",
            &ResolutionFactsTrailer { resolution_facts: "facts-blob".into(), watch_paths: Vec::new() },
        ),
        named(
            "facts and watch paths",
            &ResolutionFactsTrailer {
                resolution_facts: "facts-blob".into(),
                watch_paths: vec!["go.mod".into(), "go.work".into()],
            },
        ),
    ];

    let linked = vec![LinkedEdge { edge_id: "e1".into(), to_id: "n9".into() }];
    let requests = vec![
        named(
            "reindex",
            &request(Some(RequestId::Number(1)), ControlMessage::Reindex { file_path: "a.go".into() }),
        ),
        named(
            "fileChanged",
            &request(
                Some(RequestId::Number(2)),
                ControlMessage::FileChanged { file_path: "a.go".into(), reextract: false },
            ),
        ),
        named(
            "fileChanged reextract",
            &request(
                Some(RequestId::String("r-3".into())),
                ControlMessage::FileChanged { file_path: "a.go".into(), reextract: true },
            ),
        ),
        named(
            "fileChanged notification",
            &request(None, ControlMessage::FileChanged { file_path: "a.go".into(), reextract: false }),
        ),
        named("status", &request(Some(RequestId::Number(4)), ControlMessage::Status)),
        named(
            "semanticPass whole project",
            &request(
                Some(RequestId::Number(5)),
                ControlMessage::SemanticPass {
                    file_paths: Vec::new(),
                    linked_edges: Vec::new(),
                    budget_ms: None,
                },
            ),
        ),
        named(
            "semanticPass with every optional field",
            &request(
                Some(RequestId::Number(6)),
                ControlMessage::SemanticPass {
                    file_paths: vec!["a.go".into(), "b/c.go".into()],
                    linked_edges: linked,
                    budget_ms: Some(30000),
                },
            ),
        ),
        named(
            "workspaceChanged",
            &request(None, ControlMessage::WorkspaceChanged { file_path: "go.mod".into() }),
        ),
        named("prepareSemanticPass", &request(None, ControlMessage::PrepareSemanticPass)),
        named(
            "filesCreated",
            &request(None, ControlMessage::FilesCreated { file_paths: vec!["a.go".into(), "b.go".into()] }),
        ),
        named(
            "resolutionChanged without facts",
            &request(
                Some(RequestId::Number(7)),
                ControlMessage::ResolutionChanged { file_path: "go.mod".into(), previous_facts: None },
            ),
        ),
        named(
            "resolutionChanged with facts",
            &request(
                Some(RequestId::Number(8)),
                ControlMessage::ResolutionChanged {
                    file_path: "go.mod".into(),
                    previous_facts: Some("facts-blob".into()),
                },
            ),
        ),
    ];

    let affected = ResolutionDelta::Affected {
        files: vec![PathScope { under: "pkg".into(), not_under: vec!["pkg/vendor".into()] }],
        imports: vec![ImportSelector {
            importers: PathScope { under: String::new(), not_under: Vec::new() },
            by: ImportMatch::Specifier(Matcher::Under {
                prefix: "example.com/m".into(),
                separator: "/".into(),
            }),
        }],
    };
    let delta_result = |delta| ResolutionChangedResult { delta, facts: None, watch_paths: Vec::new() };
    let delta_response = |id, result| ResolutionChangedResponse {
        jsonrpc: JSONRPC_VERSION.into(),
        id: RequestId::Number(id),
        result,
    };

    let responses = vec![
        named("fileChanged diff", &response(RequestId::Number(2), diff())),
        named(
            "fileChanged complete",
            &response(RequestId::Number(2), FileChangeDiff { complete: true, ..diff() }),
        ),
        named(
            "semanticPass incomplete",
            &FileChangeResponse {
                incomplete: true,
                incomplete_reason: Some("budget exhausted".into()),
                unfinished_files: Some(vec!["a.go".into()]),
                ..response(RequestId::Number(5), FileChangeDiff::default())
            },
        ),
        named(
            "semanticPass every file finished",
            &FileChangeResponse {
                unfinished_files: Some(Vec::new()),
                ..response(RequestId::String("p-1".into()), FileChangeDiff::default())
            },
        ),
        named(
            "fileChanged affected",
            &response(
                RequestId::Number(2),
                FileChangeDiff {
                    affected: Some(ResolutionDelta::Unknown { reason: "mod item moved".into() }),
                    ..diff()
                },
            ),
        ),
        named("resolutionChanged unchanged", &delta_response(7, delta_result(ResolutionDelta::Unchanged))),
        named(
            "resolutionChanged unknown with facts",
            &delta_response(
                7,
                ResolutionChangedResult {
                    facts: Some("facts-blob".into()),
                    ..delta_result(ResolutionDelta::Unknown { reason: "module renamed".into() })
                },
            ),
        ),
        named(
            "resolutionChanged affected with watch paths",
            &delta_response(
                8,
                ResolutionChangedResult {
                    facts: Some("facts-blob".into()),
                    watch_paths: vec!["go.work".into()],
                    ..delta_result(affected)
                },
            ),
        ),
        named("ack", &AckResponse::new(RequestId::Number(1))),
        named("ack string id", &AckResponse::new(RequestId::String("a-1".into()))),
    ];

    let errors = vec![
        named(
            "invalid params",
            &ControlErrorResponse::invalid_params(RequestId::Number(9), "missing field `filePath`"),
        ),
        named(
            "invalid params string id",
            &ControlErrorResponse::invalid_params(RequestId::String("e-1".into()), "empty list"),
        ),
    ];

    let mut text = serde_json::to_string_pretty(&json!({
        "nodes": nodes,
        "edges": edges,
        "trailers": trailers,
        "requests": requests,
        "responses": responses,
        "errors": errors,
    }))
    .unwrap();
    text.push('\n');
    text
}

#[test]
fn the_go_wire_corpus_matches_the_wire_types() {
    let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../plugins/go/testdata/wire.json"));
    let expected = corpus();
    if std::env::var_os("G_MESH_BLESS").is_some() {
        fs::write(path, &expected).unwrap();
    }
    let on_disk =
        fs::read_to_string(path).expect("plugins/go/testdata/wire.json is missing; run with G_MESH_BLESS=1");
    assert_eq!(
        on_disk, expected,
        "plugins/go/testdata/wire.json is stale; regenerate it with G_MESH_BLESS=1"
    );
}
