// Keeps the Go plugin's binary (plugins/go/g-mesh-plugin-go) up to date
// whenever its sources change, so `cargo test`'s end-to-end daemon<->plugin
// tests always exercise a fresh build without a separate manual step. The
// other bundled plugins are cargo-workspace members and need nothing here:
// `cargo build --workspace` builds them.
//
// Best-effort, deliberately - a warning, never a hard `panic!`/
// `process::exit`, even when Go is missing entirely: `cargo check`/`cargo
// build` are run by people editing Rust who have no reason to have Go
// installed. A test that actually spawns the Go plugin fails on its own
// without it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The Go plugin's spawn command in `plugins/go/plugin.toml` is
/// `"./g-mesh-plugin-go"` - no `.exe` on any platform. `go build -o` writes
/// exactly that literal name on every platform (unlike `cargo build`, it does
/// not force a `.exe` suffix onto an explicitly named output), and Windows's
/// `CreateProcess` runs a PE binary by its contents, not its extension, so a
/// checkout's manifest stays spawnable without per-platform edits.
const GO_PLUGIN_BINARY: &str = "g-mesh-plugin-go";

include!("go_plugin_build_flags.rs");

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets this"));

    build_go_plugin(&manifest_dir);
}

/// Builds `plugins/go` into `plugins/go/g-mesh-plugin-go`, the binary
/// `plugins/go/plugin.toml`'s `[plugin.spawn] command = "./g-mesh-plugin-go"`
/// names. A prebuilt binary rather than `go run ./...`, which would recompile
/// the module on every spawn.
fn build_go_plugin(manifest_dir: &Path) {
    let plugin_dir = manifest_dir.join("../plugins/go");

    println!("cargo:rerun-if-changed={}", plugin_dir.join("go.mod").display());
    println!("cargo:rerun-if-changed={}", manifest_dir.join("go_plugin_build_flags.rs").display());
    for entry in std::fs::read_dir(&plugin_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "go") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

    let status = Command::new("go")
        .arg("build")
        .args(GO_PLUGIN_BUILD_FLAGS)
        .args(["-o", GO_PLUGIN_BINARY, "."])
        .current_dir(&plugin_dir)
        .status();
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
