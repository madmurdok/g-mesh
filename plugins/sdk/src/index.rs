//! What the plugin remembers: the last extraction of every file it has seen,
//! the text it was extracted from, and the open sites the structural pass
//! left behind.
//!
//! # Two jobs, one store
//!
//! The per-file cache the incremental diff needs ("what did I last tell core
//! about this file") and the store a semantic engine reads ("what did the
//! structural pass leave unanswered, and which node is at this position")
//! are the same data asked two different questions, so they are one store.
//! Keeping them apart would mean two things to invalidate on every edit, and
//! the failure of forgetting one is silent: a semantic pass answering about a
//! file as it was two edits ago writes edges from node ids that are no longer
//! in the index.
//!
//! # Process lifetime only
//!
//! Nothing here is persisted. A restarted plugin has an empty index, and the
//! first `fileChanged` for each file reports its whole extraction as
//! additions - the diff against nothing is everything, which is correct and
//! costs one file's extraction. The cold path is a full `--bulk-index`, so
//! there is nothing to resume.
//!
//! # What it is not
//!
//! Not core's index. It holds this plugin's own files only, it is never
//! queried across files by name, and nothing in it is authoritative: core's
//! SQLite index is. It exists because a semantic tier has to correlate an
//! answer (a position, a location) back to a node id the structural tier
//! already sent, and that mapping lives nowhere else.

use std::collections::BTreeMap;

use g_mesh_wire::{Position, WireNode};

use crate::graph::{FileGraph, OpenSite};
use crate::path::RelPath;

/// One file as the plugin last saw it.
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// The text the graph was extracted from. Kept because a semantic engine
    /// that drives a language server has to send the server the same bytes
    /// the structural pass read (`textDocument/didOpen`), and reading the
    /// file again could pick up an edit that has not been extracted yet.
    pub source: String,
    /// The last extraction - the diff baseline, and the node set positions
    /// are resolved against.
    pub graph: FileGraph,
}

/// The plugin's own view of the project: every file it has extracted.
#[derive(Debug, Clone, Default)]
pub struct SdkIndex {
    files: BTreeMap<RelPath, FileEntry>,
}

impl SdkIndex {
    /// An empty index - a freshly started plugin.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an extraction, replacing whatever was there.
    pub fn insert(&mut self, path: RelPath, source: String, graph: FileGraph) {
        self.files.insert(path, FileEntry { source, graph });
    }

    /// Forgets a file - it was deleted, or its extraction panicked and the
    /// last good baseline is no longer known to describe it.
    pub fn remove(&mut self, path: &RelPath) -> Option<FileEntry> {
        self.files.remove(path)
    }

    /// Forgets everything. The right response to `workspaceChanged`: the
    /// project model that every extraction was made against is gone, so every
    /// extraction made against it is suspect.
    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// What the plugin last saw of `path`.
    pub fn entry(&self, path: &RelPath) -> Option<&FileEntry> {
        self.files.get(path)
    }

    /// The last extraction of `path` - the diff baseline.
    pub fn graph(&self, path: &RelPath) -> Option<&FileGraph> {
        self.files.get(path).map(|entry| &entry.graph)
    }

    /// The text `path`'s graph was extracted from.
    pub fn source(&self, path: &RelPath) -> Option<&str> {
        self.files.get(path).map(|entry| entry.source.as_str())
    }

    /// Every file, in path order.
    pub fn files(&self) -> impl Iterator<Item = (&RelPath, &FileEntry)> {
        self.files.iter()
    }

    /// Every file's path, in order. The list a whole-project semantic pass
    /// works through when core sends it an empty `filePaths`.
    pub fn paths(&self) -> Vec<RelPath> {
        self.files.keys().cloned().collect()
    }

    /// How many files are held.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether nothing has been extracted yet.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The open sites of `path`, or an empty slice for a file not held.
    pub fn open_sites(&self, path: &RelPath) -> &[OpenSite] {
        self.files.get(path).map(|entry| entry.graph.open_sites.as_slice()).unwrap_or(&[])
    }

    /// The node a semantic engine's answer landed on: the **smallest** node
    /// of `path` whose range contains `position`.
    ///
    /// Smallest, not first: a definition location points at a symbol's own
    /// name, which is inside the symbol, which is inside the file's `File`
    /// node. Every one of those contains the position, and only the innermost
    /// is the answer. Ties - two nodes with exactly the same range, an
    /// overload signature beside its implementation - are broken by id, so
    /// the answer is at least stable rather than whichever the iteration
    /// order produced.
    pub fn node_at(&self, path: &RelPath, position: Position) -> Option<&WireNode> {
        let entry = self.files.get(path)?;
        entry
            .graph
            .nodes
            .iter()
            .filter(|node| contains(node, position))
            .min_by(|a, b| span(a).cmp(&span(b)).then_with(|| a.id.cmp(&b.id)))
    }

    /// The node with this id, and the file it is in - what an engine uses to
    /// turn an id it was given back into something it can ask about.
    pub fn node(&self, id: &str) -> Option<(&RelPath, &WireNode)> {
        self.files.iter().find_map(|(path, entry)| {
            entry.graph.nodes.iter().find(|node| node.id == id).map(|node| (path, node))
        })
    }
}

/// Whether `position` is inside `node`'s range, treating the range as
/// half-open at its end in columns and closed in lines - the same convention
/// the ranges themselves use.
fn contains(node: &WireNode, position: Position) -> bool {
    let start = (node.range.start.line, node.range.start.col);
    let end = (node.range.end.line, node.range.end.col);
    let at = (position.line, position.col);
    start <= at && at <= end
}

/// A node's range as a comparable size, for picking the innermost one.
fn span(node: &WireNode) -> (u32, u32) {
    let lines = node.range.end.line.saturating_sub(node.range.start.line);
    let cols =
        if lines == 0 { node.range.end.col.saturating_sub(node.range.start.col) } else { node.range.end.col };
    (lines, cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{FileGraphBuilder, NodeSpec};
    use g_mesh_wire::{NodeKind, Range};

    fn at(line: u32, col: u32) -> Position {
        Position { line, col }
    }

    fn range(start: Position, end: Position) -> Range {
        Range { start, end }
    }

    fn index() -> (SdkIndex, RelPath) {
        let path = RelPath::new("a.toy");
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &path);
        builder.file_node(range(at(0, 0), at(9, 0)));
        builder.add_node(NodeSpec::new(NodeKind::Function, "outer", "outer", range(at(1, 0), at(5, 1))));
        builder.add_node(NodeSpec::new(NodeKind::Variable, "inner", "inner", range(at(2, 4), at(2, 9))));
        let mut index = SdkIndex::new();
        index.insert(path.clone(), "source".to_string(), builder.finish());
        (index, path)
    }

    #[test]
    fn node_at_picks_the_innermost_containing_node() {
        let (index, path) = index();
        assert_eq!(index.node_at(&path, at(2, 5)).map(|n| n.name.as_str()), Some("inner"));
        assert_eq!(index.node_at(&path, at(4, 0)).map(|n| n.name.as_str()), Some("outer"));
        assert_eq!(index.node_at(&path, at(8, 0)).map(|n| n.name.as_str()), Some("a.toy"));
    }

    #[test]
    fn node_at_a_position_in_no_node_and_in_no_known_file_is_none() {
        let (index, path) = index();
        assert!(index.node_at(&path, at(99, 0)).is_none());
        assert!(index.node_at(&RelPath::new("nowhere.toy"), at(0, 0)).is_none());
    }

    #[test]
    fn clearing_forgets_every_file() {
        let (mut index, path) = index();
        assert_eq!(index.len(), 1);
        assert!(index.graph(&path).is_some());
        index.clear();
        assert!(index.is_empty());
        assert!(index.graph(&path).is_none());
        assert!(index.open_sites(&path).is_empty());
    }

    #[test]
    fn a_node_can_be_found_by_id_together_with_its_file() {
        let (index, path) = index();
        let id = index.graph(&path).unwrap().nodes[2].id.clone();
        let (found_in, node) = index.node(&id).expect("the node is in the index");
        assert_eq!(found_in, &path);
        assert_eq!(node.name, "inner");
        assert!(index.node("no-such-id").is_none());
    }
}
