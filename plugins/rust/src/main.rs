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
        // No semantic tier yet - see `plugin.toml`'s `capabilities.semantic_pass = false`.
        None,
    )
}
