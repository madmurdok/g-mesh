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
            // Never Python source, and since GM-299 the directory a
            // project-local pyright lives in - along with the 5,205 typeshed
            // stubs it bundles. See `project::EXCLUDE_DIRS` for the argument.
            "node_modules",
        ]),
        // The pyright tier (GM-299). A *factory*, not an engine: the SDK calls
        // this on the first `semanticPass` and never before, which is what
        // `capabilities.semantic-engine-lazy` checks and what keeps a
        // structural-only wake-up from starting a type checker. Nothing here
        // runs until then - not the `PATH` lookup, not the `node_modules`
        // lookup, not the `--version` probe, and certainly not pyright.
        Some(Box::new(g_mesh_plugin_python::semantic::engine)),
    )
}
