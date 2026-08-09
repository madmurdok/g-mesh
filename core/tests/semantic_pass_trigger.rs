//! Proves the two moments core is specified to ask for a semantic pass: once
//! the cold-start bulk walk has landed, and again after every incremental
//! reparse settles.
//!
//! Both are properties of the *daemon's* sequencing, not of any one function,
//! so this drives the real `g-mesh daemon` binary and watches what actually
//! arrives on the plugin's stdin. The plugin here is a stub rather than the
//! bundled JS/TS one (`G_MESH_JS_TS_PLUGIN_PATH` is the same override
//! `daemon::plugin` documents for exactly this): what is being tested is
//! which requests core sends and in what order, and a stub can record that
//! without a tree-sitter parse in the way. `plugin_bridge.rs` covers the
//! same wire against the real plugin.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use g_mesh::daemon;
use g_mesh::storage::connection::project_dir;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");
const TIMEOUT: Duration = Duration::from_secs(20);

/// Where the stub writes one line per control message it received. Read by
/// the daemon's child processes, not by core - it is this test's own channel.
const METHOD_LOG_ENV: &str = "G_MESH_FAKE_PLUGIN_LOG";

/// A stub plugin: it speaks the control plane correctly (handshake, framing,
/// a diff-shaped answer to anything that expects one) and records every
/// method it is asked for. Its `--bulk-index` mode emits one canned NDJSON
/// node so the walk has something real to commit.
const STUB_PLUGIN: &str = r#""use strict";
const fs = require("fs");

const LOG = process.env.G_MESH_FAKE_PLUGIN_LOG;
const args = process.argv.slice(2);

function record(entry) {
  fs.appendFileSync(LOG, entry + "\n");
}

function writeFrame(obj) {
  const body = Buffer.from(JSON.stringify(obj), "utf8");
  process.stdout.write("Content-Length: " + body.length + "\r\n\r\n");
  process.stdout.write(body);
}

const EMPTY_DIFF = { upsertNodes: [], deleteNodeIds: [], upsertEdges: [], deleteEdgeIds: [] };

function handle(envelope) {
  record(envelope.method);
  if (envelope.id === undefined) return;
  if (envelope.method === "fileChanged" || envelope.method === "semanticPass") {
    writeFrame({ jsonrpc: "2.0", id: envelope.id, result: EMPTY_DIFF });
  } else {
    writeFrame({ jsonrpc: "2.0", id: envelope.id, result: { acknowledged: true } });
  }
}

if (args[0] === "--bulk-index") {
  record("bulkIndex");
  process.stdout.write(
    JSON.stringify({
      id: "n1",
      kind: "File",
      name: "seed.ts",
      qualifiedName: "seed.ts",
      filePath: "seed.ts",
      range: { start: { line: 0, col: 0 }, end: { line: 0, col: 0 } },
      exported: false,
      language: "typescript",
    }) + "\n",
  );
} else {
  writeFrame({ protocolVersion: 1, language: "typescript", pluginVersion: "0.1.0" });

  let buffer = Buffer.alloc(0);
  process.stdin.on("data", (chunk) => {
    buffer = Buffer.concat([buffer, chunk]);
    for (;;) {
      const headerEnd = buffer.indexOf("\r\n\r\n");
      if (headerEnd === -1) return;
      const match = /Content-Length:\s*(\d+)/i.exec(buffer.slice(0, headerEnd).toString("utf8"));
      if (!match) return;
      const length = Number(match[1]);
      const start = headerEnd + 4;
      if (buffer.length < start + length) return;
      const body = buffer.slice(start, start + length).toString("utf8");
      buffer = buffer.slice(start + length);
      handle(JSON.parse(body));
    }
  });
  process.stdin.on("end", () => process.exit(0));
}
"#;

/// The project being indexed and the scratch space this test's own machinery
/// lives in. They are deliberately two directories: the stub and its method
/// log must sit *outside* the watched root, or writing a log line would look
/// like a source edit and feed the watcher its own tail.
struct Harness {
    project: tempfile::TempDir,
    aux: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let harness =
            Self { project: tempfile::tempdir().unwrap(), aux: tempfile::tempdir().unwrap() };
        fs::write(harness.plugin_path(), STUB_PLUGIN).unwrap();
        fs::write(harness.method_log(), "").unwrap();
        fs::write(harness.root().join("seed.ts"), "export const seed = 1;\n").unwrap();

        // As of task 156, `daemon::bulk_index` no longer resolves the plugin
        // via `plugin::PLUGIN_PATH_ENV` at all - it walks whatever
        // `daemon::manifest::discover` found, exactly like the interactive
        // supervisor a semantic pass runs against always has; and as of task
        // 163, `registry::indexer_version` fingerprints every *discovered*
        // manifest rather than the one bundled-plugin view
        // `plugin::bundled_manifest` resolves from `PLUGIN_PATH_ENV`. Nothing
        // this test exercises reads that env var any more - only
        // `daemon::build_stamp` still does, for a question (is this running
        // daemon's JS/TS build still mine?) this test has no reason to ask -
        // so it is not set here. A manifest under `G_MESH_PLUGIN_ROOTS_OVERRIDE`
        // - `daemon::manifest`'s own generalized discovery override - is what
        // points both the bulk walk and the interactive supervisor at this
        // stub.
        let plugins_root = harness.aux.path().join("plugins");
        let language_dir = plugins_root.join("typescript");
        fs::create_dir_all(&language_dir).unwrap();
        fs::write(
            language_dir.join("plugin.toml"),
            format!(
                "[plugin]\nlanguage = \"typescript\"\nprotocol_version = 1\nplugin_version = \"0.1.0\"\n\n\
                 [plugin.spawn]\ncommand = \"node\"\nargs = [\"{}\"]\n\n\
                 [plugin.languages]\nextensions = [\".ts\"]\n",
                harness.plugin_path().to_string_lossy().replace('\\', "\\\\"),
            ),
        )
        .unwrap();

        harness
    }

    fn root(&self) -> &Path {
        self.project.path()
    }

    fn plugin_path(&self) -> PathBuf {
        self.aux.path().join("index.js")
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
        fs::read_to_string(self.method_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
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

    let methods = harness.wait_for("a semantic pass after the bulk walk", |methods| {
        methods.iter().any(|m| m == "semanticPass")
    });

    let bulk = position_of(&methods, "bulkIndex").expect("the stub's bulk-index mode must have run");
    let pass = position_of(&methods, "semanticPass").unwrap();
    assert!(
        bulk < pass,
        "the pass must follow the walk it upgrades, not race it: {methods:?}"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}

/// Path 2: an ordinary edit. The reparse settles - diff committed and linked -
/// and the pass follows it over that same file, on the same connection.
#[test]
fn core_asks_for_a_semantic_pass_after_each_incremental_reparse() {
    let harness = Harness::new();
    let mut daemon = harness.spawn_daemon();

    // Let the cold-start pass happen first, so what is counted afterwards can
    // only have come from the watcher.
    let after_startup = harness
        .wait_for("the daemon to finish starting up", |methods| {
            methods.iter().any(|m| m == "semanticPass")
        })
        .len();

    // Re-edited until the watcher answers, rather than written once. The
    // watcher is registered a moment *after* the cold-start pass this test
    // just waited for (see `daemon::run`), and nothing marks the instant it
    // starts - so a single write can land in that gap, where it is not
    // missed-and-retried but missed outright, and the test would then hang
    // on an event that is never coming. Editing again costs nothing when the
    // watcher is already up: the first edit answers and the loop ends.
    let deadline = Instant::now() + TIMEOUT;
    let mut edits = 0;
    let methods = loop {
        edits += 1;
        fs::write(
            harness.root().join("edited.ts"),
            format!("export function edited(): number {{\n  return {edits};\n}}\n"),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(100));

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
