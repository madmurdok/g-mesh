//! `g-mesh-plugin-python`: the entry point the SDK's [`run`] loop drives.
//! Everything else lives in the library half of this crate (`src/lib.rs`) -
//! see its module doc for why, and `extractor`'s and `project`'s for what
//! this binary does and, in the gaps they name, deliberately does not.

use g_mesh_plugin_sdk::{run, PluginSpec};

use g_mesh_plugin_python::extractor::PythonExtractor;

fn main() -> ! {
    run(
        PythonExtractor,
        PluginSpec::new("python", env!("CARGO_PKG_VERSION"), &[".py", ".pyi"]).exclude_dirs(&[
            ".venv",
            "venv",
            "__pycache__",
            ".tox",
            ".mypy_cache",
            "site-packages",
        ]),
        // No semantic tier yet - see `plugin.toml`'s `capabilities.semantic_pass = false`.
        None,
    )
}
