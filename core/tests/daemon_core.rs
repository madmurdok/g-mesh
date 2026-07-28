//! Coverage for `g-mesh daemon` itself, independent of the shim: starting it
//! for a project opens the SQLite index, registers the file watcher, and
//! serves a real MCP session over its socket.
//!
//! The MCP conversation here is hand-rolled newline-delimited JSON rather than
//! an `rmcp` client, on purpose - it pins the literal wire format the daemon
//! must emit, which a client that shares the server's own serialization code
//! could never catch. `mcp_e2e.rs` covers the real-client side.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Every tool the MVP promises, whatever order the router lists them in.
const EXPECTED_TOOLS: [&str; 7] = [
    "find_callees",
    "find_callers",
    "find_definition",
    "find_implementations",
    "find_references",
    "get_dependencies",
    "get_file_outline",
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
fn mcp_session(socket: &Path, requests: Vec<Value>) -> Vec<Value> {
    let stream = UnixStream::connect(socket)
        .unwrap_or_else(|e| panic!("failed to connect to {}: {e}", socket.display()));

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

fn converse(stream: UnixStream, requests: Vec<Value>) -> Result<Vec<Value>, String> {
    let mut writer = stream.try_clone().map_err(|e| format!("cannot clone socket: {e}"))?;
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

fn receive(reader: &mut BufReader<UnixStream>) -> Result<Value, String> {
    let frame = read_ndjson_frame(reader)
        .map_err(|e| format!("cannot read a response: {e:#}"))?
        .ok_or("the daemon closed the connection instead of answering")?;
    serde_json::from_slice(&frame)
        .map_err(|e| format!("response is not valid JSON ({e}): {}", String::from_utf8_lossy(&frame)))
}

fn tools_list_request(id: u32) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {} })
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

    let socket = daemon::socket_path(project.root()).unwrap();
    let responses = mcp_session(&socket, vec![tools_list_request(1)]);
    assert_eq!(tool_names(&responses[0]), EXPECTED_TOOLS);

    // A file write under the project root must not crash or hang the
    // daemon - proves the watcher is registered and running, even though
    // wiring its events into a reindex is a separate ticket.
    std::fs::write(project.root().join("tracked.txt"), b"hello").unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let responses = mcp_session(&socket, vec![tools_list_request(1)]);
    assert_eq!(
        tool_names(&responses[0]),
        EXPECTED_TOOLS,
        "daemon must still serve MCP after a watched file write"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn unknown_method_gets_a_json_rpc_error_not_a_crash() {
    let project = Project::new();
    let mut daemon = spawn_daemon(project.root());
    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening", || pid_file.exists());

    let socket = daemon::socket_path(project.root()).unwrap();
    // The follow-up tools/list on the *same* connection is the point of the
    // test: a garbage request has to be answered and shrugged off, not take
    // the session down with it.
    let responses = mcp_session(
        &socket,
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
    let pid_file = daemon::pid_path(project.root()).unwrap();
    wait_for("the daemon to start listening", || pid_file.exists());

    let socket = daemon::socket_path(project.root()).unwrap();
    let responses = mcp_session(
        &socket,
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
