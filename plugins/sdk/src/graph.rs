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

use std::collections::HashMap;

use g_mesh_wire::{
    EdgeKind, NodeKind, PlaceholderTarget, Position, QualifiedPath, Range, SourceTier, Visibility,
    WireDeclaration, WireEdge, WireNode,
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
    /// ambiguous paths. Never sent to core as such; handed to the semantic
    /// engine. For an extractor that opts in
    /// ([`FileGraphBuilder::record_untyped_receiver_calls`]), the untyped
    /// receiver calls among them also reach core as their enclosing node's
    /// `untypedCalls`.
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
    /// `x.f` - a named field read through a receiver: the field-read twin of
    /// [`OpenSiteKind::ReceiverCall`]. [`OpenSite::replaces`] names the
    /// structural `REFERENCES` edge when the plugin typed the receiver, and
    /// is `None` when it did not; either way it is asked like `ReceiverCall`.
    ReceiverField,
    /// A name used as a value or type that the structural pass could not bind
    /// to a declaration, and could not honestly address a placeholder at
    /// either (an ambiguous path, a glob import's member).
    Reference,
    /// A type whose implementors or supertypes only the semantic tier knows -
    /// a trait bound, an interface satisfied structurally rather than by
    /// declaration.
    Implementation,
    /// A call whose structural `CALLS` edge is right about *which function*
    /// it reaches, and silent about *which of its overloads*: the target may
    /// carry [`NodeSpec::declarations`], and only a type checker knows which
    /// one the call binds.
    ///
    /// Unlike the other kinds, this one **refines** an edge rather than
    /// replacing or contradicting one, so [`OpenSite::replaces`] is required
    /// and names the structural edge, and `edge_kind` is `Calls`. A semantic
    /// engine either binds every such site of that edge to a declaration
    /// ordinal (the edge then gives way to bound edges carrying
    /// `toDeclaration`) or leaves the structural edge exactly as it was. It
    /// never moves the call to another target. See
    /// `docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`.
    ///
    /// A plugin records it for a call bound to a node that has
    /// `declarations`, and for any call bound to a placeholder (it cannot know
    /// whether the target is overloaded; the engine filters).
    OverloadCall,
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
    /// The id of the syntactic edge this site's answer *replaces*, when the
    /// structural tier emitted one it is not sure of.
    ///
    /// Almost always `None`: an open site usually exists precisely because
    /// nothing could be emitted for it, and there is then no edge to replace.
    /// The shape that needs it is the one `plugins/go/semantic.go` calls a
    /// `placeholderCall` - a `CALLS` edge the structural tier emitted on a
    /// guess (`T(x)` looks like a call, and turns out to be a conversion)
    /// which a semantic answer can contradict. Typed receiver calls
    /// ([`OpenSiteKind::ReceiverCall`]) and typed field reads
    /// ([`OpenSiteKind::ReceiverField`]) carry it the same way: the edge the
    /// plugin addressed through the receiver's written type. A re-export hop
    /// (a TypeScript [`OpenSiteKind::Reference`]) carries the placeholder
    /// edge it guessed.
    ///
    /// It exists because the semantic tier cannot work it out: `from_id` and
    /// [`edge_kind`](OpenSite::edge_kind) do not name an edge - one function
    /// calling two same-named methods through different receivers produces two
    /// sites with an identical pair - so an engine that retracted on that basis
    /// would delete correct edges to repair ones that were never wrong. Only
    /// the extractor knows which edge it wrote for which site, so only the
    /// extractor can say. A semantic engine retracts this id when its answer
    /// lands somewhere else, and leaves it alone when the answer confirms it
    /// (see [`crate::lsp::LspBridge`]'s retraction rules).
    pub replaces: Option<String>,
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
    /// `qualified_name` as segments. Set by [`NodeSpec::with_path`], which
    /// also derives `qualified_name` from it, so the two always agree.
    pub qualified_path: Option<QualifiedPath>,
    /// Other spellings of this declaration, appended by [`NodeSpec::alias`].
    pub alias_paths: Vec<QualifiedPath>,
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
            qualified_path: None,
            alias_paths: Vec::new(),
        }
    }

    /// A declaration addressed by `path`: `qualifiedName` is the path joined,
    /// so the node id is exactly what [`NodeSpec::new`] with that string
    /// would give. The path's last segment must be `name`; core drops a path
    /// that breaks a rule of [`QualifiedPath::check`] and keeps the node.
    pub fn with_path(kind: NodeKind, name: impl Into<String>, path: QualifiedPath, range: Range) -> Self {
        let mut spec = Self::new(kind, name, path.display(), range);
        spec.qualified_path = Some(path);
        spec
    }

    /// Adds another spelling by which a partial-path lookup finds this
    /// declaration. It must have at least two segments, end in `name` and
    /// differ from the node's own path; it need not join to `qualifiedName`.
    pub fn alias(mut self, path: QualifiedPath) -> Self {
        self.alias_paths.push(path);
        self
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
    record_untyped: bool,
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
            record_untyped: false,
        }
    }

    /// Opts this file into reporting untyped receiver calls: [`finish`](Self::finish)
    /// folds every [`OpenSiteKind::ReceiverCall`] site with no
    /// [`replaces`](OpenSite::replaces) into its enclosing node's
    /// `untyped_calls`, which core stores so a caller page can say it may be
    /// missing such calls. Off by default: a plugin that has not opted in
    /// sends no `untypedCalls`, which core reads as "not reported".
    pub fn record_untyped_receiver_calls(&mut self) {
        self.record_untyped = true;
    }

    /// Adds the file's own `File` node and returns its id.
    ///
    /// Call this first: `DEFINES`/`EXPORTS` start here, and an edge may only
    /// name a node already emitted. `qualifiedName` is the file's path, which
    /// is the convention `graph::imports` links against; `name` is its last
    /// path segment.
    ///
    /// `range` is the whole file, ending where its content ends: trim the
    /// trailing whitespace, then the end is `(number of newlines, length of
    /// the last line)` of what is left
    /// ([`CharColumns::file_range`](crate::CharColumns::file_range)). So the
    /// end line is the file's last real line, never the empty line after a
    /// final newline. Get it right or the whitespace-edit check fails for a
    /// reason that has nothing to do with the extractor: it requires that
    /// end line, and a space inserted before the last newline must not move
    /// the end - whereas an end of `(lines, 0)` or a byte count does.
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
            qualified_path: spec.qualified_path,
            alias_paths: spec.alias_paths,
            untyped_calls: Vec::new(),
        });
        id
    }

    /// Sets the overload/merge declaration list of a node already added, by
    /// its id. `false` when no node of this file has that id.
    ///
    /// For an extractor that only knows a node's full declaration list after
    /// its last redeclaration, by which time the node has been pushed. Same
    /// rule as [`NodeSpec::declarations`]: an empty list is no list.
    pub fn set_declarations(&mut self, id: &str, declarations: Vec<WireDeclaration>) -> bool {
        let Some(node) = self.graph.nodes.iter_mut().find(|node| node.id == id) else { return false };
        node.declarations = (!declarations.is_empty()).then_some(declarations);
        true
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

    /// The finished graph. When [`record_untyped_receiver_calls`](Self::record_untyped_receiver_calls)
    /// was called, every untyped receiver call (a `ReceiverCall` site that
    /// replaces no edge) lands on its `from_id` node's `untyped_calls`,
    /// sorted and deduplicated. Only a `File` or `Function` node takes them -
    /// the kinds core accepts the field on - so a site inside another
    /// declaration (a `static`'s initializer, say) is not reported.
    pub fn finish(mut self) -> FileGraph {
        if self.record_untyped {
            fold_untyped_calls(&mut self.graph);
        }
        self.graph
    }
}

/// The fold behind [`FileGraphBuilder::finish`]'s opt-in.
fn fold_untyped_calls(graph: &mut FileGraph) {
    let mut by_node: HashMap<&str, Vec<String>> = HashMap::new();
    for site in &graph.open_sites {
        if site.kind == OpenSiteKind::ReceiverCall && site.replaces.is_none() {
            by_node.entry(site.from_id.as_str()).or_default().push(site.name.clone());
        }
    }
    for node in &mut graph.nodes {
        if !matches!(node.kind, NodeKind::File | NodeKind::Function) {
            continue;
        }
        if let Some(mut names) = by_node.remove(node.id.as_str()) {
            names.sort();
            names.dedup();
            node.untyped_calls = names;
        }
    }
}

/// The `qualifiedName` a placeholder carries - see
/// [`FileGraphBuilder::add_placeholder`].
///
/// It is a label, not an address: core reads the structured `target` row, not
/// this string. What it has to be is *injective enough* that two placeholders
/// in one file waiting on different things get different ids.
///
/// Public because a plugin that builds a placeholder's `NodeSpec` itself -
/// `plugins/rust`'s re-export node - has to spell the label the same way, and
/// a second copy of the rendering is a second thing to keep in step.
pub fn render_target(target: &PlaceholderTarget) -> String {
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

/// The id [`FileGraphBuilder::add_placeholder`] will give a placeholder,
/// derived from the only three things it depends on - and nothing else.
///
/// A placeholder's `name` and `range` describe one *use site*, and an address
/// reached from several sites has several of those. A caller that has to
/// choose between them - the LSP bridge, which sees them one server answer at
/// a time - needs the id before it has finished choosing, so it gets it from
/// here rather than by adding a node it would then have to revise.
pub fn placeholder_id(file: &RelPath, kind: PlaceholderKind, target: &PlaceholderTarget) -> String {
    node_id(file.as_str(), NodeKind::Module, &render_target(target), Some(kind.native_kind()))
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
                key_path: None,
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
                key_path: None,
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
                key_path: None,
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
                key_path: None,
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

    /// **GM-378.** And the id is that address and nothing else, so a caller
    /// can know it before it has decided which use site the row describes -
    /// which is what [`placeholder_id`] is for.
    #[test]
    fn a_placeholders_id_is_its_address_whatever_row_it_ends_up_carrying() {
        let file = RelPath::new("src/a.toy");
        let target = PlaceholderTarget {
            scope: TargetScope::Container("pkg".into()),
            key: TargetKey::Name("helper".into()),
            from_container: None,
            key_path: None,
        };
        for kind in
            [PlaceholderKind::PendingSymbol, PlaceholderKind::Reexport, PlaceholderKind::ResolvedModule]
        {
            let mut graph = builder();
            let first = graph.add_placeholder(kind, "helper", target.clone(), range(1, 1));
            let mut graph = builder();
            // A different name, a different range - the same node.
            let second = graph.add_placeholder(kind, "helper::inner", target.clone(), range(9, 9));
            assert_eq!(first, second, "{kind:?}: the row moved the id");
            assert_eq!(
                first,
                placeholder_id(&file, kind, &target),
                "{kind:?}: `placeholder_id` must derive what `add_placeholder` assigns"
            );
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
                    key_path: None,
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

    /// `with_path` sets `qualifiedName` to the joined path, so the id is the
    /// one `new` gives for that string; aliases ride along to the wire node.
    #[test]
    fn a_node_built_from_a_path_keeps_the_id_of_its_display_string() {
        let path = QualifiedPath::root("m").child("::", "<S as Read>").child("::", "read");
        let alias = QualifiedPath::root("m").child("::", "S").child("::", "read");
        let mut graph = builder();
        let id = graph.add_node(
            NodeSpec::with_path(NodeKind::Function, "read", path.clone(), range(1, 2)).alias(alias.clone()),
        );
        assert_eq!(id, crate::ids::node_id("src/a.toy", NodeKind::Function, "m::<S as Read>::read", None));
        let graph = graph.finish();
        assert_eq!(graph.nodes[0].qualified_name, "m::<S as Read>::read");
        assert_eq!(graph.nodes[0].qualified_path, Some(path));
        assert_eq!(graph.nodes[0].alias_paths, vec![alias]);
        assert_eq!(graph.nodes[0].check_qualified_path(), Ok(()));
    }

    /// An open site in `from_id` named `name`, of `kind`, replacing `replaces`.
    fn site(from_id: &str, name: &str, kind: OpenSiteKind, replaces: Option<&str>) -> OpenSite {
        OpenSite {
            from_id: from_id.to_string(),
            position: Position { line: 1, col: 0 },
            name: name.to_string(),
            kind,
            edge_kind: EdgeKind::Calls,
            from_container: None,
            replaces: replaces.map(str::to_string),
        }
    }

    /// A file node, a function and a type, each with untyped receiver calls,
    /// plus a typed receiver call and an unresolved reference in the function.
    fn graph_with_open_sites(opt_in: bool) -> (FileGraph, String, String, String) {
        let mut graph = builder();
        if opt_in {
            graph.record_untyped_receiver_calls();
        }
        let file = graph.file_node(range(0, 9));
        let function = graph.add_node(NodeSpec::new(NodeKind::Function, "f", "f", range(1, 4)));
        let ty = graph.add_node(NodeSpec::new(NodeKind::Type, "T", "T", range(5, 8)));
        for open in [
            site(&function, "zeta", OpenSiteKind::ReceiverCall, None),
            site(&function, "alpha", OpenSiteKind::ReceiverCall, None),
            site(&function, "zeta", OpenSiteKind::ReceiverCall, None),
            site(&function, "typed", OpenSiteKind::ReceiverCall, Some("e-typed")),
            site(&function, "bare", OpenSiteKind::Reference, None),
            site(&file, "top", OpenSiteKind::ReceiverCall, None),
            site(&ty, "inner", OpenSiteKind::ReceiverCall, None),
        ] {
            graph.open_site(open);
        }
        (graph.finish(), file, function, ty)
    }

    fn untyped_of<'g>(graph: &'g FileGraph, id: &str) -> &'g [String] {
        &graph.nodes.iter().find(|node| node.id == id).unwrap().untyped_calls
    }

    /// Reporting is opt-in, so a plugin that has not opted in sends
    /// no `untypedCalls`, which core reads as "not reported". The open sites
    /// themselves are untouched. Control: default `record_untyped` to `true`
    /// in `FileGraphBuilder::new` (`f` gets names).
    #[test]
    fn without_the_opt_in_no_node_carries_untyped_calls() {
        let (graph, _file, _function, _ty) = graph_with_open_sites(false);

        assert!(graph.nodes.iter().all(|node| node.untyped_calls.is_empty()), "{:#?}", graph.nodes);
        assert_eq!(graph.open_sites.len(), 7, "the sites still go to the semantic engine");
    }

    /// With the opt-in, each untyped receiver call lands on its `from_id`
    /// node, sorted and deduplicated; a typed one (with `replaces`), a site
    /// of another kind, and a site in a node that is neither `File` nor
    /// `Function` do not. Controls: drop `names.sort()` (`zeta` first);
    /// drop `names.dedup()` (`zeta` twice); drop `site.replaces.is_none()`
    /// (`typed` appears); drop the `ReceiverCall` check (`bare` appears);
    /// drop the `File | Function` check (`T` gets `inner`).
    #[test]
    fn the_opt_in_folds_untyped_receiver_calls_onto_their_file_or_function_node() {
        let (graph, file, function, ty) = graph_with_open_sites(true);

        assert_eq!(untyped_of(&graph, &function), ["alpha", "zeta"]);
        assert_eq!(untyped_of(&graph, &file), ["top"]);
        assert!(untyped_of(&graph, &ty).is_empty(), "a Type node takes none: {:?}", untyped_of(&graph, &ty));
        assert_eq!(graph.open_sites.len(), 7, "folding keeps the sites for the semantic engine");
    }
}
