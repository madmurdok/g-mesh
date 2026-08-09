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
//! request) - and nothing more. It answers every request with an *empty*
//! diff, which is the point: these tests are about which process gets asked,
//! not about what a parser makes of a file.
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

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

use crate::protocol::types::CURRENT_PROTOCOL_VERSION;
use crate::storage::schema;

/// The file each fake plugin process appends its pid to on startup.
const SPAWN_LOG: &str = "spawns.log";

/// Writes a discoverable plugin directory named `language` under `root`,
/// claiming `extensions`, and returns the directory it created.
///
/// Laid out exactly as a real one is (`<root>/<language>/plugin.toml`), so
/// `daemon::manifest::discover(&[root])` picks it up with no test-only path
/// through discovery.
pub(crate) fn install(root: &Path, language: &str, extensions: &[&str]) -> PathBuf {
    let dir = root.join(language);
    fs::create_dir_all(&dir).expect("failed to create the fake plugin's directory");
    fs::write(dir.join("plugin.js"), entry_point(language))
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

/// The fake plugin itself: record the spawn, handshake, then answer every
/// framed request that carries an id with an empty diff under that same id.
///
/// Deliberately minimal about framing - it re-implements just enough of
/// `protocol::jsonrpc` to be a peer, and would rather hang than guess if core
/// ever sent something it does not understand, since a test that hangs is
/// easier to diagnose than one that silently agrees with a bug.
fn entry_point(language: &str) -> String {
    format!(
        r#"// Generated by core/src/daemon/test_plugin.rs - not a real plugin.
const fs = require("fs");
const path = require("path");

fs.appendFileSync(path.join(__dirname, "{SPAWN_LOG}"), process.pid + "\n");

function writeFrame(message) {{
  const body = Buffer.from(JSON.stringify(message), "utf8");
  process.stdout.write("Content-Length: " + body.length + "\r\n\r\n");
  process.stdout.write(body);
}}

writeFrame({{
  protocolVersion: {CURRENT_PROTOCOL_VERSION},
  language: "{language}",
  pluginVersion: "0.0.0-test",
}});

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
      writeFrame({{ jsonrpc: "2.0", id: request.id, result: {{}} }});
    }}
  }}
}});

// Same exit condition as the real plugin's (plugins/js-ts/src/index.ts): the
// core closing its end of stdin is what a deliberate sleep looks like from
// here.
process.stdin.on("end", () => process.exit(0));
"#
    )
}
