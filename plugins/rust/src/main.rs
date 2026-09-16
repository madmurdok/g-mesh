//! `g-mesh-plugin-rust`: the entry point the SDK's [`run`] loop drives.
//! Everything else lives in the library half of this crate (`src/lib.rs`) -
//! see its module doc for why, and `extractor`'s and `project`'s for what
//! this binary presently does and does not do.

use g_mesh_plugin_sdk::{run, PluginSpec};

use g_mesh_plugin_rust::extractor::RustExtractor;

fn main() -> ! {
    run(
        RustExtractor,
        PluginSpec::new("rust", env!("CARGO_PKG_VERSION"), &[".rs"]).exclude_dirs(&["target"]),
        // The rust-analyzer tier (GM-290). A *factory*, not an engine: the SDK
        // calls this on the first `semanticPass` and never before, which is
        // what `capabilities.semantic-engine-lazy` checks and what keeps a
        // structural-only wake-up from loading a compiler. Nothing here runs
        // until then - not the `PATH` lookup, not the `--version` probe, and
        // certainly not rust-analyzer.
        Some(Box::new(g_mesh_plugin_rust::semantic::engine)),
    )
}
