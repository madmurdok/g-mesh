//! A stand-in language plugin, for unit tests that need a *second* language
//! to exist.
//!
//! Every existing test that spawns a plugin spawns the real bundled JS/TS one
//! (`core/tests/plugin_crash_recovery.rs`, `daemon::plugin`'s own tests, via
//! `daemon::plugin::bundled_manifest`), because until now there was exactly
//! one plugin to spawn. `daemon::registry`'s whole subject is what happens
//! with *more* than one - routing between them, spawning them independently,
//! keeping one language's crash away from another's - and none of that can be
//! tested against a single plugin, however real.
//!
//! So this module writes a plugin directory that is real in every way
//! `daemon::manifest`, `daemon::plugin` and `daemon::lifecycle` care about -
//! a `plugin.toml` that `read_manifest`/`discover` parse like any other, and
//! a Node entry point that speaks the actual wire protocol (`Content-Length`
//! framed JSON-RPC: a handshake first, then one `FileChangeResponse` per
//! request). It answers every request with an *empty* diff, which is the
//! point: these tests are about which process gets asked, not about what a
//! parser makes of a file.
//!
//! It also answers a one-shot `--bulk-index` invocation
//! (`daemon::bulk_index`'s own spawn shape - see that module's tests), the
//! same way the real bundled plugin does: a fixed, deterministic NDJSON
//! stream of two nodes and the edge between them, named after this fake
//! plugin's own language so a test summing two languages' output can tell
//! whose contribution is whose.
//!
//! Node, rather than a shell script, for the same reason the real plugin uses
//! it: it is already a hard dependency of this crate's test suite
//! (`core/build.rs` runs `npm run build` and every plugin-spawning test
//! shells out to `node`), and framed-message parsing in `sh` would be its own
//! source of test failures.
//!
//! # Counting spawns
//!
//! Each fake process appends its own pid to `spawns.log` in its plugin
//! directory before it does anything else, so a test can ask *how many
//! processes this manifest has ever produced* ([`spawns`]) rather than
//! inferring it from a pid that happens to look the same. That is what makes
//! "the second file of the same language reuses the first supervisor" and
//! "waking a sleeping supervisor re-spawns the plugin its own manifest names"
//! into assertions about processes rather than about return values.
//!
//! # Counting round trips
//!
//! Same idea, one level down: every framed request that carries an id (a
//! real round trip, as opposed to the handshake or a notification) is
//! appended to `requests.log` in the same directory, as `"<method>
//! <filePath>"`, before it is answered ([`requests`]). Task 129 is what
//! first needed this - "a burst of rapid saves to the same file costs one
//! plugin round trip, not one per save" is a claim about how many times the
//! plugin was actually asked, and `spawns.log` alone cannot distinguish a
//! debounced burst from a single lucky one that never crashed the process it
//! was already talking to.
//!
//! [`file_changed_requests`] narrows that log to just the `fileChanged`
//! entries - what `PluginSupervisor::file_changed`/`apply_file_change`
//! actually sends per incremental reparse - excluding the `semanticPass`
//! request `apply_file_change` also always sends on the very same round trip
//! (`watcher::apply::apply_file_change`'s own doc comment): that one is a
//! fixed 1:1 side effect of a `fileChanged` request, not a second thing a
//! debounce test is checking, so counting both together would double every
//! number for a reason unrelated to what changed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use crate::protocol::types::CURRENT_PROTOCOL_VERSION;
use crate::storage::schema;

/// The file each fake plugin process appends its pid to on startup.
const SPAWN_LOG: &str = "spawns.log";

/// The file a *gated* plugin waits for before it announces its handshake -
/// see [`install_gated`] and [`open_handshake_gate`].
const HANDSHAKE_GATE: &str = "handshake.allow";

/// The file each fake plugin process appends one line to per answered
/// request that carried an id - see this module's "Counting round trips" doc.
const REQUEST_LOG: &str = "requests.log";

/// Writes a discoverable plugin directory named `language` under `root`,
/// claiming `extensions`, and returns the directory it created.
///
/// Laid out exactly as a real one is (`<root>/<language>/plugin.toml`), so
/// `daemon::manifest::discover(&[root])` picks it up with no test-only path
/// through discovery.
pub(crate) fn install(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, false)
}

/// [`install`], but the plugin does not answer its handshake until
/// [`open_handshake_gate`] is called - a spawn that is *held open* for as long
/// as a test wants to look at what the rest of the daemon does meanwhile
/// (`daemon::registry`'s task-164 tests).
///
/// A gate rather than a sleep, deliberately. What those tests are about is a
/// spawn that is in flight *right now*, and a real one is a process launch
/// plus whatever the plugin does before it can speak - hundreds of
/// milliseconds, but a different number on every machine and a wildly
/// different one under a loaded `cargo test`, where a bare `node` start has
/// been measured taking over a second. Any test that raced a fixed delay
/// would be asserting about this machine's scheduler as much as about the
/// daemon. A gate removes wall-clock time from the question entirely: the
/// spawn stays in flight until the test says otherwise, so "while a spawn is
/// in progress" is a state the test *holds*, not one it hopes to catch.
///
/// The gate sits in front of the handshake frame specifically, because that is
/// what `PluginProcess::spawn` blocks on. The process itself still starts, and
/// still records its pid in `spawns.log` first, so a test can tell "the spawn
/// is in flight" from "it has not started yet" by observation ([`spawns`]).
///
/// Every test that installs one of these **must** open its gate, on every path
/// including a failing assertion: a spawning thread that is never let go never
/// joins.
pub(crate) fn install_gated(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    install_inner(root, language, extensions, true)
}

/// Lets the plugin(s) installed in `plugin_dir` finish their handshake - see
/// [`install_gated`]. Idempotent, and safe to call on a plugin that was never
/// gated in the first place.
pub(crate) fn open_handshake_gate(plugin_dir: &Path) {
    fs::write(plugin_dir.join(HANDSHAKE_GATE), "go\n")
        .expect("failed to open the fake plugin's handshake gate");
}

fn install_inner(root: &Path, language: &str, extensions: &[&str], gated: bool) -> PathBuf {
    let dir = root.join(language);
    fs::create_dir_all(&dir).expect("failed to create the fake plugin's directory");
    fs::write(dir.join("plugin.js"), entry_point(language, gated))
        .expect("failed to write the fake plugin's entry point");
    fs::write(dir.join("plugin.toml"), manifest(language, extensions))
        .expect("failed to write the fake plugin's manifest");
    dir
}

/// Every pid this plugin directory has ever been spawned as, oldest first.
/// Empty (rather than a panic) before the first spawn - "never spawned" is a
/// perfectly ordinary thing for a test to assert.
pub(crate) fn spawns(plugin_dir: &Path) -> Vec<u32> {
    let Ok(log) = fs::read_to_string(plugin_dir.join(SPAWN_LOG)) else { return Vec::new() };
    log.lines().filter_map(|line| line.trim().parse().ok()).collect()
}

/// Every id-carrying request this plugin directory's process(es) have ever
/// answered, oldest first, across every spawn, as `"<method> <filePath>"`
/// (`filePath` empty for a request with no such field, e.g. `status`).
/// Empty (rather than a panic) before the first one, same as [`spawns`].
pub(crate) fn requests(plugin_dir: &Path) -> Vec<String> {
    let Ok(log) = fs::read_to_string(plugin_dir.join(REQUEST_LOG)) else { return Vec::new() };
    log.lines().map(str::to_string).collect()
}

/// Just the `fileChanged` requests among [`requests`], as the file path each
/// one named - i.e. one entry per real `PluginSupervisor::file_changed` ->
/// `apply_file_change` round trip, the granularity task 129's debounce test
/// cares about. Deliberately excludes the `semanticPass` request
/// `apply_file_change` also always sends on the same round trip
/// (`watcher::apply::apply_file_change`'s own doc): that one is a fixed,
/// pre-existing 1:1 side effect of *this* one, not a second thing debouncing
/// could coalesce away, and counting it in would double every number below
/// for a reason that has nothing to do with what this test is checking.
pub(crate) fn file_changed_requests(plugin_dir: &Path) -> Vec<String> {
    requests(plugin_dir)
        .into_iter()
        .filter_map(|line| line.strip_prefix("fileChanged ").map(str::to_string))
        .collect()
}

/// A fresh in-memory index for the (empty) diffs a fake plugin's round trips
/// commit. Shared by every caller of this module, because none of them cares
/// what is in it - only that the commit path a real file change takes is the
/// one being exercised.
pub(crate) fn empty_index() -> Mutex<Connection> {
    let conn = Connection::open_in_memory().expect("failed to open an in-memory index");
    conn.pragma_update(None, "foreign_keys", "ON").expect("failed to enable foreign keys");
    schema::apply(&conn).expect("failed to apply the schema");
    Mutex::new(conn)
}

fn manifest(language: &str, extensions: &[&str]) -> String {
    let extensions =
        extensions.iter().map(|ext| format!("\"{ext}\"")).collect::<Vec<_>>().join(", ");
    format!(
        r#"
[plugin]
language = "{language}"
protocol_version = {CURRENT_PROTOCOL_VERSION}
plugin_version = "0.0.0-test"

[plugin.spawn]
command = "node"
args = ["./plugin.js"]

[plugin.languages]
extensions = [{extensions}]
"#
    )
}

/// The fake plugin itself: record the spawn, handshake (once its gate is open,
/// if a test asked for a `gated` one), then answer every framed request that
/// carries an id with an empty diff under that same id.
///
/// Deliberately minimal about framing - it re-implements just enough of
/// `protocol::jsonrpc` to be a peer, and would rather hang than guess if core
/// ever sent something it does not understand, since a test that hangs is
/// easier to diagnose than one that silently agrees with a bug.
fn entry_point(language: &str, gated: bool) -> String {
    format!(
        r#"// Generated by core/src/daemon/test_plugin.rs - not a real plugin.
const fs = require("fs");
const path = require("path");

fs.appendFileSync(path.join(__dirname, "{SPAWN_LOG}"), process.pid + "\n");

// One-shot bulk-index mode (`daemon::bulk_index::run`'s spawn shape:
// "<command> <args...> --bulk-index <project_root>"): emit a fixed, small
// NDJSON stream - two nodes and the edge between them, named after this
// plugin's own language - and exit, rather than starting the interactive
// framed-JSON-RPC loop below. Enough for a test to prove two plugins' output
// both landed and summed, not just the first (or only) one's.
if (process.argv[2] === "--bulk-index") {{
  const line = (obj) => process.stdout.write(JSON.stringify(obj) + "\n");
  line({{
    id: "{language}-n1",
    kind: "Function",
    name: "{language}-n1",
    qualifiedName: "{language}-n1",
    filePath: "src/{language}-a.src",
    range: {{ start: {{ line: 0, col: 0 }}, end: {{ line: 1, col: 0 }} }},
    exported: true,
    language: "{language}",
  }});
  line({{
    id: "{language}-n2",
    kind: "Function",
    name: "{language}-n2",
    qualifiedName: "{language}-n2",
    filePath: "src/{language}-b.src",
    range: {{ start: {{ line: 0, col: 0 }}, end: {{ line: 1, col: 0 }} }},
    exported: true,
    language: "{language}",
  }});
  line({{
    id: "{language}-e1",
    fromId: "{language}-n1",
    toId: "{language}-n2",
    kind: "CALLS",
    source: "tree-sitter",
    resolved: true,
  }});
  process.exit(0);
}}

function writeFrame(message) {{
  const body = Buffer.from(JSON.stringify(message), "utf8");
  process.stdout.write("Content-Length: " + body.length + "\r\n\r\n");
  process.stdout.write(body);
}}

// The handshake core blocks on inside `PluginProcess::spawn`. Sent straight
// away (the ordinary case), or held until a test opens this plugin's gate
// file - which is how a test keeps a spawn in flight for as long as it needs
// to look at something else. See `install_gated`.
function announce() {{
  writeFrame({{
    protocolVersion: {CURRENT_PROTOCOL_VERSION},
    language: "{language}",
    pluginVersion: "0.0.0-test",
  }});
}}
if ({gated}) {{
  const gate = path.join(__dirname, "{HANDSHAKE_GATE}");
  (function awaitGate() {{
    if (fs.existsSync(gate)) {{
      announce();
      return;
    }}
    setTimeout(awaitGate, 5);
  }})();
}} else {{
  announce();
}}

let buffered = Buffer.alloc(0);
process.stdin.on("data", (chunk) => {{
  buffered = Buffer.concat([buffered, chunk]);
  for (;;) {{
    const headerEnd = buffered.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const header = buffered.slice(0, headerEnd).toString("utf8");
    const length = /content-length:\s*(\d+)/i.exec(header);
    if (!length) return;
    const bodyStart = headerEnd + 4;
    const bodyEnd = bodyStart + Number(length[1]);
    if (buffered.length < bodyEnd) return;
    const request = JSON.parse(buffered.slice(bodyStart, bodyEnd).toString("utf8"));
    buffered = buffered.slice(bodyEnd);
    if (request.id !== undefined && request.id !== null) {{
      const filePath = (request.params && request.params.filePath) || "";
      fs.appendFileSync(path.join(__dirname, "{REQUEST_LOG}"), request.method + " " + filePath + "\n");
      writeFrame({{ jsonrpc: "2.0", id: request.id, result: {{}} }});
    }}
  }}
}});

// Same exit condition as the real plugin's (plugins/typescript/src/index.ts): the
// core closing its end of stdin is what a deliberate sleep looks like from
// here.
process.stdin.on("end", () => process.exit(0));
"#
    )
}
