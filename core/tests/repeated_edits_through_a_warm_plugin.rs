//! GM-293: the second edit of a file in one plugin process's lifetime has to
//! reach the index.
//!
//! On 2.12.0 it did not (GM-292), and nothing in the suite noticed.
//! `rusqlite`'s `bundled` feature compiles SQLite with
//! `SQLITE_DEFAULT_FOREIGN_KEYS=1`, so the
//! daemon's connection enforced the `edges -> nodes` foreign keys that every
//! comment in core described as off. The plugin reports any change to a
//! symbol - a longer body, a moved range, a new signature - as a delete plus
//! an upsert of the same id, while the edges into it that did not change
//! (the file's `DEFINES`, a caller's `CALLS` from another file) are not
//! re-sent. The delete tripped the foreign key, the whole diff rolled back,
//! and `PluginProcess::apply_file_change` then "replayed" the file against a
//! plugin whose cache already held the new text: an empty diff, reported as
//! success. The answer stayed stale, without an error, and the query-time
//! staleness check then stamped that stale graph fresh.
//!
//! The first edit after a plugin starts always worked - the plugin has no
//! cached state, so it sends the whole file as upserts and deletes nothing -
//! and no earlier test edited an existing declaration a second time through
//! the same plugin (the retry loops elsewhere only rewrite a trailing
//! comment, which changes no symbol). Every test here therefore makes an edit
//! *through a warm plugin cache*, and makes it twice.
//!
//! Two layers, deliberately:
//!
//!  - [`PluginProcess`] driven directly over the production connection
//!    (`storage::connection::open`, not an in-memory database with pragmas of
//!    the test's choosing) - deterministic about exactly which request
//!    carries which edit, which is what the cross-file delete/rename case
//!    needs.
//!  - The whole stack - shim, daemon, watcher, plugin, MCP - answering
//!    `get_file_outline`, because "the MCP answer is stale" is the symptom a
//!    user actually saw, and the layer where the failure used to be
//!    swallowed.
//!
//! Requires `plugins/typescript/dist/` to be up to date; `core/build.rs` runs
//! `npm run build` there whenever this crate is built.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::daemon::plugin::{bundled_manifest, PluginProcess};
use g_mesh::embedding::EmbeddingPipeline;
use g_mesh::storage::connection::{self, project_dir};
use g_mesh::storage::schema;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;
use common::wait_until_indexed;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// A temp project root whose state directory (and anything still running
/// against it) is torn down with it.
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

    fn write(&self, relative: &str, contents: &str) {
        let path = self.root().join(relative);
        fs::create_dir_all(path.parent().unwrap()).expect("failed to create a fixture directory");
        fs::write(&path, contents).expect("failed to write a fixture file");
    }

    fn index_path(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory").join("index.db")
    }

    /// Kills the daemon and its plugin, if this project has them - only ever
    /// the processes this test's own state directory names.
    fn stop(&self) {
        for path in [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())] {
            let Ok(path) = path else { continue };
            common::kill_pid_file(&path);
        }
        if let Ok(endpoint) = daemon::endpoint(self.root()) {
            endpoint.clear_stale();
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        self.stop();
        if let Ok(state) = project_dir(self.root()) {
            let _ = fs::remove_dir_all(&state);
        }
    }
}

/// `(startLine, endLine)` of every node named `name` in `file_path`.
fn ranges_of(conn: &Connection, file_path: &str, name: &str) -> Vec<(i64, i64)> {
    let mut stmt = conn
        .prepare("SELECT startLine, endLine FROM nodes WHERE filePath = ?1 AND name = ?2 ORDER BY startLine")
        .unwrap();
    stmt.query_map([file_path, name], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

// --- PluginProcess over the production connection --------------------------

const LIB: &str = "lib.ts";

const GREET: &str = "export function greet(): string {\n  return \"hello\";\n}\n";

/// [`GREET`] with two more lines in its body: `greet`'s own range changes, so
/// the plugin reports it as a delete plus an upsert - while the file's
/// `DEFINES` edge into it, whose id has no position in it, is not re-sent.
const GREET_GROWN: &str =
    "export function greet(): string {\n  const a = 1;\n  const b = 2;\n  return \"hello\";\n}\n";

/// [`GREET_GROWN`] with three lines above it and a new function after it:
/// every range in the file moves, and something new has to appear.
const GREET_SHIFTED_WITH_FAREWELL: &str = "// one\n// two\n// three\n\
     export function greet(): string {\n  const a = 1;\n  const b = 2;\n  return \"hello\";\n}\n\
     \nexport function farewell(): string {\n  return \"bye\";\n}\n";

fn open_production_index(project: &Project) -> Mutex<Connection> {
    let conn = connection::open(project.root()).expect("failed to open the project's index");
    schema::apply(&conn).expect("failed to apply the schema");
    Mutex::new(conn)
}

fn apply(plugin: &PluginProcess, conn: &Mutex<Connection>, file_path: &str) {
    plugin
        .apply_file_change(conn, file_path, &EmbeddingPipeline::disabled())
        .unwrap_or_else(|err| panic!("applying a change to {file_path} failed: {err:#}"));
}

#[test]
fn a_declaration_edited_twice_through_a_warm_plugin_cache_is_updated_both_times() {
    let project = Project::new();
    project.write(LIB, GREET);
    let conn = open_production_index(&project);
    let plugin = PluginProcess::spawn(project.root(), &bundled_manifest(), project.root().join("plugin.pid"))
        .expect("failed to spawn the JS/TS plugin");

    // A cold cache: the whole file arrives as upserts. This always worked.
    apply(&plugin, &conn, LIB);
    let [(start, end)] = ranges_of(&conn.lock().unwrap(), LIB, "greet")[..] else {
        panic!("the first extraction must index exactly one `greet`");
    };

    // Edit 1, through the now-warm cache.
    project.write(LIB, GREET_GROWN);
    apply(&plugin, &conn, LIB);
    assert_eq!(
        ranges_of(&conn.lock().unwrap(), LIB, "greet"),
        vec![(start, end + 2)],
        "a body that grew by two lines must move `greet`'s end - the first edit through a warm \
         plugin is a delete plus an upsert of the same id"
    );

    // Edit 2, through the same warm cache.
    project.write(LIB, GREET_SHIFTED_WITH_FAREWELL);
    apply(&plugin, &conn, LIB);
    let conn = conn.lock().unwrap();
    assert_eq!(
        ranges_of(&conn, LIB, "greet"),
        vec![(start + 3, end + 5)],
        "three lines added above must shift `greet` by three"
    );
    let farewell = ranges_of(&conn, LIB, "farewell");
    assert_eq!(farewell.len(), 1, "the appended function must be indexed: {farewell:?}");
    assert!(farewell[0].0 > end + 5, "`farewell` is written after `greet`: {farewell:?}");
}

const HELPERS: &str = "helpers.ts";
const HELPERS_ORIGINAL: &str = "export function helper(): number {\n  return 1;\n}\n\n\
     export function doomed(): number {\n  return 2;\n}\n";
const HELPERS_RENAMED: &str = "export function renamedHelper(): number {\n  return 1;\n}\n\n\
     export function doomed(): number {\n  return 2;\n}\n";
const HELPERS_WITHOUT_DOOMED: &str = "export function renamedHelper(): number {\n  return 1;\n}\n";

const CALLER: &str = "caller.ts";
const CALLER_SOURCE: &str = "import { helper, doomed } from \"./helpers\";\n\n\
     export function run(): number {\n  return helper() + doomed();\n}\n";

/// How many edges from `caller.ts`'s symbols land on a node named `name`.
fn cross_file_edges_into(conn: &Connection, name: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM edges e
           JOIN nodes source ON source.id = e.fromId
           JOIN nodes target ON target.id = e.toId
          WHERE source.filePath = ?1 AND target.filePath = ?2 AND target.name = ?3",
        [CALLER, HELPERS, name],
        |row| row.get(0),
    )
    .unwrap()
}

/// The foreign key's other victim: a symbol that another file points at.
/// Deleting or renaming it deletes a node a surviving cross-file edge still
/// references - which with enforcement on rolled the whole diff back.
///
/// With it off, the edit applies and the caller's edge is left dangling until
/// `caller.ts` is itself reindexed, which is the lazy cross-file semantics
/// `graph::imports::link_diff` documents. That edge is not asserted on here:
/// it is a consequence this test tolerates, not a guarantee it pins.
#[test]
fn renaming_then_deleting_a_symbol_another_file_calls_is_applied() {
    let project = Project::new();
    project.write(HELPERS, HELPERS_ORIGINAL);
    project.write(CALLER, CALLER_SOURCE);
    let conn = open_production_index(&project);
    let plugin = PluginProcess::spawn(project.root(), &bundled_manifest(), project.root().join("plugin.pid"))
        .expect("failed to spawn the JS/TS plugin");

    apply(&plugin, &conn, HELPERS);
    apply(&plugin, &conn, CALLER);
    {
        let conn = conn.lock().unwrap();
        assert!(
            cross_file_edges_into(&conn, "helper") > 0 && cross_file_edges_into(&conn, "doomed") > 0,
            "the precondition that makes this a cross-file case: caller.ts's calls must be linked \
             onto helpers.ts's nodes"
        );
    }

    project.write(HELPERS, HELPERS_RENAMED);
    apply(&plugin, &conn, HELPERS);
    {
        let conn = conn.lock().unwrap();
        assert_eq!(ranges_of(&conn, HELPERS, "helper"), vec![], "the old name must be gone");
        assert_eq!(ranges_of(&conn, HELPERS, "renamedHelper").len(), 1, "the new name must be indexed");
    }

    project.write(HELPERS, HELPERS_WITHOUT_DOOMED);
    apply(&plugin, &conn, HELPERS);
    let conn = conn.lock().unwrap();
    assert_eq!(ranges_of(&conn, HELPERS, "doomed"), vec![], "a deleted symbol must leave the index");
    assert_eq!(ranges_of(&conn, HELPERS, "renamedHelper").len(), 1);
}

// --- The whole stack, answered over MCP ------------------------------------

const OUTLINED: &str = "src/lib.ts";

/// How long an edit gets to show up in an MCP answer. A deadlock guard rather
/// than a timing claim: on a healthy daemon each poll's own staleness check
/// reindexes the file synchronously, so the first poll after a write already
/// answers from it.
const EDIT_DEADLINE: Duration = Duration::from_secs(60);

fn body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a successful call: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => serde_json::from_str(&text.text).expect("tool result is not JSON"),
        other => panic!("expected text content, got {other:?}"),
    }
}

/// `get_file_outline`'s answer for [`OUTLINED`], as `(name, startLine,
/// endLine)`.
async fn outline(client: &rmcp::service::RunningService<rmcp::RoleClient, ()>) -> Vec<(String, i64, i64)> {
    let result = client
        .call_tool(CallToolRequestParams::new("get_file_outline").with_arguments(
            json!({ "file_path": OUTLINED }).as_object().cloned().expect("arguments literal is an object"),
        ))
        .await
        .expect("tools/call failed");
    body(&result)["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .map(|symbol| {
            (
                symbol["name"].as_str().unwrap().to_string(),
                symbol["startLine"].as_i64().unwrap(),
                symbol["endLine"].as_i64().unwrap(),
            )
        })
        .collect()
}

/// Polls [`outline`] until it equals `expected`, failing with the last answer
/// seen - which, for the bug this file is about, is the pre-edit outline.
async fn await_outline(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    what: &str,
    expected: &[(String, i64, i64)],
) {
    let deadline = Instant::now() + EDIT_DEADLINE;
    loop {
        let seen = outline(client).await;
        if seen == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: get_file_outline still answers {seen:?}, expected {expected:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn get_file_outline_answers_from_a_declaration_edited_twice_under_a_running_daemon() {
    let project = Project::new();
    project.write(OUTLINED, GREET);

    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    wait_until_indexed(project.root());

    // The first query of a file the interactive plugin has never seen is
    // itself a reparse (no `indexed_files` baseline yet), which is what warms
    // the plugin's cache for the two edits below.
    let initial = outline(&client).await;
    let [(ref name, start, end)] = initial[..] else {
        panic!("the cold-start index must outline exactly `greet`: {initial:?}");
    };
    assert_eq!(name, "greet");

    project.write(OUTLINED, GREET_GROWN);
    await_outline(&client, "after the first edit", &[("greet".to_string(), start, end + 2)]).await;

    project.write(OUTLINED, GREET_SHIFTED_WITH_FAREWELL);
    let deadline = Instant::now() + EDIT_DEADLINE;
    let after_second = loop {
        let seen = outline(&client).await;
        if seen.len() == 2 || Instant::now() >= deadline {
            break seen;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(after_second.len(), 2, "after the second edit, `farewell` must be outlined: {after_second:?}");
    assert_eq!(after_second[0], ("greet".to_string(), start + 3, end + 5), "{after_second:?}");
    assert_eq!(after_second[1].0, "farewell", "{after_second:?}");

    // The answer is only as good as the index behind it - and a baseline
    // written over a stale graph would survive a restart, so both have to
    // agree, not just the in-flight answer.
    let index = Connection::open(project.index_path()).expect("failed to open the index");
    assert_eq!(ranges_of(&index, OUTLINED, "greet"), vec![(start + 3, end + 5)]);
    assert_eq!(ranges_of(&index, OUTLINED, "farewell").len(), 1);

    client.cancel().await.expect("failed to shut the client down");
}
