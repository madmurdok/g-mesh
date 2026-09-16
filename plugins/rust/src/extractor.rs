//! The `Extractor` this binary registers with the SDK's [`run`](g_mesh_plugin_sdk::run) loop.
//!
//! # Why this is a stub
//!
//! GM-285's scope is the project model - [`ProjectContext`](crate::project::ProjectContext) -
//! not the extractor: turning one file's bytes into declarations and edges is
//! tree-sitter-rust's job, and tree-sitter-rust is GM-286's dependency to
//! add, not this task's. What a plugin needs to *build, run, and pass the
//! structural half of the conformance kit* is an `Extractor` at all, so this
//! one emits exactly the file's own `File` node - nothing a real extractor
//! will not also emit as the very first line of its own output
//! ([`FileGraphBuilder::file_node`] is `extract`'s mandatory first call) -
//! and nothing more.
//!
//! GM-286 replaces [`RustExtractor::extract`]'s body; it does not need to
//! replace [`RustExtractor::load_project`] or touch [`crate::project`] at
//! all. It reads a file's own container key back from the
//! [`ProjectContext`] handed to `extract` (see that module's doc for the
//! exact call) and sets `container`/`container_parent` on each declaration
//! it builds - never on the `File` node itself, which is not a container
//! member (see `project`'s module doc, "What the File node does not carry").

use g_mesh_plugin_sdk::{Extractor, FileGraph, FileGraphBuilder, RelPath};

use crate::project::ProjectContext;

pub struct RustExtractor;

impl Extractor for RustExtractor {
    const LANGUAGE: &'static str = "rust";
    type Project = ProjectContext;

    fn load_project(&self, root: &std::path::Path) -> anyhow::Result<ProjectContext> {
        ProjectContext::load(root)
    }

    /// One file's `File` node, and nothing else. See this module's doc for
    /// why: GM-286 fills this in with tree-sitter-rust.
    fn extract(&self, _project: &ProjectContext, path: &RelPath, source: &str) -> FileGraph {
        let mut graph = FileGraphBuilder::new(Self::LANGUAGE, "rust-stub", path);
        graph.file_node(g_mesh_plugin_sdk::wire::Range {
            start: g_mesh_plugin_sdk::wire::Position { line: 0, col: 0 },
            end: file_end(source),
        });
        graph.finish()
    }
}

/// The file's end position, in the SDK's half-open-at-the-end convention:
/// `(number of newlines, length of the final unterminated line)`. Lifted
/// verbatim from the toy plugin's own comment on why this matters -
/// `id-stability.whitespace-edit` fails for any other formula, because an end
/// of `(lines, 0)` or a byte count moves when a trailing space is inserted
/// before the last newline and this does not.
fn file_end(source: &str) -> g_mesh_plugin_sdk::wire::Position {
    let lines: Vec<&str> = source.split('\n').collect();
    g_mesh_plugin_sdk::wire::Position {
        line: (lines.len() - 1) as u32,
        col: lines.last().map_or(0, |line| line.chars().count()) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_emits_only_the_files_own_file_node() {
        let project = ProjectContext::default();
        let graph = RustExtractor.extract(&project, &RelPath::new("src/lib.rs"), "fn main() {}\n");
        assert_eq!(graph.nodes.len(), 1, "{:?}", graph.nodes);
        assert_eq!(graph.nodes[0].kind, g_mesh_plugin_sdk::wire::NodeKind::File);
        assert_eq!(graph.nodes[0].qualified_name, "src/lib.rs");
        assert!(graph.edges.is_empty());
        assert!(graph.open_sites.is_empty());
    }

    #[test]
    fn a_trailing_space_before_the_last_newline_does_not_move_the_files_range() {
        let project = ProjectContext::default();
        let plain = RustExtractor.extract(&project, &RelPath::new("a.rs"), "fn a() {}\n");
        let spaced = RustExtractor.extract(&project, &RelPath::new("a.rs"), "fn a() {} \n");
        assert_eq!(plain, spaced);
    }
}
