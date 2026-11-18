//! De-duplication by id over a [`FileGraphBuilder`].
//!
//! A node's id is `(filePath, kind, qualifiedName, nativeKind)` and an edge's
//! is `(fromId, kind, toId)`. Source code writes the same id twice in
//! ordinary code: conditional definitions, a rebinding, the same call twice,
//! an import and a use through it. Emitting the duplicate would put two lines
//! with one id into the bulk stream and two entries into a diff's
//! `upsertNodes`. [`GraphEmitter`] emits the **first** occurrence in source
//! order; every later one resolves to that same id, and edges written from
//! inside a later branch land on the node the first produced.

use std::collections::{HashMap, HashSet};

use g_mesh_wire::{EdgeKind, NodeKind, PlaceholderTarget, Range, TargetKey, TargetScope};

use crate::graph::{FileGraph, FileGraphBuilder, NodeSpec, OpenSite, PlaceholderKind};
use crate::ids::{edge_id, node_id};
use crate::path::RelPath;

/// How a placeholder is recognised as one already emitted: everything its id
/// is derived from, in a form that can key a map.
type PlaceholderKey = (&'static str, bool, String, bool, String);

fn placeholder_key(kind: PlaceholderKind, target: &PlaceholderTarget) -> PlaceholderKey {
    let (is_container, scope) = match &target.scope {
        TargetScope::File(path) => (false, path.clone()),
        TargetScope::Container(container) => (true, container.clone()),
    };
    let (is_qualified, key) = match &target.key {
        TargetKey::Name(name) => (false, name.clone()),
        TargetKey::QualifiedName(qualified) => (true, qualified.clone()),
    };
    (kind.native_kind(), is_container, scope, is_qualified, key)
}

/// The graph being built for one file, with every id emitted so far.
pub struct GraphEmitter {
    graph: FileGraphBuilder,
    path: RelPath,
    file_id: String,
    nodes: HashSet<String>,
    edges: HashSet<String>,
    placeholders: HashMap<PlaceholderKey, String>,
}

impl GraphEmitter {
    /// Starts a file's graph with its `File` node, `file`, which must be
    /// first: every `DEFINES` edge starts there, and an edge may only name a
    /// node already emitted. `graph` is passed in so the caller can configure
    /// the builder before the first node.
    pub fn new(mut graph: FileGraphBuilder, path: &RelPath, file: NodeSpec) -> Self {
        let file_id = graph.add_node(file);
        Self {
            graph,
            path: path.clone(),
            file_id: file_id.clone(),
            nodes: HashSet::from([file_id]),
            edges: HashSet::new(),
            placeholders: HashMap::new(),
        }
    }

    /// The `File` node's id.
    pub fn file_id(&self) -> &str {
        &self.file_id
    }

    /// The underlying builder, for the writes this layer does not wrap.
    pub fn graph_mut(&mut self) -> &mut FileGraphBuilder {
        &mut self.graph
    }

    /// Adds a declaration, with its `DEFINES` edge (and `EXPORTS` when
    /// `exported`), and returns its id. A second declaration resolving to an
    /// id already emitted adds nothing and returns the first one's id.
    pub fn declare(&mut self, spec: NodeSpec, exported: bool) -> String {
        let id = node_id(self.path.as_str(), spec.kind, &spec.qualified_name, spec.native_kind.as_deref());
        if self.nodes.insert(id.clone()) {
            self.graph.add_node(spec);
            self.graph.defines(&self.file_id.clone(), &id, exported);
        }
        id
    }

    /// Whether a declaration with `spec`'s id was already emitted.
    pub fn is_declared(&self, spec: &NodeSpec) -> bool {
        self.nodes.contains(&node_id(
            self.path.as_str(),
            spec.kind,
            &spec.qualified_name,
            spec.native_kind.as_deref(),
        ))
    }

    /// Records `id` as emitted; false when it already was. For a node a
    /// caller builds and adds itself (a re-export), possibly later via
    /// [`Self::add_node`].
    pub fn claim_node(&mut self, id: &str) -> bool {
        self.nodes.insert(id.to_string())
    }

    /// Adds a node already claimed with [`Self::claim_node`].
    pub fn add_node(&mut self, spec: NodeSpec) -> String {
        self.graph.add_node(spec)
    }

    /// Adds (or finds) the placeholder waiting on `target`, and returns its
    /// id. Two placeholders waiting on one thing are one node.
    pub fn placeholder(
        &mut self,
        kind: PlaceholderKind,
        name: &str,
        target: PlaceholderTarget,
        range: Range,
    ) -> String {
        let cache_key = placeholder_key(kind, &target);
        if let Some(id) = self.placeholders.get(&cache_key) {
            return id.clone();
        }
        let id = self.graph.add_placeholder(kind, name, target, range);
        self.nodes.insert(id.clone());
        self.placeholders.insert(cache_key, id.clone());
        id
    }

    /// Adds (or finds) the `external_module` node for a module the project
    /// does not contain, and returns its id.
    pub fn external_module(&mut self, name: &str, range: Range) -> String {
        let id = node_id(self.path.as_str(), NodeKind::Module, name, Some("external_module"));
        if self.nodes.insert(id.clone()) {
            self.graph.add_external_module(name, range);
        }
        id
    }

    /// An edge onto a declaration of this same file: `resolved: true`.
    /// Returns its id, which a repeat shares.
    pub fn resolved_edge(&mut self, kind: EdgeKind, from: &str, to: &str) -> String {
        let id = edge_id(from, kind, to, None);
        if self.edges.insert(id.clone()) {
            self.graph.resolved_edge(kind, from, to);
        }
        id
    }

    /// An edge onto a placeholder: `resolved: false`, since only core can
    /// confirm it. Returns its id, which a repeat shares.
    pub fn placeholder_edge(&mut self, kind: EdgeKind, from: &str, to: &str) -> String {
        let id = edge_id(from, kind, to, None);
        if self.edges.insert(id.clone()) {
            self.graph.placeholder_edge(kind, from, to);
        }
        id
    }

    /// An `IMPORTS` edge onto a placeholder, carrying the import's text as
    /// written as its `specifier`. When two imports draw the same edge, the
    /// first one's text is kept; the id does not include it.
    pub fn import_edge(&mut self, from: &str, to: &str, specifier: &str) -> String {
        let id = edge_id(from, EdgeKind::Imports, to, None);
        if self.edges.insert(id.clone()) {
            self.graph.import_edge(from, to, specifier);
        }
        id
    }

    /// Records a use site the structural tier could not settle.
    pub fn open_site(&mut self, site: OpenSite) {
        self.graph.open_site(site);
    }

    /// Marks every node of this file as having come from a source with a
    /// syntax error. A normal answer, not a failure.
    pub fn mark_syntax_errors(&mut self) {
        self.graph.mark_syntax_errors();
    }

    /// The finished graph.
    pub fn finish(self) -> FileGraph {
        self.graph.finish()
    }
}
