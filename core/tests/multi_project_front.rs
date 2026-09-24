//! GM-399 (D10-D12 in `docs/architecture/lazy-indexing.md`): a folder of
//! projects is served by the front, through the real shim (slice 4), and
//! `select_project` switches the session to the chosen project's own daemon
//! (slice 5, D11 step 3).
//!
//! The fixture root holds `a/` (a `.git` directory and `a.ts`), `b/` (`.git`
//! and `b.ts`) and `c/` (`go.mod`), with no marker of its own.

use std::path::{Path, PathBuf};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ProgressNotificationParam};
use rmcp::service::{NotificationContext, RoleClient, RunningService};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{ClientHandler, ServiceExt};
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
        let env: Vec<(&str, String)> = match front_idle_ms {
            Some(ms) => vec![(daemon::front::FRONT_IDLE_ENV, ms.to_string())],
            None => Vec::new(),
        };
        self.connect_in(self.root(), (), &env).await
    }

    /// A shim session started in `dir`, with `handler` as its client and
    /// `env` on top of the suite's defaults. Every daemon a shim bootstraps
    /// inherits its environment, so `env` reaches a sub-project's daemon too.
    async fn connect_in<H: ClientHandler>(
        &self,
        dir: &Path,
        handler: H,
        env: &[(&str, String)],
    ) -> RunningService<RoleClient, H> {
        let dir = dir.to_path_buf();
        let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
            cmd.kill_on_drop(true)
                .arg("mcp-shim")
                .current_dir(&dir)
                .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
                // No real model: keeps the embedding backfill pass a no-op.
                .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir");
            for (key, value) in env {
                cmd.env(key, value);
            }
        }))
        .expect("failed to spawn the shim");
        handler.serve(transport).await.expect("MCP initialization failed")
    }

    fn sub(&self, name: &str) -> PathBuf {
        self.root().join(name)
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        for dir in [self.root().to_path_buf(), self.sub("a"), self.sub("b"), self.sub("c")] {
            let Ok(state) = project_dir(&dir) else { continue };
            // `a_failed_switch_leaves_the_session_on_the_front` makes one
            // unreadable; give it back before looking inside.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755));
            }
            for path in [daemon::pid_path(&dir), daemon::plugin_pid_path(&dir)].into_iter().flatten() {
                common::kill_pid_file(&path);
            }
            for (_, plugin_pid) in daemon::registry::discovered_pid_files(&state) {
                common::kill_pid_file(&plugin_pid);
            }
            if let Ok(endpoint) = daemon::endpoint(&dir) {
                endpoint.clear_stale();
            }
            let _ = std::fs::remove_dir_all(state);
        }
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

    // The switch directive a selection carries never reaches the client
    // (slice 5); `select_project_carries_the_selected_projects_own_guidance`
    // checks it is gone, and `selecting_a_project_switches_the_session` that
    // the shim acted on it.

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

// ---- Slice 5 (GM-399 S3): the session switch in the shim ----

/// `P1`'s first sentence, which every normal daemon's instructions open with
/// and a front's too - so the tests below look for it *after* the switch
/// line, where only the replayed text can put it.
const P1_FIRST_SENTENCE: &str = "Structural code-graph queries over this project's index.";

fn canonical(path: &Path) -> String {
    path.canonicalize().expect("fixture paths exist").display().to_string()
}

async fn select<H: ClientHandler>(client: &RunningService<RoleClient, H>, project: &str) -> CallToolResult {
    let arguments = json!({ "project": project }).as_object().cloned().unwrap();
    client
        .call_tool(CallToolRequestParams::new("select_project").with_arguments(arguments))
        .await
        .unwrap_or_else(|err| panic!("select_project must return a result, not a protocol failure: {err}"))
}

/// The guidance a successful switch carries: everything after the shim's own
/// switch line.
fn guidance_of(result: &CallToolResult, project: &Path) -> String {
    assert_ne!(result.is_error, Some(true), "the switch must succeed: {}", text_of(result));
    let text = text_of(result);
    let line = format!("this session now serves {}", canonical(project));
    assert!(text.contains(&line), "`{line}` missing from:\n{text}");
    text.split_once("would receive it:\n\n").map(|(_, guidance)| guidance.to_string()).unwrap_or_default()
}

/// Names in a `get_file_outline` answer, or `None` for an error / non-JSON
/// answer.
fn outline_names(result: &CallToolResult) -> Option<Vec<String>> {
    if result.is_error == Some(true) {
        return None;
    }
    let outline: serde_json::Value = serde_json::from_str(&text_of(result)).ok()?;
    Some(
        outline["results"]
            .as_array()?
            .iter()
            .filter_map(|symbol| symbol["name"].as_str().map(str::to_string))
            .collect(),
    )
}

fn bulk_indexed(root: &Path) -> bool {
    let db = project_dir(root).unwrap().join("index.db");
    rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .ok()
        .and_then(|conn| g_mesh::storage::schema::bulk_index_completed(&conn).ok())
        .unwrap_or(false)
}

/// After `select_project {project:"b"}` the session is served by `b`'s own
/// daemon: paths are `b`-relative, `b` gets indexed, and neither the folder
/// nor `a` does.
///
/// Control: make the shim ignore the directive (`on_front_frame` returns the
/// frame untouched): the outline call reaches the front and gets its "none is
/// selected" error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selecting_a_project_switches_the_session() {
    let folder = Folder::new();
    let client = folder.connect(None).await;

    guidance_of(&select(&client, "b").await, &folder.sub("b"));
    let outline = call(&client, "get_file_outline", json!({ "file_path": "b.ts" })).await;
    assert_eq!(
        outline_names(&outline),
        Some(vec!["b".to_string()]),
        "b.ts's outline must come from b's daemon:\n{}",
        text_of(&outline)
    );

    common::wait_for("b's index to record a completed walk", common::startup_timeout(), || {
        bulk_indexed(&folder.sub("b"))
    });
    for dir in [folder.root().to_path_buf(), folder.sub("a")] {
        let db = project_dir(&dir).unwrap().join("index.db");
        assert!(!db.exists(), "{} must not have been indexed: {}", dir.display(), db.display());
    }

    client.cancel().await.expect("failed to shut the client down");
}

/// The instructions gap (D11 step 5, option A): the result carries `b`'s own
/// initialize instructions for its current (unindexed) phase; the client's
/// instructions stay the front's; the directive is removed.
///
/// Controls: (1) forward the front's result as is (skip the rewrite in
/// `on_front_frame`): no "this session now serves" and no cold-start line; (3)
/// keep the directive
/// (drop the `meta.remove(SWITCH_PROJECT_META)` line): `_meta` survives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn select_project_carries_the_selected_projects_own_guidance() {
    let folder = Folder::new();
    let client = folder.connect(None).await;
    let b = folder.sub("b");

    let selected = select(&client, "b").await;
    let guidance = guidance_of(&selected, &b);
    // `cold_start`'s state line, in either of its renderings: with the root
    // (`Index root: <b>. Not indexed yet`), or without it when the root would
    // push the text over the byte ceiling - which a long temp dir does.
    let with_root = format!("Index root: {}. Not indexed yet", canonical(&b));
    assert!(
        guidance.starts_with(&with_root) || guidance.starts_with("Not indexed yet"),
        "b's guidance must open with its cold-start line:\n{guidance}"
    );
    assert!(guidance.contains(P1_FIRST_SENTENCE), "P1 missing from b's guidance:\n{guidance}");

    // The known limitation, pinned: MCP reads instructions once, so the
    // client keeps the front's text.
    let instructions = client.peer_info().and_then(|info| info.instructions.clone()).unwrap_or_default();
    assert!(instructions.contains("is a folder of 3 projects"), "{instructions}");

    let directive = selected.meta.as_ref().and_then(|meta| meta.0.get("g-mesh/switchProject"));
    assert!(directive.is_none(), "the switch directive must not reach the client: {:?}", selected.meta);

    client.cancel().await.expect("failed to shut the client down");
}

/// The guidance is `b`'s *live* text, not a plausible one about `b`: with
/// `b` already indexed by another session, the text a session started in
/// `b` would get has no "Not indexed yet".
///
/// Control: have the shim render `instructions::cold_start(b, false, ..)`
/// itself instead of using the replayed `instructions`: "Not indexed yet"
/// appears although `b` is indexed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guidance_reflects_the_projects_current_state() {
    let folder = Folder::new();
    let b = folder.sub("b");
    let in_b = folder.connect_in(&b, (), &[]).await;
    common::wait_until_indexed(&b);

    let client = folder.connect(None).await;
    let guidance = guidance_of(&select(&client, "b").await, &b);
    assert!(guidance.contains(P1_FIRST_SENTENCE), "P1 missing from b's guidance:\n{guidance}");
    assert!(!guidance.contains("Not indexed yet"), "b is indexed, yet its guidance says:\n{guidance}");

    client.cancel().await.expect("failed to shut the client down");
    in_b.cancel().await.expect("failed to shut the b session down");
}

/// Reselecting the current project is a full switch (Q9), so it re-renders
/// the guidance for the project's state now.
///
/// Control: a same-project shortcut that re-sends the first switch's cached
/// guidance: the second text still says "Not indexed yet".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselecting_the_same_project_refreshes_its_guidance() {
    let folder = Folder::new();
    let client = folder.connect(None).await;
    let b = folder.sub("b");

    let first = guidance_of(&select(&client, "b").await, &b);
    assert!(first.contains("Not indexed yet"), "{first}");
    let outline = call(&client, "get_file_outline", json!({ "file_path": "b.ts" })).await;
    assert_eq!(outline_names(&outline), Some(vec!["b".to_string()]), "{}", text_of(&outline));
    common::wait_for("b's index to record a completed walk", common::startup_timeout(), || bulk_indexed(&b));

    let second = guidance_of(&select(&client, "b").await, &b);
    assert!(second.contains(P1_FIRST_SENTENCE), "{second}");
    assert!(!second.contains("Not indexed yet"), "reselection must re-render b's guidance:\n{second}");

    client.cancel().await.expect("failed to shut the client down");
}

/// Selecting another project moves the session again.
///
/// Control: keep routing to the first sub-project upstream (don't replace
/// `router.sub` once set): `a.ts` is not found and `b.ts` still answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselecting_switches_again() {
    let folder = Folder::new();
    let client = folder.connect(None).await;

    guidance_of(&select(&client, "b").await, &folder.sub("b"));
    guidance_of(&select(&client, "a").await, &folder.sub("a"));

    let a = call(&client, "get_file_outline", json!({ "file_path": "a.ts" })).await;
    assert_eq!(outline_names(&a), Some(vec!["a".to_string()]), "{}", text_of(&a));
    let b = call(&client, "get_file_outline", json!({ "file_path": "b.ts" })).await;
    let names = outline_names(&b);
    assert!(
        names.as_ref().is_none_or(|names| names.is_empty()),
        "b.ts must be unknown to a's daemon, got {names:?}:\n{}",
        text_of(&b)
    );
    assert!(!text_of(&b).contains("none is selected"), "the call must reach a's daemon:\n{}", text_of(&b));

    client.cancel().await.expect("failed to shut the client down");
}

/// A switch whose bootstrap fails is an error result naming the failure,
/// and the session stays on the front.
///
/// Control: set `router.sub` before the connector's result is checked (e.g.
/// install a dead upstream on failure): the next call errors at transport
/// level or reaches no daemon instead of getting the front's answer.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_switch_leaves_the_session_on_the_front() {
    use std::os::unix::fs::PermissionsExt;

    let folder = Folder::new();
    let client = folder.connect(None).await;
    let b = folder.sub("b");
    let state = project_dir(&b).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o000)).unwrap();

    let selected = select(&client, "b").await;
    assert_eq!(selected.is_error, Some(true), "{}", text_of(&selected));
    let text = text_of(&selected);
    assert!(text.contains(&format!("could not switch this session to {}", canonical(&b))), "{text}");

    let refs = call(&client, "find_references", json!({ "symbol_name": "b" })).await;
    assert_eq!(refs.is_error, Some(true));
    assert!(text_of(&refs).contains("none is selected"), "{}", text_of(&refs));

    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    client.cancel().await.expect("failed to shut the client down");
}

/// A client that records every progress notification it receives.
#[derive(Clone, Default)]
struct ProgressRecorder(std::sync::Arc<std::sync::Mutex<Vec<ProgressNotificationParam>>>);

impl ClientHandler for ProgressRecorder {
    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.0.lock().unwrap().push(params);
    }
}

/// Notifications from the selected project's daemon reach the client, not
/// only responses: a first call on unindexed `b`, held on its walk, gets a
/// progress heartbeat.
///
/// Control: make the sub-project reader forward only response frames (drop
/// frames with a `method`): no progress arrives and the wait times out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_passes_through_after_a_switch() {
    let folder = Folder::new();
    let hold = folder.root().join("walk.hold");
    std::fs::write(&hold, b"").unwrap();
    let recorder = ProgressRecorder::default();
    let env = [
        (g_mesh::mcp::PROGRESS_INTERVAL_ENV, "200".to_string()),
        (g_mesh::daemon::bulk_index::WALK_HOLD_FILE_ENV, hold.display().to_string()),
    ];
    let client = folder.connect_in(folder.root(), recorder.clone(), &env).await;

    guidance_of(&select(&client, "b").await, &folder.sub("b"));
    let call = tokio::spawn({
        let peer = client.peer().clone();
        let arguments = json!({ "file_path": "b.ts" }).as_object().cloned().unwrap();
        async move {
            peer.call_tool(CallToolRequestParams::new("get_file_outline").with_arguments(arguments)).await
        }
    });
    common::wait_for("a progress notification from b's daemon", common::startup_timeout(), || {
        !recorder.0.lock().unwrap().is_empty()
    });
    std::fs::remove_file(&hold).unwrap();
    let outline = call.await.unwrap().expect("tools/call must return a result");
    assert_eq!(outline_names(&outline), Some(vec!["b".to_string()]), "{}", text_of(&outline));

    client.cancel().await.expect("failed to shut the client down");
}
