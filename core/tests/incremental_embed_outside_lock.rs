//! Acceptance test for GM-396: computing embeddings for an *incremental*
//! reparse must not hold the daemon's one SQLite mutex - the same defect
//! GM-394 fixed for a cold-start bulk-index batch, but for
//! `watcher::apply::round_trip` (a watcher event, `ensure_fresh`, or a
//! replayed queued change) instead of `daemon::bulk_index::commit`.
//!
//! [`g_mesh::watcher::apply::HOLD_COMPUTE_FILE_ENV`] is `round_trip`'s own
//! test-only knob (see its doc comment): it parks a round trip open, with the
//! connection lock already released, at exactly the point a real
//! `EmbeddingPipeline::compute`'s inference would run. This file uses it to
//! prove a concurrent tool call against a *different*, already-fresh file is
//! never blocked on that window.
//!
//! # Why `g-mesh init` runs first, in its own process
//!
//! The daemon also runs a whole-project semantic pass right after its own
//! cold-start bulk walk, inline and before its watcher is registered
//! (`daemon::mod::run`'s own comment on why that pass cannot be
//! backgrounded) - and that pass goes through the very same `round_trip`
//! function this file's knob catches. Setting the hold from the moment the
//! *test* daemon starts would therefore catch that pass first, wedge it
//! (with the watcher never registered as a result), and never reach the
//! specific incremental reparse this file means to test.
//!
//! `g-mesh init` builds the whole index - the bulk walk *and* its semantic
//! pass - as a one-shot command with no daemon and no hold var involved at
//! all. The daemon this file then spawns (with the hold var set) sees both
//! already recorded (`schema::bulk_index_completed`/`semantic_pass_completed`),
//! skips straight to registering its watcher, and the one edit this file
//! makes afterward is the *first* round trip that daemon process ever sends -
//! exactly the one `HOLD_COMPUTE_FILE_ENV` is meant to catch.
//!
//! # The control
//!
//! [`a_tool_call_against_an_unrelated_file_is_not_blocked_by_a_reparses_lock_free_embedding_window`]
//! is meaningless on its own without having been shown to fail against the
//! pre-fix code. That control was run separately (a worktree of
//! `release-3.11.1` with only this file and a copy of `watcher::apply::
//! round_trip`'s hold hook added at the equivalent point - the code before
//! GM-396 holds the connection lock across the whole round trip, so the hook
//! fires from inside it instead of after it releases): it failed the
//! `CONCURRENT_CALL_BUDGET` assertion, the concurrent call having taken as
//! long as the hold was open for - see the task's completion notes for the
//! command and its output.

use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::ipc;
use g_mesh::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};
use g_mesh::storage::connection::project_dir;
use g_mesh::watcher::apply::HOLD_COMPUTE_FILE_ENV;
use rusqlite::Connection;
use serde_json::{json, Value};

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Generous next to what a socket connect plus one `get_file_outline` call
/// over an already-open, already-fresh file costs - nowhere near the 30s
/// safety valve on the hold itself, which is exactly the gap this file exists
/// to prove is zero rather than merely "less than 30s".
const CONCURRENT_CALL_BUDGET: Duration = Duration::from_secs(3);

struct Project {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Project {
    fn new() -> Self {
        Self { dir: tempfile::tempdir().expect("failed to create a temp project root"), daemon: None }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, relative_path: &str, contents: &str) {
        let path = self.root().join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create a fixture directory");
        }
        std::fs::write(&path, contents).expect("failed to write a fixture file");
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        if let Ok(state) = project_dir(self.root()) {
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}

/// Runs `g-mesh init` to completion against `root` - a one-shot bulk walk
/// plus its semantic pass, with no daemon and no watcher involved at all. See
/// this module's own doc comment for why this has to happen before the
/// daemon this file goes on to spawn.
fn init_project(root: &Path) {
    let status = Command::new(BIN)
        .arg("init")
        .current_dir(root)
        // Deterministic and fast: this file is about lock contention, not
        // embeddings, and a real model directory (if one happens to exist on
        // the machine running this) would make `init`'s own walk pay for a
        // load this test does not need.
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to run g-mesh init");
    assert!(status.success(), "g-mesh init must succeed against a freshly seeded project");
}

/// Spawns the project's daemon directly (the same `g-mesh daemon
/// --project-root` invocation `shim::spawn_detached_daemon` uses) with
/// [`HOLD_COMPUTE_FILE_ENV`] pointed at `hold_file`.
///
/// This alone does **not** engage the hold: `round_trip`'s own
/// `hold_compute_open_for_tests` reads the env var fresh on every call but
/// only actually waits while the *file* it names exists (see that function's
/// doc comment) - and a child process's environment is fixed at spawn, so
/// this is the only chance to point it anywhere at all. `hold_file` is
/// therefore created empty here as a name only; the caller decides when to
/// `std::fs::write` it into existence and start the hold for real.
fn spawn_daemon_pointing_at_hold_file(project: &mut Project, hold_file: &Path) {
    let daemon = Command::new(BIN)
        .arg("daemon")
        .arg("--project-root")
        .arg(project.root())
        .env(HOLD_COMPUTE_FILE_ENV, hold_file)
        .env(g_mesh::embedding::model::MODEL_DIR_ENV, "/nonexistent-g-mesh-test-model-dir")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn the daemon directly");
    project.daemon = Some(daemon);
}

fn wait_for(what: &str, ready: impl FnMut() -> bool) {
    common::wait_for(what, common::startup_timeout(), ready);
}

/// The number of `nodes` rows named `name` - read through a *second*,
/// independent connection straight to the on-disk database, the same way
/// `handshake_independent_of_indexing.rs`'s `wait_until_the_graph_holds`
/// confirms a bulk-index batch's own hold is genuinely engaged. `apply_diff`/
/// the two linking passes commit (and release `conn`'s lock) strictly before
/// `round_trip` reaches its hold point, so a nonzero count here is proof the
/// reparse is now sitting in that lock-free window, not merely that it
/// started.
fn node_count(root: &Path, name: &str) -> i64 {
    let db = project_dir(root).expect("failed to resolve the state directory").join("index.db");
    Connection::open(&db)
        .ok()
        .and_then(|conn| {
            conn.query_row("SELECT COUNT(*) FROM nodes WHERE name = ?1", [name], |row| row.get::<_, i64>(0))
                .ok()
        })
        .unwrap_or(0)
}

/// A fresh connection's `initialize`/`notifications/initialized` handshake
/// followed by one `tools/call` for `get_file_outline` - hand-rolled
/// newline-delimited JSON-RPC exactly like `daemon_core.rs`, no MCP client
/// dependency needed for one request/response pair.
///
/// The clock starts before the connection even exists, deliberately -
/// `mcp::mod::GMeshMcpServer` briefly takes `conn`'s own lock on `initialize`
/// too (`last_used::touch`, for GC bookkeeping), so timing only the
/// `tools/call` half would miss exactly the delay this file's assertion is
/// about if a round trip elsewhere is holding that lock: it would resolve
/// during the handshake instead, before this function's own timer had even
/// started, and the measured `tools/call` alone would come back fast for a
/// reason with nothing to do with GM-396.
fn outline_elapsed(endpoint: &ipc::Endpoint, file_path: &str) -> (Duration, Value) {
    let started = Instant::now();
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
                "clientInfo": { "name": "g-mesh-incremental-embed-tests", "version": "0" },
            },
        }),
    );
    let initialized = receive(&mut reader);
    assert_eq!(
        initialized["result"]["serverInfo"]["name"], "g-mesh",
        "unexpected initialize response: {initialized}"
    );
    send(&mut writer, &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    send(
        &mut writer,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_file_outline", "arguments": { "file_path": file_path } },
        }),
    );
    let response = receive(&mut reader);
    (started.elapsed(), response)
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

fn outline_symbol_names(response: &Value) -> Vec<String> {
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool call returned no text content: {response}"));
    let payload: Value =
        serde_json::from_str(text).unwrap_or_else(|e| panic!("tool payload is not JSON ({e}): {text}"));
    payload["results"]
        .as_array()
        .unwrap_or_else(|| panic!("outline has no results array: {payload}"))
        .iter()
        .map(|symbol| symbol["name"].as_str().expect("a symbol name must be a string").to_string())
        .collect()
}

/// GM-396's acceptance criterion: while an incremental reparse's embedding
/// step is parked with `conn`'s lock released (`HOLD_COMPUTE_FILE_ENV`), a
/// tool call against a *different*, already-fresh file is answered fast, not
/// queued behind that window.
///
/// `a.ts` is the file this test edits after the daemon's watcher is
/// registered - the one whose reparse gets held. `b.ts` is never touched
/// after the warm-up query below records its baseline, so its own
/// `ensure_fresh` fast path needs only `conn`'s lock, briefly
/// (`watcher::staleness`'s own module doc) - never `daemon::lifecycle::
/// PluginSupervisor::inner`/`daemon::plugin::PluginProcess::state`, the
/// *other* lock `a.ts`'s stuck round trip is holding for its whole duration
/// (`daemon::lifecycle`'s "Lock order" section) and which this test is
/// deliberately not about: only `conn`'s lock is what GM-396 changes.
///
/// # Why `b.ts` needs a warm-up query first
///
/// `indexed_files` - the table `watcher::staleness::ensure_fresh`'s two-tier
/// check reads - is written only by that check itself, never by a bulk walk
/// (`g-mesh init`'s own walk, or a daemon's cold start, populate `nodes`/
/// `edges`, not this table). So immediately after `init_project`, *every*
/// file looks "never indexed" to `is_stale`, `b.ts` included - querying it
/// cold would need a real round trip of its own (`Decision::NeedsReindex`),
/// which would then compete for the very same
/// `PluginSupervisor::inner`/`PluginProcess::state` lock `a.ts`'s reparse is
/// holding, and defeat the point of using `b.ts` as an unrelated control at
/// all. One query for `b.ts` before the hold is armed records its baseline
/// the ordinary way an `ensure_fresh` slow path always does, so the query
/// this test actually measures hits `is_stale`'s fast path as intended.
#[test]
fn a_tool_call_against_an_unrelated_file_is_not_blocked_by_a_reparses_lock_free_embedding_window() {
    let mut project = Project::new();
    project.write("src/a.ts", "export function alpha(): number {\n  return 1;\n}\n");
    project.write("src/b.ts", "export function beta(): number {\n  return 2;\n}\n");

    init_project(project.root());

    // Named now, created empty later - see `spawn_daemon_pointing_at_hold_file`'s
    // own doc comment for why the hold is not armed yet just because the
    // daemon knows this path.
    let hold_file = project.root().join(".g-mesh-hold-compute");
    spawn_daemon_pointing_at_hold_file(&mut project, &hold_file);

    let pid_file = daemon::pid_path(project.root()).expect("failed to resolve the pid file path");
    wait_for("the daemon to start listening", || pid_file.exists());

    // The warm-up query - see this test's own doc comment ("Why `b.ts` needs
    // a warm-up query first"). The hold file does not exist yet, so
    // `round_trip`'s hold hook is a no-op for whatever round trip this needs.
    let endpoint = daemon::endpoint(project.root()).expect("failed to resolve the daemon's endpoint");
    let (_warm_up_elapsed, warm_up_response) = outline_elapsed(&endpoint, "src/b.ts");
    assert_eq!(
        outline_symbol_names(&warm_up_response),
        vec!["beta".to_string()],
        "the warm-up query must itself succeed: {warm_up_response}"
    );

    // Now arm the hold, and edit `a.ts` - `init_project` already fully
    // indexed the project (walk + semantic pass), so this is the first
    // watcher-triggered round trip this daemon process ever sends, and the
    // one the hold file catches.
    std::fs::write(&hold_file, b"").expect("failed to plant the hold file");
    project.write(
        "src/a.ts",
        "export function alpha(): number {\n  return 42;\n}\n\nexport function newly_added(): number {\n  return 7;\n}\n",
    );

    // Proof the reparse's `apply_diff`/link_diff already committed (and
    // therefore released `conn`'s lock) and it is now sitting in its
    // lock-free hold - see `node_count`'s own doc comment.
    wait_for("the reparse's diff to commit", || node_count(project.root(), "newly_added") > 0);

    let (elapsed, response) = outline_elapsed(&endpoint, "src/b.ts");
    assert!(
        elapsed < CONCURRENT_CALL_BUDGET,
        "a tool call against an unrelated, already-fresh file must not wait on a reparse's lock-free \
         embedding window: took {elapsed:?}"
    );
    assert_eq!(
        outline_symbol_names(&response),
        vec!["beta".to_string()],
        "the unrelated file's own outline must still be answered correctly: {response}"
    );

    std::fs::remove_file(&hold_file).expect("failed to release the hold");
}
