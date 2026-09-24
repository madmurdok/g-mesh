//! GM-399 slice 4 (D10-D12 in `docs/architecture/lazy-indexing.md`): a
//! folder of projects is served by the front, through the real shim.
//!
//! The fixture root holds `a/` (a `.git` directory and `a.ts`), `b/` (`.git`
//! and `b.ts`) and `c/` (`go.mod`), with no marker of its own. Until slice 5
//! lands the shim forwards the `select_project` result's `_meta` untouched,
//! which is what this file checks; the session switch is slice 5's.

use std::path::{Path, PathBuf};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use serde_json::json;
use tokio::process::Command;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// The eight index-backed tools every daemon lists.
const INDEX_TOOLS: [&str; 8] = [
    "find_callees",
    "find_callers",
    "find_definition",
    "find_implementations",
    "find_references",
    "get_dependencies",
    "get_file_outline",
    "search_code",
];

struct Folder {
    dir: tempfile::TempDir,
}

impl Folder {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a temp folder");
        let root = dir.path();
        for (path, contents) in [
            ("a/a.ts", "export const a = 1;\n"),
            ("b/b.ts", "export const b = 2;\n"),
            ("c/go.mod", "module example.com/c\n\ngo 1.22\n"),
        ] {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, contents).unwrap();
        }
        std::fs::create_dir_all(root.join("a/.git")).unwrap();
        std::fs::create_dir_all(root.join("b/.git")).unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn state_dir(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory")
    }

    fn pid_file(&self) -> PathBuf {
        daemon::pid_path(self.root()).expect("failed to resolve the pid file path")
    }

    async fn connect(&self, front_idle_ms: Option<u64>) -> RunningService<RoleClient, ()> {
        let root = self.root().to_path_buf();
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&root)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
            if let Some(ms) = front_idle_ms {
                cmd.env(daemon::front::FRONT_IDLE_ENV, ms.to_string());
            }
        }))
        .expect("failed to spawn the shim");
        ().serve(transport).await.expect("MCP initialization failed")
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        common::kill_pid_file(&self.pid_file());
        let _ = std::fs::remove_dir_all(self.state_dir());
    }
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    args: serde_json::Value,
) -> CallToolResult {
    let arguments = args.as_object().cloned().expect("arguments must be an object");
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
        .unwrap_or_else(|err| panic!("{name} must return a result, not a protocol failure: {err}"))
}

fn text_of(result: &CallToolResult) -> String {
    result.content.iter().filter_map(|block| block.as_text()).map(|text| text.text.as_str()).collect()
}

#[tokio::test]
async fn a_folder_of_projects_is_served_by_the_front() {
    let folder = Folder::new();
    let client = folder.connect(None).await;
    let canonical = folder.root().canonicalize().unwrap();

    // Instructions name the projects and the way out.
    let info = client.peer_info().expect("server never reported its info");
    assert_eq!(info.server_info.name, "g-mesh");
    let instructions = info.instructions.clone().expect("a front must send instructions");
    assert!(instructions.contains("select_project"), "{instructions}");
    assert!(instructions.contains("is a folder of 3 projects"), "{instructions}");
    assert!(instructions.ends_with("Projects: a, b, c."), "{instructions}");

    // The usual eight, plus select_project.
    let listed = client.list_tools(None).await.expect("tools/list failed");
    let mut names: Vec<String> = listed.tools.iter().map(|tool| tool.name.to_string()).collect();
    names.sort();
    let mut expected: Vec<String> = INDEX_TOOLS.iter().map(|name| name.to_string()).collect();
    expected.push("select_project".to_string());
    expected.sort();
    assert_eq!(names, expected);

    // Listing, and an unknown project.
    let listing = call(&client, "select_project", json!({})).await;
    assert_ne!(listing.is_error, Some(true));
    let text = text_of(&listing);
    for line in ["- a [.git]", "- b [.git]", "- c [go.mod]"] {
        assert!(text.contains(line), "`{line}` missing from the listing:\n{text}");
    }
    let unknown = call(&client, "select_project", json!({ "project": "nope" })).await;
    assert_eq!(unknown.is_error, Some(true));
    let text = text_of(&unknown);
    for line in ["- a [.git]", "- b [.git]", "- c [go.mod]"] {
        assert!(text.contains(line), "an unknown project must list the real ones:\n{text}");
    }

    // A selection carries the switch directive for the shim (slice 5); until
    // then the shim forwards it untouched.
    let selected = call(&client, "select_project", json!({ "project": "b" })).await;
    assert_ne!(selected.is_error, Some(true), "{}", text_of(&selected));
    let meta = selected.meta.as_ref().expect("a selection must carry _meta");
    let root =
        meta.0.get("g-mesh/switchProject").and_then(|switch| switch.get("root")).and_then(|r| r.as_str());
    assert_eq!(root, Some(canonical.join("b").to_str().unwrap()));

    // Every index-backed tool answers at once, naming the choices.
    let refs = call(&client, "find_references", json!({ "symbol_name": "a" })).await;
    assert_eq!(refs.is_error, Some(true));
    let text = text_of(&refs);
    assert!(text.contains("none is selected"), "{text}");
    assert!(text.contains("one of: a, b, c"), "{text}");

    let outline = call(&client, "get_file_outline", json!({ "file_path": "b/b.ts" })).await;
    assert_eq!(outline.is_error, Some(true));
    let text = text_of(&outline);
    assert!(text.contains("lies in 'b'"), "the file_path hint must name b:\n{text}");

    // No index for the folder, no plugin, and the phase says front.
    common::wait_until_phase(folder.root(), "front");
    let state = folder.state_dir();
    assert!(!state.join("index.db").exists(), "a front must never create the folder's index.db");
    let plugin_pid_files = daemon::registry::discovered_pid_files(&state);
    assert!(plugin_pid_files.is_empty(), "a front spawns no plugin: {plugin_pid_files:?}");

    client.cancel().await.expect("failed to shut the client down");
}

#[tokio::test]
async fn an_idle_front_exits_after_its_own_short_timeout() {
    let folder = Folder::new();
    let client = folder.connect(Some(500)).await;
    let pid_file = folder.pid_file();
    assert!(pid_file.exists(), "the front must publish its pid file before answering initialize");
    assert_eq!(daemon::read_phase_in(&folder.state_dir()).as_deref(), Some("front"));

    client.cancel().await.expect("failed to shut the client down");

    common::wait_for("the idle front to exit", common::startup_timeout(), || !pid_file.exists());
    assert!(daemon::read_phase_in(&folder.state_dir()).is_none(), "the phase file goes with the front");
}
