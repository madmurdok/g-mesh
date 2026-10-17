//! Rebuilding a plugin, and nothing else, must not leave a daemon serving the
//! graph the previous plugin build produced.
//!
//! `daemon_build_staleness.rs` covers the same failure one level over, for the
//! core executable, and its docs carry the argument for why a long-lived
//! daemon needs the check at all. This file is about the half that argument
//! leaves out. Everything in the index is computed by a plugin - a separate
//! binary the core executable says nothing about - so a rebuilt plugin under a
//! running daemon would otherwise leave that daemon holding logic that no
//! longer exists on disk, with `g-mesh status` truthfully reporting "daemon
//! build: this build" beside it.
//!
//! # How a rebuild is staged
//!
//! Each test installs a private copy of the TypeScript plugin's
//! `plugin.toml` into a discovery root of its own and points core at that root
//! through
//! [`PLUGIN_ROOTS_OVERRIDE_ENV`](g_mesh::daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV).
//! The manifest's command names the workspace-built binary by
//! `${G_MESH_BIN_DIR}`, so the copy runs the real plugin. A "rebuild" is a byte
//! change to a file in the copied plugin directory, which is what
//! `daemon::plugin::fingerprint` hashes. The core executable the shim and the
//! daemon come from is the same file throughout, which is precisely the
//! condition under which a check on the executable alone sees nothing.
//!
//! The override drives both halves of the chain these tests walk end to end:
//! the index's generation (`daemon::registry::indexer_version`) and the build
//! stamp (`daemon::build_stamp`, which is what makes a shim retire the
//! incumbent daemon at all) are both digests over the *discovered* plugins.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;

use g_mesh::daemon::{self, manifest::PLUGIN_ROOTS_OVERRIDE_ENV};
use g_mesh::storage::connection::project_dir;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::ServiceExt;
use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::process::Command;

mod common;

use common::wait_until_indexed;
use common::Lifeline;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// One file importing another - the same minimal graph the build-staleness
/// tests use, chosen because its `Incoming` walk is non-empty and so can be
/// emptied by hand and seen to come back.
const FILES: [(&str, &str); 2] = [
    (
        "src/index.ts",
        r#"import { connect } from "./db/connection.js";

export function start(): number {
  return connect();
}
"#,
    ),
    ("src/db/connection.ts", "export function connect(): number {\n  return 1;\n}\n"),
];

/// The file edited to stand in for a changed plugin build. Any file in the
/// plugin directory would do - the fingerprint covers the whole directory.
const REBUILT_FILE: &str = "build-output";

/// A private, discoverable copy of the TypeScript plugin, so a test can
/// rebuild "the plugin" without touching the one every other test in the
/// suite is using.
///
/// Laid out as a discovery root of its own (`<root>/typescript/plugin.toml`),
/// since that is what `daemon::manifest::discover` reads.
struct PluginBuild {
    /// Owns the discovery root, removed on drop.
    _dir: tempfile::TempDir,
    /// The discovery root - what [`PLUGIN_ROOTS_OVERRIDE_ENV`] is pointed at.
    root: PathBuf,
    /// `<root>/typescript/` - the plugin directory itself, whose whole
    /// content is what `indexer_version` fingerprints.
    plugin_dir: PathBuf,
}

impl PluginBuild {
    fn copied() -> Self {
        let dir = tempfile::tempdir().expect("failed to create a plugin root");
        let root = dir.path().to_path_buf();
        // The directory name has to be the manifest's `language` - that is
        // `read_manifest`'s own rule, not a convention this test picks.
        let plugin_dir = root.join("typescript");
        std::fs::create_dir_all(&plugin_dir).expect("failed to create the plugin directory");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript/plugin.toml"),
            plugin_dir.join("plugin.toml"),
        )
        .expect("failed to copy the TypeScript plugin's manifest");
        std::fs::write(plugin_dir.join(REBUILT_FILE), "the first build\n")
            .expect("failed to write the stand-in build output");

        Self { _dir: dir, root, plugin_dir }
    }

    /// Adds a second plugin to the same discovery root: the workspace's fake
    /// plugin under the language `fake`, claiming an extension no fixture file
    /// has. Its directory is returned, holding a stand-in build output of its
    /// own.
    fn with_a_second_plugin(&self) -> PathBuf {
        let dir = self.root.join("fake");
        std::fs::create_dir_all(&dir).expect("failed to create the second plugin's directory");
        std::fs::write(
            dir.join("plugin.toml"),
            "[plugin]\nlanguage = \"fake\"\nprotocol_version = 2\nplugin_version = \"0.1.0\"\n\n\
             [plugin.spawn]\ncommand = \"${G_MESH_BIN_DIR}/g-mesh-fake-plugin\"\n\
             args = [\"--language\", \"fake\", \"--plugin-version\", \"0.1.0\"]\n\n\
             [plugin.languages]\nextensions = [\".fk\"]\n",
        )
        .expect("failed to write the second plugin's manifest");
        std::fs::write(dir.join(REBUILT_FILE), "the first build\n")
            .expect("failed to write the second plugin's stand-in build output");
        dir
    }

    /// The one file these tests rebuild, resolved inside the copy.
    fn rebuilt_file(&self) -> PathBuf {
        self.plugin_dir.join(REBUILT_FILE)
    }

    /// Rewrites the stand-in build output with different bytes. The plugin
    /// that comes back up still behaves identically - the test is about
    /// whether the *change* is noticed, not about what the change does.
    fn rebuild_with_a_change(&self) {
        let path = self.rebuilt_file();
        let mut source = std::fs::read_to_string(&path).expect("failed to read the build output");
        source.push_str("rebuilt with different extraction logic\n");
        std::fs::write(&path, source).expect("failed to rewrite the build output");
    }

    /// A rebuild that emitted the same bytes.
    fn rebuild_unchanged(&self) {
        let path = self.rebuilt_file();
        let source = std::fs::read(&path).expect("failed to read the build output");
        // Long enough that the filesystem records a different mtime, which is
        // what makes this a real test of "content, not mtime".
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, source).expect("failed to re-emit the build output");
    }
}

struct Project {
    dir: tempfile::TempDir,
    plugin: PluginBuild,
}

impl Project {
    fn new() -> Self {
        let project = Self {
            dir: tempfile::tempdir().expect("failed to create a temp project root"),
            plugin: PluginBuild::copied(),
        };
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

    fn index_path(&self) -> PathBuf {
        project_dir(self.root()).expect("failed to resolve the state directory").join("index.db")
    }

    fn daemon_pid(&self) -> u32 {
        let path = daemon::pid_path(self.root()).expect("failed to resolve the pid file path");
        daemon::read_pid_file(&path).unwrap_or_else(|| panic!("no daemon pid recorded at {}", path.display()))
    }

    /// The generation the index says it was filled by. The plugin's half of it
    /// is what these tests are ultimately about.
    fn recorded_generation(&self) -> String {
        let conn = Connection::open(self.index_path()).expect("failed to open the project index");
        conn.query_row("SELECT indexer_version FROM meta WHERE id = 1", [], |row| row.get(0))
            .expect("failed to read the recorded indexer generation")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        for path in [daemon::pid_path(self.root()), daemon::plugin_pid_path(self.root())] {
            let Ok(path) = path else { continue };
            if let Some(pid) = daemon::read_pid_file(&path) {
                common::kill_and_wait(pid);
            }
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

/// One `get_dependencies` call through a freshly spawned shim, pointed at this
/// project's private plugin build - and deliberately without stopping the
/// daemon behind it, which is the whole subject of this file.
async fn importers_of(project: &Project, file_path: &str) -> Vec<String> {
    let root = project.plugin.root.clone();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.lifeline();
        // `kill_on_drop`, because a shim that outlives the test wedges the
        // whole process on Windows (GM-249 - see `common::kill_and_wait`).
        cmd.kill_on_drop(true)
            .arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            // Inherited by the daemon this shim bootstraps, which is what has
            // to discover (and be judged on) this test's own plugin build
            // rather than the bundled one every other test uses.
            .env(PLUGIN_ROOTS_OVERRIDE_ENV, &root);
    }))
    .expect("failed to spawn the shim");
    let client = ().serve(transport).await.expect("MCP initialization failed");
    // After the connection is up, for the reason `daemon_build_staleness.rs`
    // spells out: a replacement daemon wipes the index before it binds, so a
    // completion marker readable from here can only be the new walk's own.
    wait_until_indexed(project.root());

    let result = client
        .call_tool(
            CallToolRequestParams::new("get_dependencies").with_arguments(
                json!({ "file_path": file_path, "direction": "Incoming" })
                    .as_object()
                    .cloned()
                    .expect("arguments literal is an object"),
            ),
        )
        .await
        .expect("tools/call failed");
    let walk = body(&result);

    client.cancel().await.expect("failed to shut the client down");

    let mut paths: Vec<String> = walk["results"]
        .as_array()
        .expect("results is not an array")
        .iter()
        .filter_map(|row| row["filePath"].as_str().map(str::to_string))
        .collect();
    paths.sort();
    paths
}

/// Empties the one thing the query under test looks at, while leaving every
/// file and symbol in place - so a wrong answer reads as "this query has no
/// answer" rather than "this project is unknown", which is how the real
/// failure presented.
///
/// The recorded generation is deliberately *not* touched: whether the index is
/// thrown away has to follow from the plugin having been rebuilt, and nothing
/// else. Written straight into the database the running daemon has open.
fn strip_the_import_edges(index: &Path) {
    let conn = Connection::open(index).expect("failed to open the project index");
    let removed =
        conn.execute("DELETE FROM edges WHERE kind = 'IMPORTS'", []).expect("failed to strip edges");
    assert!(removed > 0, "the fixture must have had import edges to strip");
}

fn status(project: &Project) -> String {
    let output = StdCommand::new(BIN)
        .arg("status")
        .current_dir(project.root())
        .env(PLUGIN_ROOTS_OVERRIDE_ENV, &project.plugin.root)
        .output()
        .expect("failed to run `g-mesh status`");
    assert!(
        output.status.success(),
        "`g-mesh status` failed with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("status output is not valid UTF-8")
}

/// The headline case: the plugin is rebuilt with different logic while a
/// daemon is up, the core binary is not touched at all, and the very next MCP
/// call is answered off an index the new plugin walked.
#[tokio::test]
async fn a_plugin_rebuilt_under_a_running_daemon_costs_the_project_a_re_walk() {
    let project = Project::new();

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "the cold-start index must answer this before anything is done to it"
    );
    let daemon_holding_the_old_plugin = project.daemon_pid();
    let generation_before = project.recorded_generation();

    strip_the_import_edges(&project.index_path());
    project.plugin.rebuild_with_a_change();

    // `status` answers from outside the daemon, so asking must not itself be
    // what fixes anything - and it is the discoverable signal for a human who
    // suspects a rebuild has not taken.
    let reported = status(&project);
    assert!(reported.contains("holding a plugin that has been rebuilt"), "{reported}");
    assert!(reported.contains("g-mesh stop"), "the report has to say what to do: {reported}");
    assert_eq!(
        project.daemon_pid(),
        daemon_holding_the_old_plugin,
        "reporting on a daemon must never be a way of restarting one"
    );

    assert_eq!(
        importers_of(&project, "src/db/connection.ts").await,
        vec!["src/index.ts".to_string()],
        "a daemon holding a plugin that has been rebuilt must not answer off the index it built"
    );
    assert_ne!(
        project.daemon_pid(),
        daemon_holding_the_old_plugin,
        "the answer has to come from a daemon running the plugin that is on disk now"
    );

    let generation_after = project.recorded_generation();
    assert_ne!(
        generation_after, generation_before,
        "the index has to record the plugin build that filled it, or the next start repeats this"
    );
    assert!(
        generation_after.starts_with(&format!("{}+", g_mesh::storage::schema::CURRENT_INDEXER_VERSION)),
        "core's own pipeline generation is still half of it: {generation_after}"
    );
}

/// The control that makes the test above about the plugin's *content* and not
/// merely about its mtime. A rebuild can rewrite a file with identical bytes,
/// and charging a project a full re-walk for it would make the mechanism
/// expensive enough to be worth turning off.
#[tokio::test]
async fn a_plugin_re_emitted_to_the_same_bytes_leaves_the_daemon_and_its_index_alone() {
    let project = Project::new();

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    let incumbent = project.daemon_pid();

    strip_the_import_edges(&project.index_path());
    project.plugin.rebuild_unchanged();

    assert!(
        importers_of(&project, "src/db/connection.ts").await.is_empty(),
        "nothing about the pipeline changed, so the daemon is not restarted and the \
         doctored graph is what it still has to serve"
    );
    assert_eq!(project.daemon_pid(), incumbent, "a re-emitted identical plugin must not retire anything");
}

/// The stamp covers every discovered plugin, not only TypeScript's: rebuilding
/// a plugin that no file of this project is even routed to still retires the
/// daemon, since it holds that plugin's old build all the same.
#[tokio::test]
async fn a_rebuild_of_any_discovered_plugin_retires_the_daemon_not_only_typescripts() {
    let project = Project::new();
    let second_plugin = project.plugin.with_a_second_plugin();

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    let incumbent = project.daemon_pid();

    let rebuilt = second_plugin.join(REBUILT_FILE);
    let mut output = std::fs::read_to_string(&rebuilt).expect("failed to read the build output");
    output.push_str("rebuilt with different extraction logic\n");
    std::fs::write(&rebuilt, output).expect("failed to rewrite the build output");

    let reported = status(&project);
    assert!(reported.contains("holding a plugin that has been rebuilt"), "{reported}");

    assert_eq!(importers_of(&project, "src/db/connection.ts").await, vec!["src/index.ts".to_string()]);
    assert_ne!(project.daemon_pid(), incumbent, "a daemon holding the old build of any plugin is replaced");
}
