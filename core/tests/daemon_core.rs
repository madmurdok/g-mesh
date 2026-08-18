//! Coverage for `g-mesh daemon` itself, independent of the shim: starting it
//! for a project opens the SQLite index, registers the file watcher, and
//! serves a real MCP session over its endpoint.
//!
//! The MCP conversation here is hand-rolled newline-delimited JSON rather than
//! an `rmcp` client, on purpose - it pins the literal wire format the daemon
//! must emit, which a client that shares the server's own serialization code
//! could never catch. `mcp_e2e.rs` covers the real-client side.

use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::ipc;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use serde_json::{json, Value};

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every tool the MVP promises, whatever order the router lists them in.
const EXPECTED_TOOLS: [&str; 8] = [
    "find_callees",
    "find_callers",
    "find_definition",
    "find_implementations",
    "find_references",
    "get_dependencies",
    "get_file_outline",
    "search_code",
];

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("failed to create a temp project root") }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    /// Writes a source file *before* any daemon exists. Nothing but the
    /// cold-start bulk index can ever put such a file in the graph: the
    /// watcher is only told about changes made while it is running, and a
    /// file that was already there never makes one.
    fn seed(&self, relative_path: &str, contents: &str) {
        let path = self.root().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create a fixture directory");
        }
        std::fs::write(&path, contents).expect("failed to seed a fixture file");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn spawn_daemon(root: &Path) -> Child {
    Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon")
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Runs one MCP conversation over a fresh connection - `initialize`, the
/// `initialized` notification, then `requests` in order - and returns the
/// responses, indexed the same as `requests`.
///
/// It all happens on a helper thread so a daemon that stops answering fails
/// the test with a timeout instead of hanging it forever.
fn mcp_session(endpoint: &ipc::Endpoint, requests: Vec<Value>) -> Vec<Value> {
    let stream = ipc::Stream::connect(endpoint)
        .unwrap_or_else(|e| panic!("failed to connect to {endpoint}: {e}"));

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(converse(stream, requests));
    });

    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(responses)) => responses,
        Ok(Err(err)) => panic!("MCP session failed: {err}"),
        Err(err) => panic!("MCP session did not finish within {TIMEOUT:?}: {err}"),
    }
}

fn converse(stream: ipc::Stream, requests: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut writer =
        stream.try_clone().map_err(|e| format!("cannot clone the daemon connection: {e}"))?;
    let mut reader = BufReader::new(stream);

    send(&mut writer, &json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "g-mesh-daemon-tests", "version": "0" },
        },
    }))?;
    let initialized = receive(&mut reader)?;
    if initialized["result"]["serverInfo"]["name"] != "g-mesh" {
        return Err(format!("unexpected initialize response: {initialized}"));
    }
    send(&mut writer, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;

    let mut responses = Vec::with_capacity(requests.len());
    for request in &requests {
        send(&mut writer, request)?;
        responses.push(receive(&mut reader)?);
    }
    Ok(responses)
}

fn send<W: Write>(writer: &mut W, message: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(message).expect("request is always serializable");
    write_ndjson_frame(writer, &body).map_err(|e| format!("cannot send {message}: {e:#}"))
}

fn receive(reader: &mut BufReader<ipc::Stream>) -> Result<Value, String> {
    let frame = read_ndjson_frame(reader)
        .map_err(|e| format!("cannot read a response: {e:#}"))?
        .ok_or("the daemon closed the connection instead of answering")?;
    serde_json::from_slice(&frame)
        .map_err(|e| format!("response is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame)))
}

fn tools_list_request(id: u32) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {} })
}

fn outline_request(id: u32, file_path: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": "get_file_outline", "arguments": { "file_path": file_path } },
    })
}

/// The tool's own JSON payload, which travels as *text* inside the MCP
/// content block rather than as a nested object.
fn tool_payload(response: &Value) -> Value {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool call returned no text content: {response}"));
    serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("tool payload is not JSON ({e}): {text}"))
}

fn symbol_names(response: &Value) -> Vec<String> {
    tool_payload(response)["results"]
        .as_array()
        .unwrap_or_else(|| panic!("outline has no results array: {response}"))
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("a symbol name must be a string").to_string())
        .collect()
}

fn tool_names(response: &Value) -> Vec<String> {
    let tools = response["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("tools/list did not return a tool array: {response}"));
    let mut names: Vec<String> = tools
        .iter()
        .map(|tool| {
            // Every tool must publish an object schema, or a client has no way
            // to build a call for it - listing a name alone is not discovery.
            assert_eq!(
                tool["inputSchema"]["type"], "object",
                "tool {} has no object input schema: {tool}",
                tool["name"]
            );
            tool["name"].as_str().expect("tool name must be a string").to_string()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn daemon_opens_sqlite_watches_files_and_serves_the_mcp_tool_surface() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());

    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening", || pid_file.exists());

    let db_path = project_dir(project.root()).unwrap().join("index.db");
    assert!(db_path.exists(), "the daemon must open (and thus create) the project's SQLite file");

    let endpoint = daemon::endpoint(project.root()).unwrap();
    let responses = mcp_session(&endpoint, vec![tools_list_request(1)]);
    assert_eq!(tool_names(&responses[0]), EXPECTED_TOOLS);

    // A file write under the project root must not crash or hang the
    // daemon - proves the watcher is registered and running, even though
    // wiring its events into a reindex is a separate ticket.
    std::fs::write(project.root().join("tracked.txt"), b"hello").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let responses = mcp_session(&endpoint, vec![tools_list_request(1)]);
    assert_eq!(
        tool_names(&responses[0]),
        EXPECTED_TOOLS,
        "daemon must still serve MCP after a watched file write"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// The cold-start guarantee: a project that already had source files when its
/// daemon started ends up with them in the graph - no edit, no `touch`,
/// nothing that could have reached the watcher, so only the bulk walk can
/// have put them there.
///
/// The wait moved from the pid file to the walk's own completion marker when
/// task 105 put the socket bind ahead of the walk; what is asserted below did
/// not change at all.
#[test]
fn a_pre_existing_project_is_indexed_by_the_cold_start_walk_alone() {
    let project = Project::new();
    project.seed(
        "src/greeter.ts",
        "export function greet(name: string): string {\n  \
           return `hello ${name}`;\n\
         }\n\n\
         export class Greeter {\n  \
           run(): string {\n    \
             return greet(\"world\");\n  \
           }\n\
         }\n",
    );
    project.seed("src/util.ts", "export const VERSION = \"1\";\n");

    let mut daemon = spawn_daemon(project.root());
    // Waiting on the walk's completion marker rather than on the pid file:
    // the pid file now appears at the bind, before the walk, so it says the
    // daemon is reachable and nothing about whether it can answer yet. There
    // is no sleep here, and there must not be.
    wait_until_indexed(project.root());

    let endpoint = daemon::endpoint(project.root()).unwrap();
    let responses = mcp_session(&endpoint, vec![outline_request(1, "src/greeter.ts")]);

    assert_eq!(
        responses[0]["result"]["isError"], false,
        "a file that existed before the daemon must be in the index: {}",
        responses[0]
    );
    let names = symbol_names(&responses[0]);
    assert!(names.contains(&"greet".to_string()), "outline is missing `greet`: {names:?}");
    assert!(names.contains(&"Greeter".to_string()), "outline is missing `Greeter`: {names:?}");

    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// The other half of that guarantee: the walk is a *cold start*, not a
/// startup routine - a restart against an already-indexed project must not
/// re-crawl the whole tree. Proven here by a file added while no daemon was
/// running that nothing ever asks about (`src/third.ts`): if the second start
/// had walked the project again, it would be in the index; it must not be.
///
/// `src/second.ts`, also added while no daemon was running, *is* asked about
/// (via `get_file_outline`) and *is* expected to come back - not because the
/// restart walked the project, but because `watcher::staleness::ensure_fresh`
/// (task 117) now runs a per-file mtime/hash check before that one handler
/// answers and synchronously reindexes just the file the query named. That is
/// what keeps this consistent with the guarantee this test is really about:
/// `src/third.ts` proves no bulk walk happened; `src/second.ts` proves a
/// query-time, single-file catch-up is not the same thing as one.
#[test]
fn a_restart_against_an_already_indexed_project_does_not_walk_it_again() {
    let project = Project::new();
    project.seed("src/first.ts", "export function first(): number {\n  return 1;\n}\n");

    let mut first_daemon = spawn_daemon(project.root());
    let pid_file = daemon::pid_path(project.root()).unwrap();
    // The first daemon has to be allowed to *finish* its walk: killed part
    // way through it would leave the completion marker unset, and the second
    // start would legitimately walk again - which is the very thing this test
    // is here to rule out.
    wait_until_indexed(project.root());
    let _ = first_daemon.kill();
    let _ = first_daemon.wait();
    // Otherwise the wait for the *second* daemon would be satisfied by the
    // dead one's file, and the session below could race the real startup.
    std::fs::remove_file(&pid_file).expect("failed to clear the stale pid file");

    // Both written with nothing watching and nothing serving: only a second
    // full walk could get either into the index on its own. `second.ts` is
    // queried below and must be caught by the per-file staleness check;
    // `third.ts` is never queried by anything and must stay uncaught by it -
    // that asymmetry is what tells a full re-walk apart from one.
    project.seed("src/second.ts", "export function second(): number {\n  return 2;\n}\n");
    project.seed("src/third.ts", "export function third(): number {\n  return 3;\n}\n");

    let mut second_daemon = spawn_daemon(project.root());
    wait_for("the second daemon to start listening", || pid_file.exists());

    let endpoint = daemon::endpoint(project.root()).unwrap();
    let responses = mcp_session(
        &endpoint,
        vec![outline_request(1, "src/first.ts"), outline_request(2, "src/second.ts")],
    );

    assert_eq!(
        responses[0]["result"]["isError"], false,
        "the first walk's index must survive the restart: {}",
        responses[0]
    );
    assert_eq!(symbol_names(&responses[0]), vec!["first".to_string()]);
    assert_eq!(
        responses[1]["result"]["isError"], false,
        "a file added while no daemon was running must still be caught by the query-time \
         staleness check once something asks about it: {}",
        responses[1]
    );
    assert_eq!(symbol_names(&responses[1]), vec!["second".to_string()]);

    // The file nothing asked about must still be missing - the query-time
    // check above reindexed exactly the one file it was asked about, not the
    // project.
    let db_path = project_dir(project.root()).unwrap().join("index.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let third_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM nodes WHERE name = 'third'", [], |row| row.get(0)).unwrap();
    assert_eq!(third_count, 0, "a file nothing queried must not appear, or the restart walked the project after all");

    let _ = second_daemon.kill();
    let _ = second_daemon.wait();
}

#[test]
fn unknown_method_gets_a_json_rpc_error_not_a_crash() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening", || pid_file.exists());

    let endpoint = daemon::endpoint(project.root()).unwrap();
    // The follow-up tools/list on the *same* connection is the point of the
    // test: a garbage request has to be answered and shrugged off, not take
    // the session down with it.
    let responses = mcp_session(
        &endpoint,
        vec![
            json!({ "jsonrpc": "2.0", "id": 1, "method": "not_a_real_method" }),
            tools_list_request(2),
        ],
    );

    assert_eq!(responses[0]["error"]["code"], -32601);
    assert_eq!(tool_names(&responses[1]), EXPECTED_TOOLS);

    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// A tool call the server refuses (here: `get_dependencies` with no anchor to
/// start from) has to come back as a tool-level error *result* and leave the
/// session usable - the framing this pins is what every handler's error paths
/// rely on, and it used to be pinned against the last not-yet-implemented
/// tool, back when there was one.
#[test]
fn a_rejected_tool_call_reports_an_error_result_rather_than_failing_the_session() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    // The refusal this pins is the handler's own; a call that arrived while
    // the cold-start walk was still running would be refused by the
    // still-indexing guard ahead of it instead, and prove nothing.
    wait_until_indexed(project.root());

    let endpoint = daemon::endpoint(project.root()).unwrap();
    let responses = mcp_session(
        &endpoint,
        vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": "get_dependencies", "arguments": { "direction": "Outgoing" } },
            }),
            tools_list_request(2),
        ],
    );

    // Tool-level error, not a JSON-RPC one: MCP clients render protocol
    // errors opaquely, so what went wrong has to travel as content.
    assert_eq!(responses[0]["result"]["isError"], true);
    assert!(
        responses[0]["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("file_path")),
        "the error must name what the call was missing: {}",
        responses[0]
    );
    assert_eq!(tool_names(&responses[1]), EXPECTED_TOOLS);

    let _ = daemon.kill();
    let _ = daemon.wait();
}
