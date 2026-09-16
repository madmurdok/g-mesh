//! What an [`Extractor`](crate::Extractor) returns for one file, and the
//! builders that make returning a *conformant* one the path of least
//! resistance.
//!
//! # Why a builder rather than "construct `WireNode` yourself"
//!
//! The wire types are public and a plugin may build them by hand. Most of the
//! rules `g-mesh plugins check` enforces, though, are rules about how a node
//! and its id relate - the id is derived from the node's own
//! `(filePath, kind, qualifiedName, nativeKind)`, `DEFINES` runs from the
//! file's `File` node, an edge onto a placeholder is never `resolved: true` -
//! and every one of them is a rule a hand-built node can get wrong silently.
//! [`FileGraphBuilder`] derives the id from the node it is building, so the
//! two cannot disagree, and offers the edge helpers in a shape where the
//! resolved-ness follows from which helper was called.
//!
//! # Open sites are not part of the graph core sees
//!
//! [`OpenSite`]s ride along in [`FileGraph`] because the structural pass is
//! where they are discovered and the semantic pass is where they are
//! answered, and both are about one file. They are never serialized: the bulk
//! stream and every diff carry `nodes` and `edges` only. The SDK keeps them
//! in [`SdkIndex`](crate::SdkIndex) for a [`SemanticEngine`](crate::SemanticEngine)
//! to read.

use g_mesh_wire::{
    EdgeKind, NodeKind, PlaceholderTarget, Position, Range, SourceTier, Visibility, WireDeclaration,
    WireEdge, WireNode,
};

use crate::ids::{edge_id, node_id};
use crate::path::RelPath;

/// One file's structural graph, plus whatever the structural pass could not
/// settle on its own.
///
/// The unit an [`Extractor`](crate::Extractor) returns and the unit the SDK
/// diffs: two `FileGraph`s of the same file are compared id by id and field
/// by field to produce the `fileChanged` answer (see [`diff_file`](crate::diff_file)).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileGraph {
    /// Every node of this file, in the order they are streamed. A file's
    /// `File` node must come first: every `DEFINES`/`EXPORTS` edge starts at
    /// it, and `stream-order` requires an edge's endpoints to have been
    /// emitted already.
    pub nodes: Vec<WireNode>,
    /// Every edge of this file. An edge may only name nodes of this same
    /// file - "edges never leave their file", the invariant that lets core
    /// cut a bulk batch anywhere. A usage that crosses files goes through a
    /// placeholder node *in this file* instead (see [`PlaceholderKind`]).
    pub edges: Vec<WireEdge>,
    /// Use sites the structural tier could not resolve - receiver calls,
    /// ambiguous paths. Never sent to core; handed to the semantic engine.
    pub open_sites: Vec<OpenSite>,
}

impl FileGraph {
    /// Whether this file produced nothing at all - the answer for a file that
    /// was deleted, or that a panicking extractor left no graph for.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty() && self.open_sites.is_empty()
    }

    /// Sets `hasSyntaxErrors` on every node of this file.
    ///
    /// A syntax error is a normal answer, not a failure: an error-tolerant
    /// parser still produces most of a broken file's graph, and losing it
    /// would make an index go blind on exactly the files someone is in the
    /// middle of editing. The flag is per node on the wire because that is
    /// where core stores it, but it is a fact about the *file*, so setting it
    /// one node at a time is a mistake waiting to happen.
    pub fn mark_syntax_errors(&mut self) {
        for node in &mut self.nodes {
            node.has_syntax_errors = true;
        }
    }
}

/// Which kind of thing a use site is waiting on, for a semantic engine that
/// has to decide what question to ask about it.
///
/// Deliberately a small closed set rather than a free string: an
/// [`LspBridge`-style engine](crate::SemanticEngine) maps each variant onto a
/// different LSP request (`textDocument/definition`,
/// `textDocument/implementation`, `callHierarchy/incomingCalls`), and an
/// in-process engine that re-type-checks the file - `go/types`' shape - maps
/// each onto a different lookup in its own `Info`. A variant nothing can
/// answer is worse than no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenSiteKind {
    /// `x.foo()` - a method call through a receiver whose type the structural
    /// pass does not know. The case that made open sites necessary: nearly
    /// every method call in Go and Rust has this shape.
    ReceiverCall,
    /// A name used as a value or type that the structural pass could not bind
    /// to a declaration, and could not honestly address a placeholder at
    /// either (an ambiguous path, a glob import's member).
    Reference,
    /// A type whose implementors or supertypes only the semantic tier knows -
    /// a trait bound, an interface satisfied structurally rather than by
    /// declaration.
    Implementation,
}

/// One use site the structural pass left open, with everything a semantic
/// engine needs to answer it and everything the SDK needs to turn the answer
/// into an edge.
///
/// # Why these fields and not the syntax node
///
/// The two engine shapes this has to fit look at a file from opposite ends. An
/// LSP bridge asks the server a question *at a position* and gets a location
/// back; a `go/types`-style engine re-analyses the file itself and correlates
/// its own AST with what the structural pass said by position. Neither can be
/// handed a tree-sitter node - one is in another process, the other has its
/// own tree - so a position plus the name written at it is the largest common
/// denominator, and it is enough for both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSite {
    /// The node this use site sits inside: the enclosing function or type,
    /// or the file's `File` node at top level. Becomes the `fromId` of
    /// whatever edge the answer produces.
    pub from_id: String,
    /// Where the *name* starts, zero-based, in the same coordinates every
    /// range on the wire uses. An LSP bridge sends exactly this as its
    /// request position.
    pub position: Position,
    /// The name written at the site - `foo` in `x.foo()`. An engine that
    /// answers by re-analysis rather than by position uses it to correlate;
    /// an LSP bridge uses it only for diagnostics.
    pub name: String,
    /// What kind of question this is.
    pub kind: OpenSiteKind,
    /// The edge kind an answer should produce: `CALLS` for a receiver call,
    /// `REFERENCES` for a bare use, `SUPERTYPE_OF` for an implementation.
    /// Carried on the site rather than derived from [`OpenSiteKind`] because
    /// a language may legitimately want a `REFERENCES` edge for a receiver
    /// call it cannot prove is a call.
    pub edge_kind: EdgeKind,
    /// The container the *requester* is in, for the visibility check core
    /// runs on the placeholder the answer becomes
    /// (`PlaceholderTarget::from_container`). `None` for a language with no
    /// containers.
    pub from_container: Option<String>,
}

/// The `nativeKind` of a node that stands in for something outside its own
/// file, and what core does with each.
///
/// Every variant but [`ExternalModule`](PlaceholderKind::ExternalModule)
/// **requires** a [`PlaceholderTarget`] - core's `protocol::conformance`
/// rejects one without, and the kit reports it under `shape`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaceholderKind {
    /// A symbol another file is expected to provide. `graph::symbol_links`
    /// repoints the usage edge onto it once something matching the target is
    /// in the index.
    PendingSymbol,
    /// A name a file only passes through - `pub use`, `export … from`. The
    /// linker follows the chain (bounded at 8 hops) to wherever the
    /// declaration really is.
    Reexport,
    /// A module specifier that names a real file, or a container, of this
    /// project. `graph::imports` repoints the `IMPORTS` edge onto that
    /// `File` or container node and then drops the placeholder.
    ResolvedModule,
    /// A module specifier that names nothing in this project - a package, a
    /// language builtin. **Not** one of core's placeholder kinds: core stores
    /// it as an ordinary `Module` row and never links it, so it carries no
    /// target. It is still a placeholder for the *same-file rule* - nothing
    /// will ever confirm an edge onto it, so `resolved: true` would be a
    /// false claim.
    ExternalModule,
}

impl PlaceholderKind {
    /// The wire string, which must match core's own constants
    /// (`graph::symbol_links::PENDING_SYMBOL_NATIVE_KIND` and its two
    /// siblings). They are spelled out here rather than imported because this
    /// crate deliberately does not depend on core; the conformance kit is
    /// what catches a divergence, under `shape`.
    pub fn native_kind(self) -> &'static str {
        match self {
            PlaceholderKind::PendingSymbol => "pending_symbol",
            PlaceholderKind::Reexport => "reexport",
            PlaceholderKind::ResolvedModule => "resolved_module",
            PlaceholderKind::ExternalModule => "external_module",
        }
    }
}

/// One node to add to a [`FileGraph`], before the SDK gives it its id.
///
/// `filePath`, `language` and `id` are deliberately absent: the builder knows
/// the first two and derives the third, which is what keeps a node's id and
/// its own fields from ever disagreeing.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeSpec {
    /// The storage kind this symbol is stored as.
    pub kind: NodeKind,
    /// The bare name, as written.
    pub name: String,
    /// The name a lookup addresses this symbol by within its file or
    /// container - `T::m`, `Server.Close`, `foo`. Part of the id, so it must
    /// be stable across edits elsewhere in the file.
    pub qualified_name: String,
    /// Where the whole declaration is, zero-based.
    pub range: Range,
    /// Who may reach it. Defaults to [`Visibility::File`] - "says nothing,
    /// assumed to reach least", the same conservative default core's manifest
    /// parsing uses.
    pub visibility: Visibility,
    /// The language's own word for what this is (`function`, `method`,
    /// `trait_impl_method`, `macro`). Part of the id: it is what keeps a
    /// getter and a setter sharing one `qualifiedName` two nodes.
    pub native_kind: Option<String>,
    /// The rendered signature, if the language has one.
    pub signature: Option<String>,
    /// The doc comment attached to the declaration.
    pub doc_comment: Option<String>,
    /// The logical container this declaration is a member of.
    pub container: Option<String>,
    /// That container's own parent key. Sent with every member because core,
    /// not the plugin, materializes container nodes, so this is the only
    /// place a parent relationship is ever stated.
    pub container_parent: Option<String>,
    /// Every declaration this symbol is written as, when there is more than
    /// one. `None` - never `Some(vec![])` - for an ordinary symbol.
    pub declarations: Option<Vec<WireDeclaration>>,
    /// What this node is waiting to be linked onto, for a placeholder.
    ///
    /// Set by [`FileGraphBuilder::add_placeholder`], which also derives the
    /// `qualifiedName` from it so the two cannot describe different things.
    /// A plugin that builds a placeholder through [`NodeSpec`] directly owns
    /// keeping them consistent itself.
    pub target: Option<PlaceholderTarget>,
}

impl NodeSpec {
    /// A declaration with the conservative defaults: file-visible, no native
    /// kind, no container, one declaration.
    pub fn new(
        kind: NodeKind,
        name: impl Into<String>,
        qualified_name: impl Into<String>,
        range: Range,
    ) -> Self {
        Self {
            kind,
            name: name.into(),
            qualified_name: qualified_name.into(),
            range,
            visibility: Visibility::File,
            native_kind: None,
            signature: None,
            doc_comment: None,
            container: None,
            container_parent: None,
            declarations: None,
            target: None,
        }
    }

    /// Visible from anywhere - TS `export`, Go capitalized, Rust `pub`.
    pub fn public(mut self) -> Self {
        self.visibility = Visibility::Public;
        self
    }

    /// Visible to the named container and its descendants - Go unexported,
    /// Rust private, Java package-private.
    pub fn visible_in(mut self, container: impl Into<String>) -> Self {
        self.visibility = Visibility::Container(container.into());
        self
    }

    /// Sets the visibility outright, for a plugin that computes it.
    pub fn visibility(mut self, visibility: Visibility) -> Self {
        self.visibility = visibility;
        self
    }

    /// Sets the language's own kind word. Part of the id - see
    /// [`NodeSpec::native_kind`].
    pub fn native_kind(mut self, native_kind: impl Into<String>) -> Self {
        self.native_kind = Some(native_kind.into());
        self
    }

    /// Sets the rendered signature.
    pub fn signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    /// Sets the doc comment.
    pub fn doc_comment(mut self, doc_comment: impl Into<String>) -> Self {
        self.doc_comment = Some(doc_comment.into());
        self
    }

    /// Records container membership, with the container's own parent key.
    pub fn in_container(mut self, container: impl Into<String>, parent: Option<String>) -> Self {
        self.container = Some(container.into());
        self.container_parent = parent;
        self
    }

    /// Records the symbol's declaration list. An empty list is treated as
    /// "one declaration" and dropped, because that is what core reads an
    /// absent list as and an empty one would be a second way of saying it.
    pub fn declarations(mut self, declarations: Vec<WireDeclaration>) -> Self {
        self.declarations = (!declarations.is_empty()).then_some(declarations);
        self
    }
}

/// One edge to add to a [`FileGraph`], before the SDK gives it its id.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeSpec {
    /// The node the edge starts at - a node of this same file.
    pub from_id: String,
    /// The node it points at - a node of this same file, real or placeholder.
    pub to_id: String,
    /// What the edge means.
    pub kind: EdgeKind,
    /// `true` only when nothing is left for core to confirm, which within one
    /// file means: the target is a real declaration of this file. An edge
    /// onto a placeholder is always `false`.
    pub resolved: bool,
    /// Which declaration of an overloaded target this call binds. Only ever
    /// set by a semantic tier, and part of the edge's id.
    pub to_declaration: Option<u32>,
    /// Which tier produced it.
    pub source: SourceTier,
    /// The engine that produced it, as a free label for diagnostics -
    /// `tree-sitter`, `go-types`, `rust-analyzer`.
    pub engine: String,
}

/// Accumulates one file's nodes and edges, deriving each id from the item
/// itself.
///
/// Holds the file path, the language and the engine label so a plugin states
/// each once per file rather than once per node - which is also what makes
/// `ownership.language` unfailable by construction for an SDK plugin.
pub struct FileGraphBuilder {
    language: String,
    engine: String,
    file: RelPath,
    graph: FileGraph,
}

impl FileGraphBuilder {
    /// Starts a file's graph. `engine` is the structural engine's label, on
    /// every edge this builder produces (`tree-sitter` for a tree-sitter
    /// plugin).
    pub fn new(language: &str, engine: &str, file: &RelPath) -> Self {
        Self {
            language: language.to_string(),
            engine: engine.to_string(),
            file: file.clone(),
            graph: FileGraph::default(),
        }
    }

    /// Adds the file's own `File` node and returns its id.
    ///
    /// Call this first: `DEFINES`/`EXPORTS` start here, and an edge may only
    /// name a node already emitted. `qualifiedName` is the file's path, which
    /// is the convention `graph::imports` links against; `name` is its last
    /// path segment.
    ///
    /// `range` is the whole file. Get its end right or the whitespace-edit
    /// check will fail for a reason that has nothing to do with the
    /// extractor: the end is `(number of newlines, length of the final
    /// unterminated line)`, which a space inserted before the last newline
    /// does not move - whereas an end of `(lines, 0)` or a byte count does.
    pub fn file_node(&mut self, range: Range) -> String {
        let name = self.file.as_str().rsplit('/').next().unwrap_or(self.file.as_str()).to_string();
        let spec = NodeSpec::new(NodeKind::File, name, self.file.as_str(), range);
        self.add_node(spec)
    }

    /// Adds a declaration and returns its id.
    pub fn add_node(&mut self, spec: NodeSpec) -> String {
        let id = node_id(self.file.as_str(), spec.kind, &spec.qualified_name, spec.native_kind.as_deref());
        self.graph.nodes.push(WireNode {
            id: id.clone(),
            kind: spec.kind,
            name: spec.name,
            qualified_name: spec.qualified_name,
            file_path: self.file.as_str().to_string(),
            range: spec.range,
            signature: spec.signature,
            visibility: spec.visibility,
            doc_comment: spec.doc_comment,
            language: self.language.clone(),
            native_kind: spec.native_kind,
            has_syntax_errors: false,
            declarations: spec.declarations,
            container: spec.container,
            container_parent: spec.container_parent,
            target: spec.target,
        });
        id
    }

    /// Adds a placeholder standing in for something outside this file, and
    /// returns its id.
    ///
    /// The placeholder's `qualifiedName` is derived from the target so that
    /// two placeholders waiting on different things never share an id, and
    /// two waiting on the same thing always do - which is what keeps one file
    /// from emitting the same placeholder twice under two ids. The rendering
    /// is `<file>#<key>` for a file scope and `<container>::<key>` for a
    /// container scope; the two shapes cannot collide, and `nativeKind` -
    /// also in the id - keeps the placeholder kinds apart on top of that.
    ///
    /// `range` is where the *use site* is: a placeholder is a node of the
    /// file that is waiting, not of the file it is waiting on.
    pub fn add_placeholder(
        &mut self,
        kind: PlaceholderKind,
        name: impl Into<String>,
        target: PlaceholderTarget,
        range: Range,
    ) -> String {
        let qualified_name = render_target(&target);
        let mut spec =
            NodeSpec::new(NodeKind::Module, name, qualified_name, range).native_kind(kind.native_kind());
        spec.target = Some(target);
        self.add_node(spec)
    }

    /// Adds an `external_module` node - a specifier naming nothing in this
    /// project - and returns its id. Carries no target, because core never
    /// links one; see [`PlaceholderKind::ExternalModule`].
    pub fn add_external_module(&mut self, specifier: impl Into<String>, range: Range) -> String {
        let specifier = specifier.into();
        let spec = NodeSpec::new(NodeKind::Module, specifier.clone(), specifier, range)
            .native_kind(PlaceholderKind::ExternalModule.native_kind());
        self.add_node(spec)
    }

    /// Adds an edge and returns its id.
    pub fn add_edge(&mut self, spec: EdgeSpec) -> String {
        let id = edge_id(&spec.from_id, spec.kind, &spec.to_id, spec.to_declaration);
        self.graph.edges.push(WireEdge {
            id: id.clone(),
            from_id: spec.from_id,
            to_id: spec.to_id,
            kind: spec.kind,
            source: spec.source,
            engine: spec.engine,
            resolved: spec.resolved,
            to_declaration: spec.to_declaration,
        });
        id
    }

    /// An edge onto a real declaration **of this same file**: `resolved:
    /// true`, because within its own file a plugin has nothing left to
    /// confirm.
    pub fn resolved_edge(&mut self, kind: EdgeKind, from_id: &str, to_id: &str) -> String {
        self.structural_edge(kind, from_id, to_id, true)
    }

    /// An edge onto a placeholder: `resolved: false`, because only core can
    /// confirm it.
    pub fn placeholder_edge(&mut self, kind: EdgeKind, from_id: &str, to_id: &str) -> String {
        self.structural_edge(kind, from_id, to_id, false)
    }

    /// `DEFINES` plus, for a publicly visible symbol, `EXPORTS` - both from
    /// the file's `File` node, which is the only place either may start.
    pub fn defines(&mut self, file_id: &str, node_id: &str, exported: bool) {
        self.resolved_edge(EdgeKind::Defines, file_id, node_id);
        if exported {
            self.resolved_edge(EdgeKind::Exports, file_id, node_id);
        }
    }

    fn structural_edge(&mut self, kind: EdgeKind, from_id: &str, to_id: &str, resolved: bool) -> String {
        let engine = self.engine.clone();
        self.add_edge(EdgeSpec {
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            kind,
            resolved,
            to_declaration: None,
            source: SourceTier::Syntactic,
            engine,
        })
    }

    /// Records a use site the structural pass could not settle.
    pub fn open_site(&mut self, site: OpenSite) {
        self.graph.open_sites.push(site);
    }

    /// Marks the whole file as having syntax errors - see
    /// [`FileGraph::mark_syntax_errors`].
    pub fn mark_syntax_errors(&mut self) {
        self.graph.mark_syntax_errors();
    }

    /// The finished graph.
    pub fn finish(self) -> FileGraph {
        self.graph
    }
}

/// The `qualifiedName` a placeholder carries - see
/// [`FileGraphBuilder::add_placeholder`].
///
/// It is a label, not an address: core reads the structured `target` row, not
/// this string. What it has to be is *injective enough* that two placeholders
/// in one file waiting on different things get different ids.
fn render_target(target: &PlaceholderTarget) -> String {
    use g_mesh_wire::{TargetKey, TargetScope};
    let key = match &target.key {
        TargetKey::Name(name) => name,
        TargetKey::QualifiedName(qualified_name) => qualified_name,
    };
    match &target.scope {
        TargetScope::File(path) => format!("{path}#{key}"),
        TargetScope::Container(container) => format!("{container}::{key}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_wire::{TargetKey, TargetScope};

    fn range(start_line: u32, end_line: u32) -> Range {
        Range { start: Position { line: start_line, col: 0 }, end: Position { line: end_line, col: 1 } }
    }

    fn builder() -> FileGraphBuilder {
        FileGraphBuilder::new("toy", "toy-parser", &RelPath::new("src/a.toy"))
    }

    #[test]
    fn a_nodes_id_is_derived_from_the_node_itself() {
        let mut graph = builder();
        let id = graph.add_node(NodeSpec::new(NodeKind::Function, "foo", "foo", range(1, 2)).public());
        assert_eq!(id, crate::ids::node_id("src/a.toy", NodeKind::Function, "foo", None));
        let graph = graph.finish();
        assert_eq!(graph.nodes[0].id, id);
        assert_eq!(graph.nodes[0].file_path, "src/a.toy");
        assert_eq!(graph.nodes[0].language, "toy");
    }

    #[test]
    fn the_file_node_is_named_after_its_last_path_segment_and_addressed_by_its_path() {
        let mut graph = builder();
        graph.file_node(range(0, 3));
        let graph = graph.finish();
        assert_eq!(graph.nodes[0].name, "a.toy");
        assert_eq!(graph.nodes[0].qualified_name, "src/a.toy");
        assert_eq!(graph.nodes[0].kind, NodeKind::File);
    }

    /// Two placeholders waiting on different things must not share an id -
    /// otherwise one file's second unresolved import silently replaces its
    /// first.
    #[test]
    fn placeholders_waiting_on_different_things_get_different_ids() {
        let mut graph = builder();
        let a = graph.add_placeholder(
            PlaceholderKind::PendingSymbol,
            "helper",
            PlaceholderTarget {
                scope: TargetScope::File("src/b.toy".into()),
                key: TargetKey::Name("helper".into()),
                from_container: None,
            },
            range(1, 1),
        );
        let b = graph.add_placeholder(
            PlaceholderKind::PendingSymbol,
            "helper",
            PlaceholderTarget {
                scope: TargetScope::File("src/c.toy".into()),
                key: TargetKey::Name("helper".into()),
                from_container: None,
            },
            range(2, 2),
        );
        let c = graph.add_placeholder(
            PlaceholderKind::PendingSymbol,
            "helper",
            PlaceholderTarget {
                scope: TargetScope::Container("pkg".into()),
                key: TargetKey::Name("helper".into()),
                from_container: None,
            },
            range(3, 3),
        );
        // Same target, different placeholder kind - `nativeKind` is in the id.
        let d = graph.add_placeholder(
            PlaceholderKind::Reexport,
            "helper",
            PlaceholderTarget {
                scope: TargetScope::File("src/b.toy".into()),
                key: TargetKey::Name("helper".into()),
                from_container: None,
            },
            range(4, 4),
        );
        let ids = [&a, &b, &c, &d];
        for (i, one) in ids.iter().enumerate() {
            for two in ids.iter().skip(i + 1) {
                assert_ne!(one, two, "two placeholders collided on one id");
            }
        }
    }

    /// Every placeholder kind core links carries a target on the wire - the
    /// rule `protocol::conformance` enforces and the kit reports under
    /// `shape`. `external_module` is the documented exception.
    #[test]
    fn every_linkable_placeholder_carries_its_target_and_external_modules_carry_none() {
        let mut graph = builder();
        for kind in
            [PlaceholderKind::PendingSymbol, PlaceholderKind::Reexport, PlaceholderKind::ResolvedModule]
        {
            graph.add_placeholder(
                kind,
                "x",
                PlaceholderTarget {
                    scope: TargetScope::File("src/b.toy".into()),
                    key: TargetKey::Name("x".into()),
                    from_container: None,
                },
                range(1, 1),
            );
        }
        graph.add_external_module("some-package", range(2, 2));
        let graph = graph.finish();
        for node in &graph.nodes[..3] {
            assert!(node.target.is_some(), "{:?} must carry a target", node.native_kind);
        }
        assert_eq!(graph.nodes[3].native_kind.as_deref(), Some("external_module"));
        assert!(graph.nodes[3].target.is_none());
    }

    #[test]
    fn defines_emits_exports_only_for_a_visible_symbol() {
        let mut graph = builder();
        let file = graph.file_node(range(0, 2));
        let a = graph.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 1)).public());
        let b = graph.add_node(NodeSpec::new(NodeKind::Function, "b", "b", range(2, 2)));
        graph.defines(&file, &a, true);
        graph.defines(&file, &b, false);
        let graph = graph.finish();
        let kinds: Vec<_> = graph.edges.iter().map(|e| (e.kind, e.to_id.clone())).collect();
        assert_eq!(
            kinds,
            vec![(EdgeKind::Defines, a.clone()), (EdgeKind::Exports, a), (EdgeKind::Defines, b)]
        );
        assert!(graph.edges.iter().all(|e| e.resolved), "DEFINES/EXPORTS stay inside the file");
    }

    #[test]
    fn marking_syntax_errors_marks_every_node_of_the_file() {
        let mut graph = builder();
        graph.file_node(range(0, 1));
        graph.add_node(NodeSpec::new(NodeKind::Function, "a", "a", range(1, 1)));
        graph.mark_syntax_errors();
        let graph = graph.finish();
        assert!(graph.nodes.iter().all(|node| node.has_syntax_errors));
    }
}
