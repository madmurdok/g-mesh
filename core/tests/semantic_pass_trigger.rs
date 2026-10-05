//! Proves the two moments core is specified to ask for a semantic pass: once
//! the cold-start bulk walk has landed, and again after every incremental
//! reparse settles.
//!
//! Both are properties of the *daemon's* sequencing, not of any one function,
//! so this drives the real `g-mesh daemon` binary and watches what actually
//! arrives on the plugin's stdin. The plugin here is a stub rather than the
//! real TypeScript one: what is being tested is which requests core sends and
//! in what order, and a stub can record that without a parse in the way. The
//! stub is `g-mesh-fake-plugin` (`plugins/sdk/fake/main.rs`), which exists
//! only after `cargo build --workspace`. `plugin_bridge.rs` covers the same
//! wire against the real plugin.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;

mod common;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(20);

/// Where the stub writes one line per control message it received. Read by
/// the daemon's child processes, not by core - it is this test's own channel.
const METHOD_LOG_ENV: &str = "G_MESH_FAKE_PLUGIN_LOG";

/// The stub plugin, beside the daemon under test. In stub mode (no `--dir`)
/// it handshakes, records every method it is asked for in
/// [`METHOD_LOG_ENV`], answers `fileChanged`/`semanticPass` with an empty diff
/// and anything else with an acknowledgement, and walks to one canned `File`
/// node so the walk has something real to commit.
const FAKE_PLUGIN_BIN: &str = "g-mesh-fake-plugin";

/// The project being indexed and the scratch space this test's own machinery
/// lives in. They are deliberately two directories: the stub's manifest and
/// method log must sit *outside* the watched root, or writing a log line would look
/// like a source edit and feed the watcher its own tail.
struct Harness {
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness = Self { project: tempfile::tempdir().unwrap(), aux: tempfile::tempdir().unwrap() };
        let stub = Path::new(BIN)
            .parent()
            .expect("the daemon binary has a directory")
            .join(format!("{FAKE_PLUGIN_BIN}{}", std::env::consts::EXE_SUFFIX));
        assert!(stub.is_file(), "{} does not exist - run `cargo build --workspace` first", stub.display());
        fs::write(harness.method_log(), "").unwrap();
        fs::write(harness.root().join("seed.ts"), "export const seed = 1;\n").unwrap();

        // A manifest under `G_MESH_PLUGIN_ROOTS_OVERRIDE` points both the
        // bulk walk and the interactive supervisor at the stub.
        let plugins_root = harness.aux.path().join("plugins");
        let language_dir = plugins_root.join("typescript");
        fs::create_dir_all(&language_dir).unwrap();
        fs::write(
            language_dir.join("plugin.toml"),
            format!(
                "[plugin]\nlanguage = \"typescript\"\nprotocol_version = 2\nplugin_version = \"0.1.0\"\n\n\
                 [plugin.spawn]\ncommand = \"${{G_MESH_BIN_DIR}}/{FAKE_PLUGIN_BIN}\"\n\
                 args = [\"--language\", \"typescript\", \"--plugin-version\", \"0.1.0\"]\n\n\
                 [plugin.languages]\nextensions = [\".ts\"]\n\n\
                 [plugin.capabilities]\nsemantic_pass = true\n",
            ),
        )
        .unwrap();

        harness
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn plugins_root(&self) -> PathBuf {
        self.aux.path().join("plugins")
    }

    fn method_log(&self) -> PathBuf {
        self.aux.path().join("methods.log")
    }

    fn spawn_daemon(&self) -> Child {
        Command::new(BIN)
            .arg("daemon")
            .arg("--project-root")
            .arg(self.root())
            .env(daemon::manifest::PLUGIN_ROOTS_OVERRIDE_ENV, self.plugins_root())
            .env(METHOD_LOG_ENV, self.method_log())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn the daemon")
    }

    /// Every control method the stub has been asked for so far, in order.
    fn methods(&self) -> Vec<String> {
        fs::read_to_string(self.method_log()).unwrap_or_default().lines().map(str::to_string).collect()
    }

    fn wait_for(&self, what: &str, mut ready: impl FnMut(&[String]) -> bool) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            let methods = self.methods();
            if ready(&methods) {
                return methods;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {what}; methods so far: {:?}", self.methods());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Ok(state) = project_dir(self.root()) {
            let _ = fs::remove_dir_all(&state);
        }
    }
}

fn position_of(methods: &[String], method: &str) -> Option<usize> {
    methods.iter().position(|m| m == method)
}

/// Path 1: the cold start. The walk commits, the daemon starts answering off
/// it, and only then is the semantic layer asked to improve on it.
#[test]
fn core_asks_for_a_semantic_pass_once_the_bulk_index_is_built() {
    let harness = Harness::new();
    let mut daemon = harness.spawn_daemon();
    // GM-395 slice 2: the daemon walks - and registers its watcher - only once a tool call asks.
    common::trigger_activation(harness.root());

    let methods = harness.wait_for("a semantic pass after the bulk walk", |methods| {
        methods.iter().any(|m| m == "semanticPass")
    });

    let bulk = position_of(&methods, "bulkIndex").expect("the stub's bulk-index mode must have run");
    let pass = position_of(&methods, "semanticPass").unwrap();
    assert!(bulk < pass, "the pass must follow the walk it upgrades, not race it: {methods:?}");

    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// Path 2: an ordinary edit. The reparse settles - diff committed and linked -
/// and the pass follows it over that same file, on the same connection.
#[test]
fn core_asks_for_a_semantic_pass_after_each_incremental_reparse() {
    let harness = Harness::new();
    let mut daemon = harness.spawn_daemon();
    // GM-395 slice 2: the daemon walks - and registers its watcher - only once a tool call asks.
    common::trigger_activation(harness.root());

    // Let the cold-start pass happen first, so what is counted afterwards can
    // only have come from the watcher.
    let after_startup = harness
        .wait_for("the daemon to finish starting up", |methods| methods.iter().any(|m| m == "semanticPass"))
        .len();

    // Re-edited until the watcher answers, rather than written once. The
    // watcher is registered a moment *after* the cold-start pass this test
    // just waited for (see `daemon::run`), and nothing marks the instant it
    // starts - so a single write can land in that gap, where it is not
    // missed-and-retried but missed outright, and the test would then hang
    // on an event that is never coming. Editing again costs nothing when the
    // watcher is already up: the first edit answers and the loop ends.
    //
    // The gap between retries has to clear `daemon::DEBOUNCE_WINDOW` (300ms
    // as of task 129, private to that module) with margin, not just be
    // "long enough to answer": the daemon's watcher thread now waits for a
    // path to go quiet before routing it, so a retry cadence shorter than
    // that window would keep re-arming the very same debounce timer forever
    // - each write looking, from the watcher's side, like the burst still
    // going on rather than a fresh, isolated edit - and this loop would never
    // observe a settle at all.
    let deadline = Instant::now() + TIMEOUT;
    let mut edits = 0;
    let methods = loop {
        edits += 1;
        fs::write(
            harness.root().join("edited.ts"),
            format!("export function edited(): number {{\n  return {edits};\n}}\n"),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(700));

        let methods = harness.methods();
        // Only the tail is considered, so a reparse can only be credited to
        // the edit above - never to anything startup did.
        let settled = methods.get(after_startup..).is_some_and(|tail| {
            position_of(tail, "fileChanged")
                .is_some_and(|changed| tail[changed..].iter().any(|m| m == "semanticPass"))
        });
        if settled {
            break methods;
        }
        assert!(
            Instant::now() < deadline,
            "no reparse-and-pass arrived after {edits} edit(s); methods: {methods:?}"
        );
    };

    let tail = &methods[after_startup..];
    let changed = position_of(tail, "fileChanged").expect("the watcher must have routed the edit");
    assert!(
        tail[changed..].iter().any(|m| m == "semanticPass"),
        "a settled reparse must be followed by a semantic pass: {tail:?}"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}
