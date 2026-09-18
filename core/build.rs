// Keeps the bundled plugins' compiled output (plugins/typescript/dist/,
// plugins/go/g-mesh-plugin-go) up to date whenever their sources change, so
// `cargo test`'s end-to-end daemon<->plugin tests always exercise a fresh
// build without a separate manual build step. `daemon::plugin` resolves
// each plugin's entry point relative to this same crate's manifest
// directory, so the two stay in sync by construction.
//
// Best-effort, for both plugins, deliberately - a warning, never a hard
// `panic!`/`process::exit`, even when the build step is missing entirely
// (no npm, no Go). The alternative - failing the build itself - was
// considered and rejected: `cargo check`/`cargo build` are run constantly by
// someone editing Rust who has never touched a plugin and has no reason to
// have Node installed, and a workflow that never exercises the
// daemon<->plugin path should not gain a new, unrelated toolchain
// requirement just because this crate happens to also own two plugin
// builds. A test that actually spawns a plugin genuinely cannot work
// without it, so that failure is real; a plain `cargo check` failing for
// the same reason would not be.
//
// What was not acceptable, and is what this file used to do, is leaving
// that later test failure to explain itself. "The plugin-spawning tests
// will fail with a clear error" was the original promise here, and it was
// false in practice: `daemon::plugin`'s handshake read reports "plugin
// closed its stdout before sending a handshake" when node can't find
// dist/src/index.js, which names the symptom and nothing about npm, node,
// or a build step at all. GM-291, GM-296, GM-299 and this release's own
// verification pass each lost time to that message before finding this
// file. Two things now close that gap instead of one: this build step's own
// warning below names the missing `node_modules` and the exact fix
// (`npm ci && npm run build` in plugins/typescript) whenever it can tell
// that's the cause rather than a real `tsc` failure; and
// `daemon::plugin::PluginState::spawn` checks the entry file exists before
// ever spawning `node`, so the same missing build now fails at test time
// with that same command named, not with a bare "closed its stdout".

use std::path::{Path, PathBuf};
use std::process::Command;

/// npm ships as a `.cmd` shim on Windows, and `CreateProcess` - which is what
/// `Command` calls - only resolves real executables through `PATH`, never
/// `PATHEXT`. Spawning bare "npm" there fails with "program not found" even
/// on a machine with a perfectly good Node install, which is what the first
/// Windows pipeline run reported. The other three platforms have a plain
/// `npm` executable and want the bare name.
#[cfg(windows)]
const NPM: &str = "npm.cmd";
#[cfg(not(windows))]
const NPM: &str = "npm";

/// The Go plugin's spawn command in `plugins/go/plugin.toml` is
/// `"./g-mesh-plugin-go"` - no `.exe` on any platform, since that manifest
/// (unlike this build step) has no `#[cfg(windows)]` of its own to give it
/// one. Building under that exact name here is what keeps a checkout's
/// manifest spawnable without editing it per platform - Windows support for
/// this plugin is left to GM-283 (distribution), which already owns
/// per-target binary naming for the release matrix; this build step's own
/// job is only "keep a checkout building `plugins check`/`plugins list`",
/// which today it does correctly on macOS/Linux and, on Windows, produces a
/// binary `plugin.toml` cannot spawn (a `cargo build`/`cargo test` there
/// still succeeds - only the plugin-spawning tests would fail, the same
/// "diagnosis enough" contract this file already accepts for a missing Go
/// toolchain).
const GO_PLUGIN_BINARY: &str = "g-mesh-plugin-go";

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets this"));

    build_ts_plugin(&manifest_dir);
    build_go_plugin(&manifest_dir);
}

fn build_ts_plugin(manifest_dir: &Path) {
    let plugin_dir = manifest_dir.join("../plugins/typescript");

    println!("cargo:rerun-if-changed={}", plugin_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", plugin_dir.join("package.json").display());
    println!("cargo:rerun-if-changed={}", plugin_dir.join("tsconfig.json").display());

    match Command::new(NPM).arg("run").arg("build").current_dir(&plugin_dir).status() {
        Ok(status) if status.success() => {}
        // `npm run build` invokes `tsc` out of `node_modules/.bin`, which a
        // fresh clone does not have until `npm ci` has populated it - that
        // failure (exit status 127, "tsc: not found") is indistinguishable
        // from a real break in the warning below unless `node_modules`
        // itself is checked directly, which is why that check, not the exit
        // status, decides which message prints. Naming the cause and the
        // one-line fix here is the whole point of this diagnostic: four
        // separate pieces of work (GM-291, GM-296, GM-299, this release's
        // own verification) instead hit this as five `core` tests failing
        // with "failed to spawn the JS/TS plugin: plugin closed its stdout
        // before sending a handshake" - a symptom with no mention of npm at
        // all - and each first suspected its own change before tracing it
        // back here.
        Ok(status) if !plugin_dir.join("node_modules").is_dir() => println!(
            "cargo:warning=plugins/typescript/node_modules is missing, so `npm run build` in {} exited with \
             {status} instead of building - the JS/TS plugin's dist/ was never produced. Run `npm ci && npm run \
             build` in plugins/typescript; skipping this leaves dist/ missing and core's plugin-spawning tests \
             will fail later with \"failed to spawn ... plugin closed its stdout before sending a handshake\", \
             which does not name this as the cause",
            plugin_dir.display()
        ),
        Ok(status) => println!(
            "cargo:warning=`{NPM} run build` in {} exited with {status} - the JS/TS plugin's dist/ may be stale. \
             `node_modules` is present, so this looks like a real build failure rather than a missing `npm ci`; \
             run `npm run build` there directly to see tsc's own output",
            plugin_dir.display()
        ),
        Err(err) => println!(
            "cargo:warning=failed to run `{NPM} run build` in {}: {err} - the JS/TS plugin's dist/ may be stale or \
             missing (is npm on PATH? try `npm ci && npm run build` in plugins/typescript)",
            plugin_dir.display()
        ),
    }
}

/// Builds `plugins/go` into `plugins/go/g-mesh-plugin-go`, the binary
/// `plugins/go/plugin.toml`'s `[plugin.spawn] command = "./g-mesh-plugin-go"`
/// names - see GM-279's own report (and this file's `GO_PLUGIN_BINARY` doc
/// comment) for why a prebuilt binary was chosen over `go run ./...`
/// (recompiling the whole module on every single spawn is not a cost worth
/// paying on the hot path of a real daemon's per-file requests) and why
/// *this* is where that binary gets built rather than left to a manual
/// step: it is exactly the automation `build_ts_plugin` above already
/// provides for the other bundled plugin, and without it `cargo test`,
/// `g-mesh plugins check` and `g-mesh plugins list` would need a
/// contributor to remember an out-of-band `go build` before any of them
/// could see a working Go plugin on a fresh checkout.
fn build_go_plugin(manifest_dir: &Path) {
    let plugin_dir = manifest_dir.join("../plugins/go");

    println!("cargo:rerun-if-changed={}", plugin_dir.join("go.mod").display());
    for entry in std::fs::read_dir(&plugin_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "go") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let status =
        Command::new("go").args(["build", "-o", GO_PLUGIN_BINARY, "."]).current_dir(&plugin_dir).status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => println!(
            "cargo:warning=`go build` in {} exited with {status} - the Go plugin's binary may be stale",
            plugin_dir.display()
        ),
        Err(err) => println!(
            "cargo:warning=failed to run `go build` in {}: {err} - the Go plugin's binary may be stale or missing \
             (is a Go toolchain on PATH?)",
            plugin_dir.display()
        ),
    }
}
