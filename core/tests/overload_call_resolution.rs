//! End-to-end acceptance test for the overloads epic: a real MCP client, the
//! real shim, a real daemon, a real cold-start bulk index driven by the real
//! JS/TS plugin, and the real semantic pass driving a real `tsserver` child -
//! queried through the actual MCP tool surface, not an internal unit test or
//! a direct SQLite read.
//!
//! **What this test can and cannot prove, established by investigation
//! before writing it.** `docs/architecture/g-mesh-v1.md`'s "Overloads and
//! merged declarations" section describes `toDeclaration` (which overload a
//! call binds), per-declaration `signature`s, `declarationCount`,
//! `boundSignatures` and `boundDeclaration` as the eventual shape of
//! `find_definition`/`find_callers`/`find_callees`/`get_file_outline`'s
//! answers. As of this task, none of that is actually wired into the MCP
//! layer: `find_definition.rs`'s `DefinitionNode`, `find_callers_callees.rs`'s
//! `CallerSite`/`CalleeSite`, and `get_file_outline.rs`'s `OutlineSymbol` carry
//! only a single flat `signature` field (or none) and a `resolved` bit - no
//! field names the specific overload a call bound, or how many declarations a
//! symbol has. That data exists only as `edges.toDeclaration` and the
//! `declarations` table, readable today only by a direct SQLite query - which
//! is exactly what `overload_call_binding.rs` (checker-matching) and
//! `overload_declaration_storage.rs` (storage) already do and already prove.
//!
//! What a real caller of the real tools genuinely observes today, and what
//! this test proves instead: an overloaded function survives the whole real
//! pipeline as **one** correctly-merged symbol - not silently dropped
//! (`function_signature`'s old fate) and not split into two nodes (a class
//! method's old fate) - and **both** of its distinct overload call sites come
//! back as confirmed (`resolved: true`) callers of that one symbol, through
//! `get_file_outline`, `find_definition` and `find_callers`. The test only
//! proceeds to those assertions once a direct read of `edges.toDeclaration`
//! confirms the checker has actually bound both call sites - the same fact
//! `overload_call_binding.rs` proves - so a regression in the semantic pass
//! (a hang, a crash, a wrong ordinal count) still fails this test via timeout
//! even though the specific JSON fields asserted on afterward happen to be
//! structural. The DB peek is the readiness oracle, never the assertion.
//!
//! Fixture mirrors `overload_call_binding.rs`'s own: `parse` with two call
//! signatures and an implementation, called once per signature from a second
//! file. Structured after `ambiguous_reexport_linking.rs`.
//!
//! Requires `plugins/typescript/dist/` to be up to date, which `core/build.rs`
//! keeps so.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Same generous budget the other tsserver-backed fixtures in this release
/// use: the semantic pass runs after the structural index is already being
/// served, and pays a `tsserver` child's own startup first.
const SEMANTIC_TIMEOUT: Duration = Duration::from_secs(45);

/// Two call signatures and their implementation - ordinal 0 takes a
/// `string`, ordinal 1 a `number`; the implementation is the one signature
/// TypeScript never shows a caller, and so the one that must never appear as
/// `get_file_outline`'s reported signature.
const LIB: &str = r#"export function parse(input: string): string[];
export function parse(input: number, radix?: number): number;
export function parse(input: string | number, radix?: number): unknown {
  return typeof input === "string" ? [input] : Number(input);
}
"#;

const USE: &str = r#"import { parse } from "./lib";

export function useString(): string[] {
  return parse("x") as string[];
}

export function useNumber(): number {
  return parse(10, 16);
}
"#;

const FILES: [(&str, &str); 3] = [
    (
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "target": "ES2020" }, "include": ["."] }"#,
    ),
    ("lib.ts", LIB),
    ("use.ts", USE),
];

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self { dir: tempfile::tempdir().expect("failed to create a temp project root") };
        for (rel, contents) in FILES {
            let path = project.root().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
            std::fs::write(&path, contents).expect("failed to write a fixture file");
        }
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn db_path(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory").join("index.db")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(self.pid_file()) {
            let _ = StdCommand::new("kill").arg("-9").arg(pid.trim()).status();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// Readiness oracle only - never asserted on directly. Counts `CALLS` edges
/// the checker has bound to a specific declaration ordinal of `parse`, the
/// same query `overload_call_binding.rs` uses to prove the ordinal binding
/// itself. Used here purely to know when it is safe to make the real,
/// wire-level assertions below: before this returns 2, the semantic pass
/// simply has not finished, and any JSON pulled off the tools would be
/// judging a race rather than the feature.
fn bound_overload_call_count(db: &Path) -> usize {
    let Ok(conn) = Connection::open(db) else {
        return 0;
    };
    conn.query_row(
        "SELECT COUNT(*) FROM edges e JOIN nodes n ON n.id = e.toId
         WHERE e.kind = 'CALLS' AND e.toDeclaration IS NOT NULL
           AND e.source = 'ts-compiler' AND e.resolved = 1 AND n.name = 'parse'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) as usize
}

#[tokio::test]
async fn an_overloaded_function_resolves_correctly_through_the_real_mcp_tools() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(&root);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(&root);

    // Wait for the checker to actually bind both overload call sites before
    // asserting anything - see `bound_overload_call_count`'s doc comment for
    // why this DB peek exists and is not itself the assertion.
    let db_path = project.db_path();
    let deadline = Instant::now() + SEMANTIC_TIMEOUT;
    loop {
        if bound_overload_call_count(&db_path) == 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the semantic pass did not bind both overloaded calls within {SEMANTIC_TIMEOUT:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Assertion 1: `get_file_outline` on lib.ts must list `parse` exactly
    // once - neither dropped (the old fate of a top-level overload
    // signature, which `tree-sitter` saw as a `function_signature` node type
    // the extractor had no case for) nor split in two (the old fate of a
    // class method's overloads, since `nativeKind` used to differ between a
    // signature and its implementation). Its reported `signature` must be a
    // real call signature, never the implementation's `string | number`
    // union - the one signature TypeScript deliberately never shows a
    // caller.
    let outline = client
        .call_tool(
            CallToolRequestParams::new("get_file_outline")
                .with_arguments(json!({ "file_path": "lib.ts" }).as_object().cloned().unwrap()),
        )
        .await
        .expect("tools/call failed");
    let outline = body(&outline);
    let outline_rows = outline["results"].as_array().expect("results is not an array");
    let parse_rows: Vec<&Value> = outline_rows.iter().filter(|r| r["name"] == "parse").collect();
    assert_eq!(parse_rows.len(), 1, "an overloaded function must be listed exactly once, not dropped or split: {outline}");
    let signature = parse_rows[0]["signature"].as_str().expect("parse's outline row has no signature");
    assert_ne!(
        signature, "parse(input: string | number, radix?: number): unknown",
        "the reported signature must be a call signature, never the implementation's: {signature}"
    );
    assert!(
        signature.contains("string") || signature.contains("number"),
        "expected a real parse call signature, got: {signature}"
    );

    // Assertion 2: `find_definition` by name must resolve to a single node,
    // not an `ambiguous` candidate page - an overload set is one symbol to
    // TypeScript, and must be one symbol here too.
    let definition = client
        .call_tool(
            CallToolRequestParams::new("find_definition")
                .with_arguments(json!({ "symbol_name": "parse" }).as_object().cloned().unwrap()),
        )
        .await
        .expect("tools/call failed");
    let definition = body(&definition);
    assert!(
        definition.get("ambiguous").is_none(),
        "an overloaded function is one symbol, not an ambiguous set of same-named declarations: {definition}"
    );
    let parse_id = definition["id"].as_str().expect("find_definition returned no id").to_string();

    // Assertion 3: `find_callers` on that one node must show exactly the two
    // real call sites, `useString` and `useNumber`, each a confirmed
    // (`resolved: true`) caller of the very same merged symbol - proving a
    // real caller of the tools sees both overload call sites land correctly
    // on the one node the checker-matching semantic pass agrees is `parse`,
    // rather than one of them silently vanishing along with a dropped or
    // duplicated declaration.
    let callers = client
        .call_tool(
            CallToolRequestParams::new("find_callers")
                .with_arguments(json!({ "symbol_id": parse_id }).as_object().cloned().unwrap()),
        )
        .await
        .expect("tools/call failed");
    let callers = body(&callers);
    let mut rows: Vec<(String, bool)> = callers["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|row| {
            (
                row["qualifiedName"].as_str().expect("no qualifiedName").to_string(),
                row["resolved"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![("useNumber".to_string(), true), ("useString".to_string(), true)],
        "both overload call sites must come back as confirmed callers of the one merged symbol: {callers}"
    );
}
