// Keeps the bundled JS/TS plugin's compiled output (plugins/typescript/dist/)
// up to date whenever its sources change, so `cargo test`'s end-to-end
// daemon<->plugin test always exercises a fresh build without a separate
// manual build step. `daemon::plugin` resolves the plugin's entry point
// relative to this same crate's manifest directory, so the two stay in
// sync by construction.
//
// Best-effort: a workflow that never touches the daemon<->plugin path (e.g.
// `cargo check` on a machine with no Node toolchain) should not be blocked
// on npm being installed - the plugin-spawning tests will fail with a clear
// "failed to spawn" error instead, which is diagnosis enough.

use std::path::PathBuf;
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

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets this"));
    let plugin_dir = manifest_dir.join("../plugins/typescript");

    println!("cargo:rerun-if-changed={}", plugin_dir.join("src").display());
    println!("cargo:rerun-if-changed={}", plugin_dir.join("package.json").display());
    println!("cargo:rerun-if-changed={}", plugin_dir.join("tsconfig.json").display());

    match Command::new(NPM).arg("run").arg("build").current_dir(&plugin_dir).status() {
        Ok(status) if status.success() => {}
        Ok(status) => println!(
            "cargo:warning=`{NPM} run build` in {} exited with {status} - the JS/TS plugin's dist/ may be stale",
            plugin_dir.display()
        ),
        Err(err) => println!(
            "cargo:warning=failed to run `{NPM} run build` in {}: {err} - the JS/TS plugin's dist/ may be stale or missing",
            plugin_dir.display()
        ),
    }
}
