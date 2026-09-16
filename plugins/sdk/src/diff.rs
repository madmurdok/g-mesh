//! The incremental diff: what a `fileChanged` answer is.
//!
//! # The contract, copied deliberately from `plugins/typescript`
//!
//! `plugins/typescript/src/incremental.ts` is the behavioural reference, and
//! this is the same algorithm with the same three deliberate choices:
//!
//! 1. **Keyed by id, refined by content.** An id present on both sides whose
//!    node differs in any field is reported as *both* a delete of that id and
//!    an upsert of the new node. An id present on one side only is reported
//!    as that side's delete or upsert. An id present on both with identical
//!    content is not reported at all.
//! 2. **Ranges count as content.** Ids are derived from identity and never
//!    from position (`ids`' module doc), so a function whose body grew keeps
//!    its id - which is the whole point - and the *only* thing that tells the
//!    index its range moved is a content comparison that includes the range.
//!    An id-only diff would silently leave every symbol below an insertion
//!    sitting at a stale range, which is precisely what
//!    `id-stability.declaration-edit-applies` was added to catch.
//! 3. **A whitespace-only edit produces nothing.** Not as a special case -
//!    there is no whitespace check anywhere in this file. It follows from (2)
//!    plus a pure extractor: if no node's fields changed, no node is
//!    reported. That is why the rule is stated as a property of the
//!    *extractor* in [`Extractor`](crate::Extractor)'s own contract.
//!
//! # Why delete-plus-upsert of the same id is correct, and what it cost
//!
//! Sending an id in `deleteNodeIds` and in `upsertNodes` of the same diff
//! reads like a mistake and is not: `storage::write::apply_diff` performs
//! every delete before every upsert, inside one transaction, so the row ends
//! up present and current. It is also the only shape that keeps the node's
//! child rows (declarations, placeholder target, embedding vector) from
//! outliving a symbol whose shape changed - the delete clears them and the
//! upsert plus `EmbeddingPipeline::apply` rebuilds them.
//!
//! This is documented rather than assumed because it *did* fail once.
//! GM-292: core's connection was believed to have foreign-key enforcement
//! off and in fact had it on, so the delete half was refused for any node
//! with an edge pointing into it - which is every symbol that is used
//! anywhere - and every warm edit was silently dropped for a release.
//! GM-293/GM-294 fixed the enforcement and added the conformance check that
//! would have caught it. **Do not work around that history here.** An SDK
//! that upserted changed nodes in place without deleting would pass today's
//! checks and leave stale child rows behind; the contract is the one above,
//! and the place it is proved is `g-mesh plugins check`, not this crate.

use std::collections::HashMap;

use g_mesh_wire::FileChangeDiff;

use crate::graph::FileGraph;

/// The diff from `previous` to `next` for one file.
///
/// `previous` is `None` for a file this process has never extracted - a cold
/// control-plane process, or a file only the bulk walk ever saw. The diff
/// against nothing is everything, which is both the honest answer and the one
/// that leaves core's graph correct with nothing seeded first.
///
/// A deleted file is `next = &FileGraph::default()`: every id the plugin had
/// for it is deleted and nothing is upserted.
pub fn diff_file(previous: Option<&FileGraph>, next: &FileGraph) -> FileChangeDiff {
    let empty = FileGraph::default();
    let previous = previous.unwrap_or(&empty);

    let previous_nodes: HashMap<&str, &_> =
        previous.nodes.iter().map(|node| (node.id.as_str(), node)).collect();
    let next_nodes: HashMap<&str, &_> = next.nodes.iter().map(|node| (node.id.as_str(), node)).collect();
    let previous_edges: HashMap<&str, &_> =
        previous.edges.iter().map(|edge| (edge.id.as_str(), edge)).collect();
    let next_edges: HashMap<&str, &_> = next.edges.iter().map(|edge| (edge.id.as_str(), edge)).collect();

    FileChangeDiff {
        // Order follows the input vectors on both sides, so a deterministic
        // extractor produces a deterministic diff - which is what makes two
        // runs of the kit comparable at all.
        delete_node_ids: previous
            .nodes
            .iter()
            .filter(|node| next_nodes.get(node.id.as_str()).is_none_or(|current| *current != *node))
            .map(|node| node.id.clone())
            .collect(),
        upsert_nodes: next
            .nodes
            .iter()
            .filter(|node| previous_nodes.get(node.id.as_str()).is_none_or(|before| *before != *node))
            .cloned()
            .collect(),
        delete_edge_ids: previous
            .edges
            .iter()
            .filter(|edge| next_edges.get(edge.id.as_str()).is_none_or(|current| *current != *edge))
            .map(|edge| edge.id.clone())
            .collect(),
        upsert_edges: next
            .edges
            .iter()
            .filter(|edge| previous_edges.get(edge.id.as_str()).is_none_or(|before| *before != *edge))
            .cloned()
            .collect(),
    }
}

/// Whether a diff says nothing at all - the answer a reparse of an unchanged
/// file must produce.
pub fn is_empty_diff(diff: &FileChangeDiff) -> bool {
    diff.upsert_nodes.is_empty()
        && diff.delete_node_ids.is_empty()
        && diff.upsert_edges.is_empty()
        && diff.delete_edge_ids.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{EdgeSpec, FileGraphBuilder, NodeSpec};
    use crate::path::RelPath;
    use g_mesh_wire::{EdgeKind, NodeKind, Position, Range, SourceTier};

    fn range(line: u32, end_col: u32) -> Range {
        Range { start: Position { line, col: 0 }, end: Position { line, col: end_col } }
    }

    /// A file with `names` as its functions, each one line long, the nth at
    /// line n, ending at `end_col` - so a test can move or resize exactly one
    /// symbol.
    fn graph(names: &[(&str, u32, u32)]) -> FileGraph {
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let file = builder.file_node(range(0, 0));
        for (name, line, end_col) in names {
            let id = builder
                .add_node(NodeSpec::new(NodeKind::Function, *name, *name, range(*line, *end_col)).public());
            builder.defines(&file, &id, true);
        }
        builder.finish()
    }

    #[test]
    fn a_reparse_of_an_unchanged_file_says_nothing() {
        let before = graph(&[("a", 1, 4), ("b", 2, 4)]);
        let after = graph(&[("a", 1, 4), ("b", 2, 4)]);
        let diff = diff_file(Some(&before), &after);
        assert!(is_empty_diff(&diff), "{diff:?}");
    }

    #[test]
    fn a_first_sighting_reports_everything_as_an_upsert() {
        let after = graph(&[("a", 1, 4)]);
        let diff = diff_file(None, &after);
        assert_eq!(diff.upsert_nodes.len(), after.nodes.len());
        assert_eq!(diff.upsert_edges.len(), after.edges.len());
        assert!(diff.delete_node_ids.is_empty());
        assert!(diff.delete_edge_ids.is_empty());
    }

    #[test]
    fn an_added_symbol_is_upserted_with_its_edges_and_nothing_is_deleted() {
        let before = graph(&[("a", 1, 4)]);
        let after = graph(&[("a", 1, 4), ("b", 2, 4)]);
        let diff = diff_file(Some(&before), &after);

        let b = crate::ids::node_id("a.toy", NodeKind::Function, "b", None);
        assert_eq!(diff.upsert_nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>(), vec![b.clone()]);
        assert_eq!(diff.upsert_edges.len(), 2, "its DEFINES and EXPORTS: {diff:?}");
        assert!(diff.upsert_edges.iter().all(|edge| edge.to_id == b));
        assert!(diff.delete_node_ids.is_empty(), "{diff:?}");
        assert!(diff.delete_edge_ids.is_empty(), "{diff:?}");
    }

    #[test]
    fn a_removed_symbol_is_deleted_with_its_edges_and_nothing_is_upserted() {
        let before = graph(&[("a", 1, 4), ("b", 2, 4)]);
        let after = graph(&[("a", 1, 4)]);
        let diff = diff_file(Some(&before), &after);

        let b = crate::ids::node_id("a.toy", NodeKind::Function, "b", None);
        assert_eq!(diff.delete_node_ids, vec![b]);
        assert_eq!(diff.delete_edge_ids.len(), 2);
        assert!(diff.upsert_nodes.is_empty(), "{diff:?}");
        assert!(diff.upsert_edges.is_empty(), "{diff:?}");
    }

    /// The shape every real user edit has, and the one GM-292 refused: the
    /// symbol is still there, at a different range. It must be reported as
    /// both a delete and an upsert of the same id - see this module's doc.
    #[test]
    fn a_symbol_that_moved_is_deleted_and_re_upserted_under_the_same_id() {
        let before = graph(&[("a", 1, 4)]);
        let after = graph(&[("a", 5, 4)]);
        let diff = diff_file(Some(&before), &after);

        let a = crate::ids::node_id("a.toy", NodeKind::Function, "a", None);
        assert_eq!(diff.delete_node_ids, vec![a.clone()]);
        assert_eq!(diff.upsert_nodes.len(), 1);
        assert_eq!(diff.upsert_nodes[0].id, a);
        assert_eq!(diff.upsert_nodes[0].range.start.line, 5);
        // Its edges did not change, so they are not re-sent - and must not be
        // deleted either, or the symbol would come back unreachable.
        assert!(diff.delete_edge_ids.is_empty(), "{diff:?}");
        assert!(diff.upsert_edges.is_empty(), "{diff:?}");
    }

    /// A symbol whose *content* changed with no change to its range: the diff
    /// has to notice, or the index keeps describing code that is gone.
    #[test]
    fn a_symbol_whose_signature_changed_is_reported_although_its_range_did_not() {
        let mut before = graph(&[("a", 1, 4)]);
        let after = graph(&[("a", 1, 4)]);
        before.nodes[1].signature = Some("fn a(old)".to_string());

        let diff = diff_file(Some(&before), &after);
        assert_eq!(diff.delete_node_ids.len(), 1);
        assert_eq!(diff.upsert_nodes.len(), 1);
        assert_eq!(diff.upsert_nodes[0].signature, None);
    }

    #[test]
    fn an_edge_whose_resolution_changed_is_deleted_and_re_upserted() {
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let file = builder.file_node(range(0, 0));
        let a = builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 4)).public());
        builder.placeholder_edge(EdgeKind::Calls, &file, &a);
        let before = builder.finish();

        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let file = builder.file_node(range(0, 0));
        let a = builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 4)).public());
        builder.resolved_edge(EdgeKind::Calls, &file, &a);
        let after = builder.finish();

        let diff = diff_file(Some(&before), &after);
        assert_eq!(diff.delete_edge_ids, vec![before.edges[0].id.clone()]);
        assert_eq!(diff.upsert_edges.len(), 1);
        assert_eq!(diff.upsert_edges[0].id, before.edges[0].id, "resolution is not part of an edge's id");
        assert!(diff.upsert_edges[0].resolved);
    }

    /// An edge whose *endpoints* changed is a different edge, since endpoints
    /// are its identity: one id goes, another arrives.
    #[test]
    fn an_edge_that_moved_to_another_target_is_a_different_edge() {
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let file = builder.file_node(range(0, 0));
        let a = builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 4)).public());
        let b = builder.add_node(NodeSpec::new(NodeKind::Function, "b", "b", range(2, 4)).public());
        let old = builder.resolved_edge(EdgeKind::Calls, &file, &a);
        let before = builder.finish();

        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let file = builder.file_node(range(0, 0));
        builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 4)).public());
        builder.add_node(NodeSpec::new(NodeKind::Function, "b", "b", range(2, 4)).public());
        let new = builder.resolved_edge(EdgeKind::Calls, &file, &b);
        let after = builder.finish();

        let diff = diff_file(Some(&before), &after);
        assert_eq!(diff.delete_edge_ids, vec![old]);
        assert_eq!(diff.upsert_edges.iter().map(|e| e.id.clone()).collect::<Vec<_>>(), vec![new]);
    }

    #[test]
    fn a_deleted_file_deletes_everything_it_had_and_upserts_nothing() {
        let before = graph(&[("a", 1, 4), ("b", 2, 4)]);
        let diff = diff_file(Some(&before), &FileGraph::default());
        assert_eq!(diff.delete_node_ids.len(), before.nodes.len());
        assert_eq!(diff.delete_edge_ids.len(), before.edges.len());
        assert!(diff.upsert_nodes.is_empty());
        assert!(diff.upsert_edges.is_empty());
    }

    /// An edge id is content-derived too, so an edge a semantic tier bound to
    /// a particular overload is not the same edge as the unbound one.
    #[test]
    fn a_declaration_binding_makes_a_new_edge_rather_than_changing_one() {
        let mut builder = FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("a.toy"));
        let file = builder.file_node(range(0, 0));
        let a = builder.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 4)).public());
        let unbound = builder.resolved_edge(EdgeKind::Calls, &file, &a);
        let bound = builder.add_edge(EdgeSpec {
            from_id: file,
            to_id: a,
            kind: EdgeKind::Calls,
            resolved: true,
            to_declaration: Some(1),
            source: SourceTier::Semantic,
            engine: "toy-types".to_string(),
        });
        assert_ne!(unbound, bound);
    }
}
