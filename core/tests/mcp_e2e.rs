//! The acceptance test for the MCP surface: a real `rmcp` client spawns the
//! real `g-mesh mcp-shim` binary, which bootstraps a real daemon, and the
//! whole MCP handshake happens over that chain.
//!
//! A genuine client rather than hand-written JSON on purpose - it exercises
//! the exact path a real MCP client takes, and cannot be subtly wrong about
//! the protocol in the same way a hand-rolled probe can. `daemon_core.rs` and
//! `shim_bootstrap.rs` keep the hand-rolled probes, so the literal wire format
//! stays pinned by something that does not share the server's own code.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use tokio::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(10);

/// Name plus the parameters a caller must supply - the half of each schema a
/// follow-up ticket is not allowed to quietly change.
const EXPECTED_TOOLS: [(&str, &[&str]); 7] = [
    ("find_callees", &["symbol_id", "cursor"]),
    ("find_callers", &["symbol_id", "cursor"]),
    ("find_definition", &["symbol_name", "file_path", "position", "cursor"]),
    ("find_implementations", &["symbol_id", "cursor"]),
    ("find_references", &["symbol_id", "cursor"]),
    (
        "get_dependencies",
        &["file_path", "module_id", "direction", "max_depth", "max_fanout", "resume_token"],
    ),
    ("get_file_outline", &["file_path", "cursor"]),
];

/// Temp project root plus teardown of the daemon the shim bootstraps for it
/// and the `~/.g-mesh/projects/<hash>/` directory it lives in.
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

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    fn daemon_pid(&self) -> u32 {
        std::fs::read_to_string(self.pid_file())
            .expect("no daemon pid file - the shim never bootstrapped one")
            .trim()
            .parse()
            .expect("pid file does not contain a pid")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if self.pid_file().exists() {
            let pid = self.daemon_pid();
            let _ = StdCommand::new("kill").arg("-9").arg(pid.to_string()).status();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

fn is_alive(pid: u32) -> bool {
    StdCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .expect("failed to signal a process")
        .success()
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test]
async fn a_real_mcp_client_discovers_and_calls_the_tool_surface_through_the_shim() {
    let project = Project::new();
    let root = project.root().to_path_buf();

    // Same spawn shape the MCP client of a real editor uses: the project
    // directory as cwd is the shim's entire notion of project identity.
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim").current_dir(&root);
    }))
    .expect("failed to spawn the shim");

    let client = ().serve(transport).await.expect("MCP initialization failed");

    let info = client.peer_info().expect("server never reported its info");
    assert_eq!(info.server_info.name, "g-mesh");
    assert!(info.capabilities.tools.is_some(), "a server with tools must advertise them");

    let listed = client.list_tools(None).await.expect("tools/list failed");
    let mut tools = listed.tools;
    tools.sort_by(|a, b| a.name.cmp(&b.name));

    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    let expected_names: Vec<&str> = EXPECTED_TOOLS.iter().map(|(name, _)| *name).collect();
    assert_eq!(names, expected_names);

    for (tool, (name, params)) in tools.iter().zip(EXPECTED_TOOLS) {
        let schema = &tool.input_schema;
        assert_eq!(schema.get("type").and_then(|t| t.as_str()), Some("object"), "{name} schema");
        let properties = schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap_or_else(|| panic!("{name} publishes no parameter properties"));
        for param in params {
            assert!(properties.contains_key(*param), "{name} is missing parameter `{param}`");
        }
        assert_eq!(
            properties.len(),
            params.len(),
            "{name} publishes unexpected parameters: {properties:?}"
        );
        assert!(
            tool.description.as_ref().is_some_and(|d| !d.is_empty()),
            "{name} has no description for an agent to choose it by"
        );
    }

    // Every tool is a stub for now, so this is the "answers instead of
    // crashing" check: a tool-level error result, connection intact.
    let called = client
        .call_tool(
            CallToolRequestParams::new("find_references").with_arguments(
                json!({ "symbol_id": "does-not-exist" })
                    .as_object()
                    .cloned()
                    .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("tools/call must return a result, not a protocol failure");
    assert_eq!(called.is_error, Some(true));

    // Both processes have to survive the call: the session still answering
    // after it is the strongest form of that assertion.
    let pid = project.daemon_pid();
    assert!(is_alive(pid), "the daemon died during the session");
    let listed_again = client
        .list_tools(None)
        .await
        .expect("the session must survive a failed tool call");
    assert_eq!(listed_again.tools.len(), EXPECTED_TOOLS.len());

    client.cancel().await.expect("failed to shut the client down");

    // The daemon is detached by design: it outlives the shim that spawned it,
    // ready for the next client.
    wait_for("the daemon to stay up after the client leaves", || is_alive(pid));
}
