//! `g-mesh-plugin-typescript`: the entry point the SDK's [`run`] loop drives.
//! Everything else lives in the library half of this crate (`src/lib.rs`).

use g_mesh_plugin_sdk::{run, PluginSpec};

use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;

fn main() -> ! {
    run(
        TypeScriptExtractor,
        PluginSpec::from_manifest_str(include_str!("../plugin.toml")),
        // The vtsls tier. A *factory*, not an engine: the SDK calls
        // it on the first `semanticPass` and never before, so a structural
        // wake-up neither looks for vtsls nor starts it.
        Some(Box::new(g_mesh_plugin_typescript::semantic::engine)),
    )
}
