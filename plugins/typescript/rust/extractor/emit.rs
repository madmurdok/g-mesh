//! The settled draft, flushed into the SDK's [`FileGraphBuilder`].
//!
//! Every node goes through [`NodeSpec`] directly, placeholders included, so a
//! placeholder keeps this plugin's own `qualifiedName` (a `resolved_module`'s
//! is the bare resolved path) rather than the one
//! [`FileGraphBuilder::add_placeholder`] would derive from its target. Nodes
//! and edges are pushed in the draft's insertion order, then the open sites.
//! Receiver-call sites are never folded into `untypedCalls`: that field would
//! change what core stores and what the caller pages say.

use g_mesh_plugin_sdk::wire::{QualifiedPath, SourceTier, Visibility};
use g_mesh_plugin_sdk::{EdgeSpec, FileGraph, FileGraphBuilder, NodeSpec, RelPath};

use crate::extractor::model::FileModel;

/// Builds the file's wire graph from `model`, marking every node when the
/// parse had syntax errors.
pub fn flush(
    mut model: FileModel,
    language: &str,
    engine: &str,
    path: &RelPath,
    syntax_errors: bool,
) -> FileGraph {
    let mut builder = FileGraphBuilder::new(language, engine, path);
    let open_sites = model.take_open_sites();
    let (nodes, edges) = model.into_parts();
    for node in nodes {
        let mut spec = NodeSpec::new(node.kind, node.name, node.qualified_name, node.range);
        spec.visibility = if node.exported { Visibility::Public } else { Visibility::File };
        spec.native_kind = node.native_kind;
        spec.signature = node.signature;
        spec.doc_comment = node.doc_comment;
        spec.declarations = node.declarations;
        spec.target = node.target;
        spec.qualified_path = node.qualified_path.map(QualifiedPath);
        let id = builder.add_node(spec);
        debug_assert_eq!(id, node.id, "the draft and the SDK derive a node id the same way");
    }
    for edge in edges {
        let id = builder.add_edge(EdgeSpec {
            from_id: edge.from_id,
            to_id: edge.to_id,
            kind: edge.kind,
            resolved: edge.resolved,
            to_declaration: None,
            source: SourceTier::Syntactic,
            engine: engine.to_string(),
        });
        debug_assert_eq!(id, edge.id, "the draft and the SDK derive an edge id the same way");
    }
    for site in open_sites {
        builder.open_site(site);
    }
    if syntax_errors {
        builder.mark_syntax_errors();
    }
    builder.finish()
}
