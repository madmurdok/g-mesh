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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{placeholder_id, render_target};
    use g_mesh_wire::{Position, Visibility};

    const FILE: &str = "pkg/mod.toy";

    fn range() -> Range {
        Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } }
    }

    fn emitter() -> GraphEmitter {
        let path = RelPath::new(FILE);
        let file = NodeSpec::new(NodeKind::File, "mod.toy", FILE, range());
        GraphEmitter::new(FileGraphBuilder::new("toy", "test", &path), &path, file)
    }

    fn function(name: &str) -> NodeSpec {
        NodeSpec::new(NodeKind::Function, name, name, range())
            .native_kind("function")
            .visibility(Visibility::Public)
    }

    fn target(scope: TargetScope, key: TargetKey) -> PlaceholderTarget {
        PlaceholderTarget { scope, key, from_container: None, key_path: None, also_scopes: Vec::new() }
    }

    #[test]
    fn the_file_node_is_first_and_its_id_is_file_id() {
        let emitter = emitter();
        let file_id = emitter.file_id().to_string();
        assert_eq!(file_id, node_id(FILE, NodeKind::File, FILE, None));
        let graph = emitter.finish();
        assert_eq!(graph.nodes.first().map(|node| node.id.as_str()), Some(file_id.as_str()));
    }

    /// The first declaration of an id wins; a second adds no node and no
    /// `DEFINES`/`EXPORTS` edge, and answers with the first one's id.
    #[test]
    fn a_second_declaration_of_one_id_adds_nothing_and_reuses_the_first() {
        let mut emitter = emitter();
        let first = emitter.declare(function("f"), true);
        let mut later = function("f");
        later.range = Range { start: Position { line: 5, col: 0 }, end: Position { line: 5, col: 1 } };
        let second = emitter.declare(later, true);
        assert_eq!(first, second);
        let graph = emitter.finish();
        assert_eq!(graph.nodes.len(), 2, "the File node and one `f`: {:?}", graph.nodes);
        assert_eq!(graph.nodes[1].range.start.line, 0, "the first occurrence is the one kept");
        assert_eq!(graph.edges.len(), 2, "one DEFINES and one EXPORTS: {:?}", graph.edges);
    }

    /// Control C8: drop the `edges` dedup set (`resolved_edge` adds
    /// unconditionally) and this fails.
    #[test]
    fn one_edge_written_twice_is_one_edge() {
        let mut emitter = emitter();
        let id = emitter.declare(function("f"), true);
        let file = emitter.file_id().to_string();
        let first = emitter.resolved_edge(EdgeKind::Calls, &file, &id);
        let second = emitter.resolved_edge(EdgeKind::Calls, &file, &id);
        assert_eq!(first, second);
        let graph = emitter.finish();
        let calls: Vec<_> = graph.edges.iter().filter(|edge| edge.kind == EdgeKind::Calls).collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].resolved);
    }

    #[test]
    fn placeholder_and_import_edges_are_deduped_and_the_first_specifier_is_kept() {
        let mut emitter = emitter();
        let f = emitter.declare(function("f"), false);
        let file = emitter.file_id().to_string();
        let p = emitter.placeholder(
            PlaceholderKind::PendingSymbol,
            "g",
            target(TargetScope::File("other.toy".into()), TargetKey::Name("g".into())),
            range(),
        );
        emitter.placeholder_edge(EdgeKind::Calls, &f, &p);
        emitter.placeholder_edge(EdgeKind::Calls, &f, &p);
        emitter.import_edge(&file, &p, "./other");
        emitter.import_edge(&file, &p, "other");
        let graph = emitter.finish();
        let calls: Vec<_> = graph.edges.iter().filter(|edge| edge.kind == EdgeKind::Calls).collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(!calls[0].resolved, "an edge onto a placeholder is never resolved");
        let imports: Vec<_> = graph.edges.iter().filter(|edge| edge.kind == EdgeKind::Imports).collect();
        assert_eq!(imports.len(), 1, "{imports:?}");
        assert_eq!(imports[0].specifier.as_deref(), Some("./other"));
    }

    /// A placeholder's id is the one `placeholder_id` (and so `render_target`)
    /// gives its target; two waiting on one target are one node, and every
    /// part of the target - kind, scope shape, key kind - tells two apart.
    ///
    /// Control: drop the key's kind (`is_qualified`) from `placeholder_key`
    /// and the name/qualified-name pair collapses into one node.
    #[test]
    fn placeholders_are_one_node_per_address_and_keyed_by_every_part_of_it() {
        let mut emitter = emitter();
        let path = RelPath::new(FILE);
        let name_in_container = target(TargetScope::Container("a.b".into()), TargetKey::Name("C".into()));
        let cases = [
            (PlaceholderKind::PendingSymbol, name_in_container.clone()),
            (
                PlaceholderKind::PendingSymbol,
                target(TargetScope::Container("a.b".into()), TargetKey::QualifiedName("C".into())),
            ),
            (
                PlaceholderKind::PendingSymbol,
                target(TargetScope::File("a.b".into()), TargetKey::Name("C".into())),
            ),
            (PlaceholderKind::Reexport, name_in_container.clone()),
        ];
        let ids: Vec<String> = cases
            .iter()
            .map(|(kind, target)| {
                let id = emitter.placeholder(*kind, "C", target.clone(), range());
                assert_eq!(id, placeholder_id(&path, *kind, target), "{kind:?} {target:?}");
                id
            })
            .collect();
        let distinct: HashSet<_> = ids.iter().collect();
        assert_eq!(distinct.len(), cases.len(), "{ids:?}");

        let again =
            emitter.placeholder(PlaceholderKind::PendingSymbol, "other-site", name_in_container, range());
        assert_eq!(again, ids[0]);
        let graph = emitter.finish();
        assert_eq!(graph.nodes.len(), 1 + cases.len(), "{:?}", graph.nodes);
        assert_eq!(graph.nodes[1].qualified_name, render_target(&cases[0].1));
    }

    #[test]
    fn an_external_module_is_one_node() {
        let mut emitter = emitter();
        let a = emitter.external_module("os", range());
        let b = emitter.external_module("os", range());
        assert_eq!(a, b);
        assert_eq!(a, node_id(FILE, NodeKind::Module, "os", Some("external_module")));
        assert_eq!(emitter.finish().nodes.len(), 2);
    }

    /// A node a caller adds itself is claimed once; a later `declare` of the
    /// same id then adds nothing.
    #[test]
    fn claim_node_and_is_declared_share_the_set_declare_uses() {
        let mut emitter = emitter();
        let spec = function("f");
        assert!(!emitter.is_declared(&spec));
        let id = node_id(FILE, spec.kind, &spec.qualified_name, spec.native_kind.as_deref());
        assert!(emitter.claim_node(&id));
        assert!(!emitter.claim_node(&id), "a second claim is refused");
        assert!(emitter.is_declared(&spec));
        assert_eq!(emitter.add_node(spec.clone()), id);
        assert_eq!(emitter.declare(spec, true), id);
        let graph = emitter.finish();
        assert_eq!(graph.nodes.len(), 2, "{:?}", graph.nodes);
        assert!(graph.edges.is_empty(), "declare of a claimed id adds no DEFINES: {:?}", graph.edges);
    }
}
