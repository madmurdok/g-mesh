//! A call answered once while the store holds both of its edges.
//!
//! The first `fileChanged` of a file a plugin process never reported answers a
//! complete diff, which re-upserts the collapsed (structural) `CALLS` edge of
//! every overload call beside the bound (semantic) one until the per-file
//! semantic pass retracts it again. This file parks the round trip in that
//! window with [`HOLD_COMPUTE_FILE_ENV`] (it holds after the commit and before
//! the per-file pass is sent) and asks MCP for the callers and callees while
//! both edges are stored.
//!
//! `g-mesh init` builds the settled index in its own process, so the daemon
//! spawned afterwards has never reported `use.ts`: the edit below is its first
//! `fileChanged` for the file, and therefore a complete one. The fixtures are
//! backdated so `init` records their baselines and the daemon's startup
//! reconcile sends no round trip the hold could catch instead (see
//! `incremental_embed_outside_lock.rs`, which this follows).
//!
//! Requires the TypeScript plugin binary, which `cargo build --workspace`
//! builds.

use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::ipc;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use g_mesh::watcher::apply::HOLD_COMPUTE_FILE_ENV;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::common;

use crate::common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Covers a `tsserver` start and project load for the per-file pass.
const SEMANTIC_TIMEOUT: Duration = Duration::from_secs(60);

/// Below the hold's own 30 s cap, so the window cannot close on its own while
/// the test is still reading it.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(20);

const TSCONFIG: &str = "{ \"compilerOptions\": { \"strict\": true } }\n";

/// Ordinal 0 takes a `string`, ordinal 1 a `number`, ordinal 2 is the
/// implementation no call binds.
const LIB: &str = r#"export function parse(input: string): string[];
export function parse(input: number, radix?: number): number;
export function parse(input: string | number, radix?: number): unknown {
  return typeof input === "string" ? [input] : Number(input);
}
"#;

/// One caller per overload. `edit` changes only a trailing comment, so every
/// rewrite keeps both calls.
fn use_source(edit: usize) -> String {
    format!(
        r#"import {{ parse }} from "./lib";

export function useString(): string[] {{
  return parse("x") as string[];
}}

export function useNumber(): number {{
  return parse(10, 16);
}}
// edit {edit}
"#
    )
}

struct Project {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
    hold_file: Option<PathBuf>,
}

impl Project {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("failed to create a temp project root"),
            daemon: None,
            hold_file: None,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, relative_path: &str, contents: &str) {
        std::fs::write(self.root().join(relative_path), contents).expect("failed to write a fixture file");
    }

    fn db(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory").join("index.db")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // A failed assertion inside the window must not leave the daemon
        // parked until the hold's cap.
        if let Some(hold_file) = self.hold_file.take() {
            let _ = std::fs::remove_file(hold_file);
        }
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn init_project(root: &Path) {
    let status = Command::new(BIN)
        .arg("init")
        .current_dir(root)
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run g-mesh init");
    assert!(status.success(), "g-mesh init must succeed against the fixture project");
}

/// The hold engages only while `hold_file` exists; the daemon's environment
/// is fixed at spawn, so its path is named here and the file created later.
fn spawn_daemon(project: &mut Project, hold_file: &Path) {
    let daemon = Command::new(BIN)
        .lifeline()
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .env(HOLD_COMPUTE_FILE_ENV, hold_file)
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon");
    project.daemon = Some(daemon);
}

/// `(bound, collapsed)` `CALLS` edges onto `parse`, from one read
/// transaction so a commit between two reads cannot make them disagree.
fn edges_onto_parse(db: &Path) -> (i64, i64) {
    let read = || -> rusqlite::Result<(i64, i64)> {
        let mut conn = Connection::open(db)?;
        let tx = conn.transaction()?;
        tx.query_row(
            "SELECT COALESCE(SUM(e.toDeclaration IS NOT NULL), 0), COALESCE(SUM(e.toDeclaration IS NULL), 0)
             FROM edges e JOIN nodes n ON n.id = e.toId
             WHERE e.kind = 'CALLS' AND n.name = 'parse'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    };
    read().unwrap_or((0, 0))
}

/// Every `CALLS` edge from the function named `caller`, bound first:
/// `(toId, name of the node at toId, toDeclaration)`.
fn calls_from(db: &Path, caller: &str) -> Vec<(String, String, Option<i64>)> {
    let conn = Connection::open(db).expect("failed to open the index");
    let mut stmt = conn
        .prepare(
            "SELECT e.toId, t.name, e.toDeclaration FROM edges e
             JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId
             WHERE f.name = ?1 AND e.kind = 'CALLS'
             ORDER BY e.toDeclaration IS NULL",
        )
        .unwrap();
    stmt.query_map([caller], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn send<W: Write>(writer: &mut W, message: &Value) {
    let body = serde_json::to_vec(message).expect("request is always serializable");
    write_ndjson_frame(writer, &body).unwrap_or_else(|e| panic!("cannot send {message}: {e:#}"));
}

fn receive(reader: &mut BufReader<ipc::Stream>) -> Value {
    let frame = read_ndjson_frame(reader)
        .unwrap_or_else(|e| panic!("cannot read a response: {e:#}"))
        .expect("the daemon closed the connection instead of answering");
    serde_json::from_slice(&frame)
        .unwrap_or_else(|e| panic!("response is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame)))
}

/// One MCP `tools/call` over a fresh connection; returns the tool's JSON
/// payload.
fn call_tool(endpoint: &ipc::Endpoint, tool: &str, arguments: Value) -> Value {
    let stream =
        ipc::Stream::connect(endpoint).unwrap_or_else(|e| panic!("failed to connect to {endpoint}: {e}"));
    let mut writer = stream.try_clone().expect("cannot clone the daemon connection");
    let mut reader = BufReader::new(stream);
    send(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "g-mesh-superseded-call-tests", "version": "0" },
            },
        }),
    );
    let initialized = receive(&mut reader);
    assert_eq!(initialized["result"]["serverInfo"]["name"], "g-mesh", "unexpected initialize: {initialized}");
    send(&mut writer, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    send(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        }),
    );
    let response = receive(&mut reader);
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{tool} returned no text content: {response}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{tool} payload is not JSON ({e}): {text}"))
}

/// The `qualifiedName`s of a caller or callee page, sorted.
fn row_names(payload: &Value) -> Vec<String> {
    let mut names: Vec<String> = payload["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results array: {payload}"))
        .iter()
        .map(|row| {
            row["qualifiedName"]
                .as_str()
                .or_else(|| row["name"].as_str())
                .unwrap_or_else(|| panic!("a row without a name: {row}"))
                .to_string()
        })
        .collect();
    names.sort();
    names
}

fn file_refs(payload: &Value) -> Vec<(String, i64)> {
    payload["files"]
        .as_array()
        .unwrap_or_else(|| panic!("no files tally: {payload}"))
        .iter()
        .map(|file| (file["path"].as_str().unwrap().to_string(), file["refs"].as_i64().unwrap()))
        .collect()
}

/// Every answer `find_callers(parse)` and `find_callees(useString)` give
/// about one stored call graph: two callers in `use.ts`, one callee.
fn assert_each_call_answered_once(endpoint: &ipc::Endpoint, when: &str) {
    let callers = call_tool(endpoint, "find_callers", json!({ "symbol_name": "parse" }));
    let names = row_names(&callers);
    assert_eq!(names.len(), 2, "{when}: find_callers(parse) must list each caller once: {callers}");
    assert!(
        names.iter().any(|name| name.ends_with("useString"))
            && names.iter().any(|name| name.ends_with("useNumber")),
        "{when}: find_callers(parse) must list useString and useNumber: {callers}"
    );
    assert_eq!(
        file_refs(&callers),
        vec![("use.ts".to_string(), 2)],
        "{when}: the rows answer's files tally must count each call once: {callers}"
    );

    let first = call_tool(endpoint, "find_callers", json!({ "symbol_name": "parse", "limit": 1 }));
    assert_eq!(first["hasMore"], true, "{when}: a one-row page of two callers has more: {first}");
    assert_eq!(first["total"], 2, "{when}: a partial page's total must count each call once: {first}");

    let count = call_tool(endpoint, "find_callers", json!({ "symbol_name": "parse", "answer": "count" }));
    assert_eq!(count["total"], 2, "{when}: answer: count must count each call once: {count}");

    let files = call_tool(endpoint, "find_callers", json!({ "symbol_name": "parse", "answer": "files" }));
    assert_eq!(files["total"], 2, "{when}: answer: files total must count each call once: {files}");
    assert_eq!(
        file_refs(&files),
        vec![("use.ts".to_string(), 2)],
        "{when}: answer: files must tally each call once: {files}"
    );

    let callees = call_tool(endpoint, "find_callees", json!({ "symbol_name": "useString" }));
    let callee_names = row_names(&callees);
    assert_eq!(callee_names.len(), 1, "{when}: find_callees(useString) must list parse once: {callees}");
    assert!(callee_names[0].ends_with("parse"), "{when}: find_callees(useString) must list parse: {callees}");

    let callee_count =
        call_tool(endpoint, "find_callees", json!({ "symbol_name": "useString", "answer": "count" }));
    assert_eq!(callee_count["total"], 1, "{when}: find_callees answer: count must be 1: {callee_count}");
}

#[test]
fn a_caller_is_listed_once_while_the_first_edit_of_a_file_holds_both_its_call_edges() {
    let mut project = Project::new();
    project.write("tsconfig.json", TSCONFIG);
    project.write("lib.ts", LIB);
    project.write("use.ts", &use_source(0));
    for file in ["tsconfig.json", "lib.ts", "use.ts"] {
        common::backdate_an_hour(&project.root().join(file));
    }
    init_project(project.root());
    let db = project.db();
    assert_eq!(edges_onto_parse(&db), (2, 0), "init must settle both overload calls to their bound edges");

    let hold_file = project.root().join(".g-mesh-hold-compute");
    spawn_daemon(&mut project, &hold_file);
    let pid_file = daemon::pid_path(project.root()).expect("failed to resolve the pid file path");
    common::wait_for("the daemon to start listening", common::startup_timeout(), || pid_file.exists());
    let endpoint = daemon::endpoint(project.root()).expect("failed to resolve the daemon's endpoint");
    common::wait_until_indexed(project.root());

    // Before the hold: the settled answer, which also activates the daemon.
    assert_each_call_answered_once(&endpoint, "settled");

    std::fs::write(&hold_file, b"").expect("failed to plant the hold file");
    project.hold_file = Some(hold_file.clone());

    // Rewritten until the window opens: a write landing before the watcher
    // is registered is missed. Writes after the held one queue behind it.
    let deadline = Instant::now() + WINDOW_TIMEOUT;
    let mut edits = 0;
    loop {
        edits += 1;
        project.write("use.ts", &use_source(edits));
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(500) {
            if edges_onto_parse(&db) == (2, 2) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if edges_onto_parse(&db) == (2, 2) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the first edit of use.ts did not re-add the collapsed edges beside the bound ones after \
             {edits} edit(s); the store holds (bound, collapsed) {:?}",
            edges_onto_parse(&db)
        );
    }

    // Both edges of one call share the stored callee id, which is what the
    // read-side filter compares.
    for caller in ["useString", "useNumber"] {
        let rows = calls_from(&db, caller);
        assert_eq!(rows.len(), 2, "{caller} must hold a bound and a collapsed edge in the window: {rows:?}");
        assert!(rows[0].2.is_some() && rows[1].2.is_none(), "{caller}: one bound, one collapsed: {rows:?}");
        assert_eq!(rows[0].0, rows[1].0, "{caller}'s two edges must share the stored toId: {rows:?}");
        assert_eq!(rows[0].1, "parse", "{caller}'s edges must point at parse: {rows:?}");
    }

    assert_each_call_answered_once(&endpoint, "in the window");

    // The answers above were read while both edges were stored.
    assert_eq!(edges_onto_parse(&db), (2, 2), "the window must still be open after the MCP answers");

    std::fs::remove_file(&hold_file).expect("failed to release the hold");
    project.hold_file = None;
    common::wait_for("the per-file pass to retract the collapsed edges", SEMANTIC_TIMEOUT, || {
        edges_onto_parse(&db) == (2, 0)
    });
    assert_each_call_answered_once(&endpoint, "settled again");
}
