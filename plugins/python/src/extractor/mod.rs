//! The Python structural extractor - presently a `File`-only stub. GM-296
//! (tree-sitter-python: declarations, containers, `DEFINES`/`EXPORTS`, the
//! placeholder shapes, open sites) replaces [`PythonExtractor::extract`]'s
//! body in place, the same relationship `plugins/go`'s GM-279 scaffold has
//! to GM-280 and `plugins/rust`'s GM-285 has to GM-286.
//!
//! # Why a stub, and why it is not empty
//!
//! This task (GM-295) is scoped to the project model alone - see
//! `crate::project`'s module doc. But a plugin binary that emits nothing at
//! all is not a plugin the daemon can spawn and index with today: the SDK's
//! `--bulk-index` mode still walks every file this manifest claims and calls
//! [`Extractor::extract`] once per file, and a `FileGraph` with no `File`
//! node is not what `g-mesh plugins check`'s own shape expectations want to
//! see for a file the walk found. So every claimed file gets exactly its own
//! `File` node - `qualified_name` its own path, `name` its last path segment
//! (the same convention every other SDK plugin uses via
//! [`FileGraphBuilder::file_node`]) - and nothing else. No declarations, no
//! edges, no container.
//!
//! # What is deliberately not here yet
//!
//! - **No container is ever set**, not even on the `File` node itself
//!   (which the wire schema *would* allow - `core::graph::containers`' own
//!   "who counts as a member" rule has no special case excluding `File`
//!   nodes). `crate::project`'s module doc (Decisions 1-3) documents exactly
//!   what GM-296 is expected to emit once it exists; wiring a `container`
//!   onto the bare `File` node now, ahead of any real declaration, would be
//!   half of that design landing with no way to test the half that matters
//!   (whether a `from pkg.sub import mod` lookup actually resolves) - GM-296
//!   builds and tests the whole thing together.
//! - **No open sites.** Nothing here has read enough of the file to know one
//!   exists.
//!
//! # Conformance consequence
//!
//! `id-stability.declaration-edit-applies` legitimately **skips** against
//! this stub: it needs a non-`File`, non-placeholder node to edit and
//! re-diff, and this extractor emits none - `plugins/rust`'s own
//! `tests/conformance.rs` documents the identical skip for its pre-GM-286
//! state. `capabilities.semantic-engine-lazy` skips too, because
//! `plugin.toml` declares `semantic_pass = false` (no semantic tier exists
//! yet), which routes the run through `capabilities.semantic-pass-undeclared`
//! instead. Every other check in the kit's fifteen applies and is expected
//! to pass on a `File`-only stream - see `tests/conformance.rs` for the
//! full, asserted list.

use g_mesh_plugin_sdk::wire::{Position, Range};
use g_mesh_plugin_sdk::{Extractor, FileGraph, FileGraphBuilder, RelPath};

use crate::project::ProjectContext;

/// The plugin's wire identifier: the manifest's `language`, this directory's
/// name, and every node's `language`.
const LANGUAGE: &str = "python";

/// The `engine` label on every edge this tier would emit - none yet, since
/// this stub emits no edges, but carried on the builder so GM-296 has
/// nothing to add here when it starts emitting them.
const ENGINE: &str = "file-only-stub";

/// The [`Extractor`] this binary registers with the SDK's `run` loop.
pub struct PythonExtractor;

impl Extractor for PythonExtractor {
    const LANGUAGE: &'static str = LANGUAGE;
    type Project = ProjectContext;

    fn load_project(&self, root: &std::path::Path) -> anyhow::Result<ProjectContext> {
        ProjectContext::load(root)
    }

    /// One file, presently answered with only its own `File` node - see this
    /// module's doc for why, and what GM-296 replaces this body with.
    ///
    /// `project` is read by neither this stub nor, transitively, by
    /// anything it calls - it exists on the signature because
    /// [`Extractor::extract`] always receives it, and GM-296's own body is
    /// exactly where `project.container_for(path)` (the seam
    /// `crate::project`'s module doc names) is expected to be called, once
    /// per file, the same way `RustExtractor::extract` calls its own
    /// project model's equivalent.
    fn extract(&self, _project: &ProjectContext, path: &RelPath, source: &str) -> FileGraph {
        let mut graph = FileGraphBuilder::new(LANGUAGE, ENGINE, path);
        graph.file_node(whole_file_range(source));
        graph.finish()
    }
}

/// The range spanning all of `source`: `(0, 0)` to `(number of newlines,
/// length of the final unterminated line)`.
///
/// Not `(lines, 0)` and not a byte count - either moves when a trailing
/// space is inserted before the file's last newline, which is exactly what
/// `id-stability.whitespace-edit` checks. `plugins/sdk/toy/main.rs`'s own
/// `file_node` call site computes the same shape for the identical reason;
/// this is not a coincidence to keep in sync by convention, it is
/// [`FileGraphBuilder::file_node`]'s own documented contract.
fn whole_file_range(source: &str) -> Range {
    let lines: Vec<&str> = source.split('\n').collect();
    let end = Position {
        line: (lines.len() - 1) as u32,
        col: lines.last().map_or(0, |line| line.chars().count()) as u32,
    };
    Range { start: Position { line: 0, col: 0 }, end }
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_plugin_sdk::wire::NodeKind;

    fn extract(source: &str) -> FileGraph {
        PythonExtractor.extract(&ProjectContext::default(), &RelPath::new("pkg/mod.py"), source)
    }

    #[test]
    fn a_file_produces_exactly_its_own_file_node() {
        let graph = extract("def f():\n    pass\n");
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].kind, NodeKind::File);
        assert_eq!(graph.nodes[0].qualified_name, "pkg/mod.py");
        assert_eq!(graph.nodes[0].name, "mod.py");
        assert!(graph.edges.is_empty());
        assert!(graph.open_sites.is_empty());
        assert!(graph.nodes[0].container.is_none(), "see this module's doc: no container yet");
    }

    #[test]
    fn a_trailing_space_before_the_last_newline_does_not_move_the_files_range() {
        let plain = extract("def f():\n    pass\n");
        let spaced = extract("def f():\n    pass \n");
        assert_eq!(plain, spaced);
    }

    #[test]
    fn extraction_is_a_pure_function_of_the_source() {
        let source = "import os\n\nclass C:\n    def m(self):\n        return os.getcwd()\n";
        assert_eq!(extract(source), extract(source));
    }

    #[test]
    fn an_empty_file_still_produces_a_file_node_with_a_zero_length_range() {
        let graph = extract("");
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.nodes[0].range.start, Position { line: 0, col: 0 });
        assert_eq!(graph.nodes[0].range.end, Position { line: 0, col: 0 });
    }
}
