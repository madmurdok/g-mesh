//! Acceptance test for task 116: rebuilding the JS/TS plugin, and nothing
//! else, must not leave a daemon serving the graph the previous plugin built.
//!
//! `daemon_build_staleness.rs` covers the same failure one level over, for the
//! core executable, and its docs carry the argument for why a long-lived
//! daemon needs the check at all. This file is about the half that argument
//! left out. Everything in the index is computed by the plugin - a separate
//! Node process, built by `npm`, that the core binary says nothing about - so
//! `cd plugins/typescript && npm run build` after an extractor change used to leave
//! a running daemon holding logic that no longer existed on disk, with
//! `g-mesh status` truthfully reporting "daemon build: this build" beside it.
//!
//! That is not hypothetical: task 115 rewrote how same-file edges resolve, and
//! the index kept answering with the resolution it replaced until the state
//! directory was deleted by hand.
//!
//! # How a rebuild is staged
//!
//! With a real one. Each test installs a private copy of the compiled plugin -
//! a `plugin.toml` beside a copy of `dist/src/*.js`, laid out exactly as the
//! bundled plugin is - into a discovery root of its own, points core at that
//! root through
//! [`PLUGIN_ROOTS_OVERRIDE_ENV`](g_mesh::daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV),
//! and edits a file in the copy while the daemon is up - which is byte for
//! byte what `npm run build` over a changed extractor does. Nothing is
//! doctored and no second binary is compiled: the core executable the shim and
//! the daemon come from is the same file throughout, which is precisely the
//! condition under which the old check saw nothing.
//!
//! The override is what makes this a test of the mechanism rather than of a
//! coincidence. Since task 163 the index's generation
//! (`daemon::registry::indexer_version`) is a digest over every *discovered*
//! plugin's build, so the plugin whose rebuild has to be noticed is the one
//! discovery found - and pointing discovery at the copy is what makes the
//! daemon under test genuinely run, and be judged on, the build these tests
//! edit. [`PLUGIN_PATH_ENV`](g_mesh::daemon::plugin::PLUGIN_PATH_ENV) is still
//! set alongside it, because the *other* half of the chain these tests walk
//! end to end - `daemon::build_stamp`, which is what makes a shim retire the
//! incumbent daemon at all - is keyed off the bundled entry point that
//! variable redirects.
//!
//! The copy still lives *inside* `plugins/typescript/` rather than in a temp
//! directory, because Node resolves `require("tree-sitter")` by walking up
//! from the file that asked - a plugin copied outside the package would fail
//! to load its grammars for reasons that have nothing to do with what is
//! being tested.
//!
//! One consequence of that placement, worth stating because it is not
//! obvious: while a copy exists, the *bundled* plugin directory it sits in has
//! different contents, so the generation string a daemon discovering the
//! bundled plugin would compute moves too. Nothing in the suite observes that -
//! `cargo test` runs one test binary at a time, each copy is created before
//! this file's own daemons start and removed on `Drop` - but a test elsewhere
//! made to run *concurrently* with this file could see an index of its own go
//! stale for reasons it never caused.

use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU32, Ordering};

use g_mesh::daemon::{
    self, manifest::PLUGIN_ROOTS_OVERRIDE_ENV, plugin::PLUGIN_PATH_ENV,
};
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

/// The file edited to stand in for a changed extractor. Any emitted file
/// would do - the fingerprint covers the whole build - but this is the one a
/// real extraction change touches.
const REBUILT_FILE: &str = "extract.js";

/// Where `core/build.rs` leaves the compiled plugin, resolved the same way
/// `daemon::plugin::plugin_entry_path` resolves it.
fn plugin_dist_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript/dist")
}

/// A private, discoverable copy of the compiled plugin, so a test can rebuild
/// "the plugin" without touching the one every other test in the suite is
/// using.
///
/// Laid out as a discovery root of its own (`<root>/typescript/plugin.toml`
/// plus `<root>/typescript/dist/src/*.js`) rather than as a bare directory of
/// `.js` files: that is what `daemon::manifest::discover` reads, and since
/// task 163 discovery's output is what both the daemon's plugin processes and
/// the index's generation string are derived from.
struct PluginBuild {
    /// The discovery root - what [`PLUGIN_ROOTS_OVERRIDE_ENV`] is pointed at.
    root: PathBuf,
    /// `<root>/typescript/` - the plugin directory itself, whose whole
    /// content is what `indexer_version` fingerprints.
    plugin_dir: PathBuf,
}

impl PluginBuild {
    fn copied() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = plugin_dist_dir().join(format!(
            "task-116-plugin-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        // The directory name has to be the manifest's `language` - that is
        // `read_manifest`'s own rule, not a convention this test picks.
        let plugin_dir = root.join("typescript");
        let emitted = plugin_dir.join("dist").join("src");
        std::fs::create_dir_all(&emitted).expect("failed to create a plugin build directory");

        let source = plugin_dist_dir().join("src");
        let entries = std::fs::read_dir(&source).unwrap_or_else(|err| {
            panic!(
                "failed to list the compiled plugin at {} ({err}) - `cargo test` builds it via core/build.rs",
                source.display()
            )
        });
        for entry in entries {
            let entry = entry.expect("failed to read a compiled plugin entry");
            if entry.path().extension().and_then(|ext| ext.to_str()) != Some("js") {
                continue;
            }
            std::fs::copy(entry.path(), emitted.join(entry.file_name()))
                .expect("failed to copy a compiled plugin file");
        }
        assert!(emitted.join("index.js").exists(), "the copied plugin must have an entry point");
        std::fs::write(plugin_dir.join("plugin.toml"), MANIFEST)
            .expect("failed to write the copied plugin's manifest");

        Self { root, plugin_dir }
    }

    fn entry(&self) -> PathBuf {
        self.plugin_dir.join("dist").join("src").join("index.js")
    }

    /// The one emitted file these tests rebuild, resolved inside the copy.
    fn rebuilt_file(&self) -> PathBuf {
        self.plugin_dir.join("dist").join("src").join(REBUILT_FILE)
    }

    /// Rewrites one emitted file, exactly as a `tsc` run over changed sources
    /// would. The appended line is a comment, so the plugin that comes back up
    /// still behaves identically - the test is about whether the *change* is
    /// noticed, not about what the change does.
    fn rebuild_with_a_change(&self) {
        let path = self.rebuilt_file();
        let mut source = std::fs::read_to_string(&path).expect("failed to read the emitted extractor");
        source.push_str("\n// rebuilt with different extraction logic\n");
        std::fs::write(&path, source).expect("failed to rewrite the emitted extractor");
    }

    /// A rebuild that emitted the same bytes - the common case, since `npm run
    /// build` rewrites everything whether or not anything changed.
    fn rebuild_unchanged(&self) {
        let path = self.rebuilt_file();
        let source = std::fs::read(&path).expect("failed to read the emitted extractor");
        // Long enough that the filesystem records a different mtime, which is
        // what makes this a real test of "content, not mtime".
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, source).expect("failed to re-emit the extractor");
    }
}

impl Drop for PluginBuild {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The copied plugin's manifest: the bundled one's, with `dist/src` where the
/// copy really put the emitted files. Written out rather than copied from
/// `plugins/typescript/plugin.toml` so a test failure points at a mismatch
/// between core and this fixture, not at a file two tests share.
const MANIFEST: &str = r#"
[plugin]
language = "typescript"
protocol_version = 1
plugin_version = "2.0.0"

[plugin.spawn]
command = "node"
args = ["dist/src/index.js"]

[plugin.languages]
extensions = [".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"]
"#;

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
        daemon::read_pid_file(&path)
            .unwrap_or_else(|| panic!("no daemon pid recorded at {}", path.display()))
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
                let _ = StdCommand::new("kill")
                    .arg("-9")
                    .arg(pid.to_string())
                    .stderr(std::process::Stdio::null())
                    .status();
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
    let entry = project.plugin.entry();
    let root = project.plugin.root.clone();
    let transport = TokioChildProcess::new(Command::new(BIN).configure(|cmd| {
        cmd.arg("mcp-shim")
            .current_dir(project.root())
            .env_remove(g_mesh::shim::PROJECT_DIR_ENV)
            .env(PLUGIN_PATH_ENV, &entry)
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
        .env(PLUGIN_PATH_ENV, project.plugin.entry())
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
    assert!(reported.contains("JS/TS plugin that has been rebuilt"), "{reported}");
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
        generation_after.starts_with("1+"),
        "core's own pipeline generation is still half of it: {generation_after}"
    );
}

/// The control that makes the test above about the plugin's *content* and not
/// merely about its mtime. `npm run build` rewrites every emitted file on
/// every invocation, so a rebuild that changed nothing is the common case -
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
