//! `g-mesh-plugin-typescript`: the entry point the SDK's [`run`] loop drives.
//! Everything else lives in the library half of this crate (`src/lib.rs`).

use g_mesh_plugin_sdk::{run, PluginSpec};

use g_mesh_plugin_typescript::extractor::grammar::EXTENSIONS;
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project;

fn main() -> ! {
    run(
        TypeScriptExtractor,
        PluginSpec::new("typescript", env!("CARGO_PKG_VERSION"), &EXTENSIONS)
            .exclude_dirs(&project::EXCLUDE_DIRS),
        None,
    )
}
