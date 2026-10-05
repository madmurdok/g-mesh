//! A real vtsls binds each call of an overloaded function to the overload
//! the call matches, through the bridge's containment path: `definition` at
//! the call lands inside one signature's range, and the edge carries that
//! declaration's ordinal.
//!
//! vtsls is a test dependency of this crate (`scripts/test-deps.sh
//! typescript`); with none installed the test fails naming that command.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::Fixture;
use g_mesh_plugin_sdk::lsp::{LspBridge, SemanticConfig};
use g_mesh_plugin_sdk::wire::{EdgeKind, SourceTier, WireEdge};
use g_mesh_plugin_sdk::{Extractor, RelPath, SdkIndex, SemanticAnswer, SemanticEngine};
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;

const OVERLOAD_TS: &str = "export function format(value: string): string;\n\
                           export function format(value: number): string;\n\
                           export function format(value: string | number): string {\n\
                           \x20 return String(value);\n\
                           }\n\
                           \n\
                           export function here(): string {\n\
                           \x20 return format(\"a\") + format(1);\n\
                           }\n";

const USE_TS: &str = "import { format } from \"./overload\";\n\
                      \n\
                      export function there(): string {\n\
                      \x20 return format(\"a\") + format(1);\n\
                      }\n";

const TSCONFIG: &str =
    "{ \"compilerOptions\": { \"strict\": true, \"module\": \"esnext\" }, \"include\": [\"src\"] }\n";

const FILES: [&str; 2] = ["src/overload.ts", "src/use.ts"];

fn index(fixture: &Fixture) -> SdkIndex {
    let project = TypeScriptExtractor.load_project(&fixture.root).expect("the project model loads");
    let mut index = SdkIndex::new();
    for path in FILES {
        let source = std::fs::read_to_string(fixture.root.join(path)).unwrap();
        let path = RelPath::new(path);
        let graph = TypeScriptExtractor.extract(&project, &path, &source);
        index.insert(path, source, graph);
    }
    index
}

/// The shipped `[plugin.semantic]`, with the command pointed at a real vtsls.
fn shipped_config() -> SemanticConfig {
    let manifest = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/plugin.toml"));
    let mut config = SemanticConfig::from_manifest_at(manifest)
        .expect("plugin.toml parses")
        .expect("plugin.toml has [plugin.semantic]");
    config.command = vtsls();
    config
}

/// This crate's `node_modules/.bin` vtsls, then `PATH`'s, proved by
/// `--version`.
fn vtsls() -> PathBuf {
    let local = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/node_modules/.bin"));
    let extensions: &[&str] = if cfg!(windows) { &["", ".cmd"] } else { &[""] };
    let mut tried = Vec::new();
    for dir in [Some(local), None] {
        for extension in extensions {
            let server = match dir {
                Some(dir) => dir.join(format!("vtsls{extension}")),
                None => PathBuf::from(format!("vtsls{extension}")),
            };
            tried.push(server.clone());
            let usable = std::process::Command::new(&server)
                .arg("--version")
                .output()
                .is_ok_and(|output| output.status.success());
            if usable {
                return server;
            }
        }
    }
    panic!(
        "this test drives a real vtsls and there is none that works (tried {tried:?}). Install it \
         with `scripts/test-deps.sh typescript` from the repository root."
    )
}

fn node_id(index: &SdkIndex, file: &str, qualified_name: &str) -> String {
    let graph = index.graph(&RelPath::new(file)).expect("the file is indexed");
    graph
        .nodes
        .iter()
        .find(|node| node.qualified_name == qualified_name)
        .unwrap_or_else(|| panic!("no {qualified_name} in {file}"))
        .id
        .clone()
}

/// `(target name, ordinal)` of every semantic `CALLS` edge from `caller`.
fn bound(answer: &SemanticAnswer, caller: &str) -> BTreeSet<(String, Option<u32>)> {
    let name_of = |edge: &WireEdge| {
        answer
            .diff
            .upsert_nodes
            .iter()
            .find(|node| node.id == edge.to_id)
            .map(|node| node.name.clone())
            .unwrap_or_else(|| panic!("edge {edge:?} lands on a placeholder the answer emits"))
    };
    answer
        .diff
        .upsert_edges
        .iter()
        .filter(|edge| {
            edge.from_id == caller && edge.source == SourceTier::Semantic && edge.kind == EdgeKind::Calls
        })
        .map(|edge| (name_of(edge), edge.to_declaration))
        .collect()
}

#[test]
fn vtsls_binds_each_overloaded_call_to_the_signature_it_matches() {
    let fixture = Fixture::new(&[
        ("tsconfig.json", TSCONFIG),
        ("src/overload.ts", OVERLOAD_TS),
        ("src/use.ts", USE_TS),
    ]);
    let index = index(&fixture);
    let root = fixture.root.canonicalize().unwrap();
    let mut bridge = LspBridge::new("typescript", &root, shipped_config());

    let answer = bridge.answer(&[], &index).expect("the bridge answers");
    assert!(answer.complete, "{:?}", answer.reason);

    // `format("a")` matches the first signature, `format(1)` the second.
    let expected: BTreeSet<(String, Option<u32>)> =
        [("format", Some(0)), ("format", Some(1))].map(|(name, at)| (name.to_string(), at)).into();
    let here = node_id(&index, "src/overload.ts", "here");
    let there = node_id(&index, "src/use.ts", "there");
    assert_eq!(bound(&answer, &here), expected, "same file: {:#?}", answer.diff);
    assert_eq!(bound(&answer, &there), expected, "through an import: {:#?}", answer.diff);
}
