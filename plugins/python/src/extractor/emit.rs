//! The one place this plugin writes to the graph: a thin layer over the
//! SDK's [`FileGraphBuilder`] that adds the two things a Python extractor
//! cannot do without - **de-duplication by id** and **character columns**.
//!
//! # Why de-duplication is a correctness requirement, not tidying
//!
//! (This is the emission half of [`super`]'s Decision 6, "conditional
//! definitions and conditional imports": that decision says every branch is
//! indexed and no predicate is evaluated; this module is where two branches
//! that produce one id become one node rather than two lines.)
//!
//! A node's id is `(filePath, kind, qualifiedName, nativeKind)` and an edge's
//! is `(fromId, kind, toId)`. Python writes the same id twice in code nobody
//! would call unusual:
//!
//!  - **A conditional definition.** `if sys.version_info >= (3, 11): def
//!    load(): …` / `else: def load(): …` is the ordinary way a Python file
//!    supports two runtimes, and it is the same name, in the same file, at
//!    the same lexical depth - the direct analogue of Rust's `cfg`
//!    alternatives, and treated the same way: every branch is indexed, no
//!    predicate is evaluated.
//!  - **A rebinding.** `def f(): …` followed later by `f = memoize(f)`, or a
//!    second `def f` that shadows the first, is legal Python that produces
//!    one name.
//!  - **Repeated calls.** `f(); f()` in one function is one `CALLS` edge
//!    written twice.
//!  - **An import and a use through it.** `from a.b import C` emits a
//!    placeholder addressed at `(container a.b, name C)`, and `C()` later in
//!    the file addresses the very same thing - by construction, since the SDK
//!    derives a placeholder's id from its target so that two placeholders
//!    waiting on one thing *are* one node.
//!
//! Emitting the duplicate would put two lines with one id into the bulk
//! stream and two entries into a `fileChanged` diff's `upsertNodes`. Nothing
//! downstream is wrong afterwards - `apply_diff` upserts by id - but the
//! stream stops being a faithful description of the file, and the second
//! branch silently replaces the first rather than merging with it.
//! [`Emitter`] answers the honest version of the same thing: the **first**
//! occurrence in source order is emitted, every later one resolves to that
//! same id, and edges written from inside the second branch land on the node
//! the first produced.
//!
//! # Character columns
//!
//! tree-sitter reports a column as a **byte** offset within its line. Every
//! other position this plugin emits - the file's own end, which is computed
//! from the source text - counts **characters**, and so does the SDK's toy
//! plugin. Mixing the two would put a declaration's column and its file's
//! column in different units on any line containing a non-ASCII character,
//! which for Python is not an exotic case at all: identifiers may be
//! non-ASCII, and docstrings routinely are. [`Positions`] converts once,
//! against a precomputed table of line starts.
//!
//! # Overload sets
//!
//! A `typing.overload` set is the one same-id redefinition that is not a
//! choice between alternatives: every `@overload` stub plus the
//! implementation *is* the function. The node row still follows the
//! first-wins rule above (range and signature of the first stub, as
//! TypeScript's "first call signature" rule), and the set as a whole travels
//! as the node's `declarations` - every `def` of that id, in source order,
//! ordinal from 0, `hasBody` only on the one not decorated `@overload`. A
//! set with no `@overload` in it is a conditional definition or a rebinding
//! and gets no list. The list is what a semantic engine binds an overloaded
//! call to by ordinal; see
//! `docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`.
//!
//! # Property accessors
//!
//! `@property def x`, `@x.setter def x` and `@x.deleter def x` are three
//! functions of one property, not a redefinition, so they are three nodes:
//! the same name, `qualifiedName` and container, told apart by `nativeKind`
//! (`method` for the getter, whose id is unchanged, then `setter` and
//! `deleter`), each with its own range, signature and docstring. The setter
//! and deleter are kept out of the name tables (`super::model`'s accessor
//! table), so a name lookup still finds the getter alone; only an
//! instance-parameter store (`self.x = v`), `del self.x` or `self.x += v`
//! is routed to them (`super::bodies`). See
//! `docs/architecture/gm-511-python-property-accessors.md`.

use std::collections::{HashMap, HashSet};

use g_mesh_plugin_sdk::ids::{edge_id, node_id};
use g_mesh_plugin_sdk::wire::{
    EdgeKind, NodeKind, PlaceholderTarget, Position, Range, TargetKey, TargetScope, WireDeclaration,
};
use g_mesh_plugin_sdk::{FileGraph, FileGraphBuilder, NodeSpec, OpenSite, PlaceholderKind, RelPath};
use tree_sitter::Node;

/// Byte columns in, character columns out.
pub(crate) struct Positions<'s> {
    source: &'s str,
    /// The byte offset each line starts at, indexed by line number.
    line_starts: Vec<usize>,
}

impl<'s> Positions<'s> {
    pub(crate) fn new(source: &'s str) -> Self {
        let mut line_starts = vec![0usize];
        line_starts.extend(source.match_indices('\n').map(|(at, _)| at + 1));
        Self { source, line_starts }
    }

    /// One tree-sitter point, in the wire's units.
    pub(crate) fn at(&self, point: tree_sitter::Point) -> Position {
        let line = point.row;
        let col = self
            .line_starts
            .get(line)
            .and_then(|start| self.source.get(*start..start + point.column))
            .map_or(point.column, |prefix| prefix.chars().count());
        Position { line: line as u32, col: col as u32 }
    }

    /// A node's whole range.
    pub(crate) fn range(&self, node: Node) -> Range {
        Range { start: self.at(node.start_position()), end: self.at(node.end_position()) }
    }

    /// The file's own range, ending where its content ends: trailing
    /// whitespace is trimmed, and the end is `(number of newlines, length of
    /// the last line)` of what is left - the file's last real line, never
    /// the empty line after a final newline.
    ///
    /// Not tree-sitter's root range, and not a byte count: this is the one
    /// formula a space inserted before the file's last newline does not move,
    /// which is what `id-stability.whitespace-edit` asks of every plugin (see
    /// `core::cli::plugin_check::session::whitespace_edit` for why that
    /// particular edit), which also requires that end line. It is
    /// [`FileGraphBuilder::file_node`]'s own documented contract, called
    /// here because this plugin builds its `File` node by hand - see
    /// [`Emitter::new`]. The `Module` declaration takes the same range.
    pub(crate) fn file_range(&self) -> Range {
        g_mesh_plugin_sdk::CharColumns::new(self.source).file_range()
    }
}

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
pub(crate) struct Emitter<'s> {
    graph: FileGraphBuilder,
    path: RelPath,
    positions: Positions<'s>,
    file_id: String,
    nodes: HashSet<String>,
    edges: HashSet<String>,
    placeholders: HashMap<PlaceholderKey, String>,
    /// Every function definition of each node id, in source order - see
    /// "Overload sets" in the module doc.
    definitions: HashMap<String, Vec<Definition>>,
    /// Re-export nodes, held back until [`Emitter::finish`] so a repeat of
    /// one (`from .b import *` written twice) can move its range to the
    /// later statement: core reads a re-export's start position as its
    /// statement's order. Safe to emit late because a re-export
    /// node is the source or target of no edge. Indexed by id in
    /// `reexport_at`.
    reexports: Vec<NodeSpec>,
    reexport_at: HashMap<String, usize>,
}

/// One `def` of a function node, as an overload set's declaration list needs
/// it.
#[derive(Debug, Clone)]
pub(crate) struct Definition {
    /// The whole definition, decorators included.
    pub(crate) range: Range,
    pub(crate) signature: Option<String>,
    /// Decorated `@overload` (or any dotted name ending in `overload`): a
    /// stub with no body of its own, whatever its `...` says syntactically.
    pub(crate) overload: bool,
}

impl<'s> Emitter<'s> {
    /// Starts a file's graph with its `File` node, which must be first: every
    /// `DEFINES` edge starts there, and an edge may only name a node already
    /// emitted.
    ///
    /// The node is built by hand rather than through
    /// [`FileGraphBuilder::file_node`] for one reason: a Python module's
    /// docstring documents the module, and while this plugin *does* emit a
    /// node for the module itself (the self-announcement of
    /// `crate::project`'s Decision 1), that node only exists for a module
    /// with a parent package - so the `File` node is the one place a
    /// top-level module's docstring can always hang. Everything else - the
    /// name being the last path segment, the `qualifiedName` being the path
    /// `graph::imports` links against, the default `file` visibility - is
    /// that helper's own contract, reproduced exactly.
    pub(crate) fn new(
        language: &str,
        engine: &str,
        path: &RelPath,
        source: &'s str,
        module_doc: Option<String>,
    ) -> Self {
        let positions = Positions::new(source);
        let mut graph = FileGraphBuilder::new(language, engine, path);
        let name = path.as_str().rsplit('/').next().unwrap_or(path.as_str()).to_string();
        let mut spec = NodeSpec::new(NodeKind::File, name, path.as_str(), positions.file_range());
        spec.doc_comment = module_doc;
        let file_id = graph.add_node(spec);
        Self {
            graph,
            path: path.clone(),
            positions,
            file_id: file_id.clone(),
            nodes: HashSet::from([file_id]),
            edges: HashSet::new(),
            placeholders: HashMap::new(),
            definitions: HashMap::new(),
            reexports: Vec::new(),
            reexport_at: HashMap::new(),
        }
    }

    pub(crate) fn positions(&self) -> &Positions<'s> {
        &self.positions
    }

    pub(crate) fn file_id(&self) -> &str {
        &self.file_id
    }

    /// Adds a declaration, with its `DEFINES` edge (and `EXPORTS`, which in
    /// Python is every declaration - see `super::keys`' visibility section),
    /// and returns its id.
    ///
    /// A second declaration resolving to an id already emitted - a
    /// conditional definition, a rebinding - adds nothing and returns the
    /// first one's id; see the module doc.
    pub(crate) fn declare(&mut self, spec: NodeSpec, exported: bool) -> String {
        let id = node_id(self.path.as_str(), spec.kind, &spec.qualified_name, spec.native_kind.as_deref());
        if self.nodes.insert(id.clone()) {
            self.graph.add_node(spec);
            self.graph.defines(&self.file_id.clone(), &id, exported);
        }
        id
    }

    /// Records one function definition of node `id` - called for every
    /// `def`, the first and every same-id one after it, in source order.
    pub(crate) fn definition(&mut self, id: &str, definition: Definition) {
        self.definitions.entry(id.to_string()).or_default().push(definition);
    }

    /// Whether node `id` of this file is an overload set: one of its
    /// definitions so far is `@overload`. Complete once the declaration pass
    /// is over, which is when the body pass asks.
    pub(crate) fn overloaded(&self, id: &str) -> bool {
        self.definitions.get(id).is_some_and(|definitions| definitions.iter().any(|d| d.overload))
    }

    /// Adds (or finds) the placeholder waiting on `target`, and returns its
    /// id.
    pub(crate) fn placeholder(
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

    /// Adds a `reexport` node: what this module publishes, and what that
    /// really is.
    ///
    /// # Why this is not [`Emitter::placeholder`]
    ///
    /// A re-export is the one placeholder kind that is a node of its
    /// **container** as well as of its file. `graph::symbol_links` finds a
    /// container scope's re-exports by `(language, container)`, so a
    /// re-export without one is invisible to every lookup addressed at the
    /// module that publishes it - which for Python is the whole point: `from
    /// pkg import Thing`, where `pkg/__init__.py` lists `Thing` in `__all__`,
    /// is a lookup addressed at container `pkg`. The SDK's `add_placeholder`
    /// builds its `NodeSpec` internally and sets no container, so this builds
    /// the spec itself.
    ///
    /// It sets no `containerParent`: a re-export is not a *member*
    /// (`graph::containers` excludes every placeholder kind from membership,
    /// which is what keeps it out of `memberCount`), and a parent is only
    /// ever read from a member's record.
    ///
    /// The `qualifiedName` names the published name as well as the address,
    /// where the SDK's own rendering would name only the address. A package
    /// whose `__init__` re-exports one declaration under two names - `from
    /// .mod import Thing` plus `Alias = Thing` in `__all__` - states two
    /// different facts about what it publishes, and an id derived from the
    /// address alone would make them one node and lose the second name.
    pub(crate) fn reexport(
        &mut self,
        published: &str,
        target: PlaceholderTarget,
        container: &str,
        range: Range,
    ) -> String {
        let qualified_name = format!("{} as {published}", render_target(&target));
        let id = node_id(
            self.path.as_str(),
            NodeKind::Module,
            &qualified_name,
            Some(PlaceholderKind::Reexport.native_kind()),
        );
        if let Some(&at) = self.reexport_at.get(&id) {
            // The same re-export again: the later statement is the one that
            // last bound the name, so it is the one whose position counts.
            self.reexports[at].range = range;
        } else if self.nodes.insert(id.clone()) {
            let mut spec = NodeSpec::new(NodeKind::Module, published, qualified_name, range)
                .native_kind(PlaceholderKind::Reexport.native_kind());
            spec.container = Some(container.to_string());
            spec.target = Some(target);
            self.reexport_at.insert(id.clone(), self.reexports.len());
            self.reexports.push(spec);
        }
        id
    }

    /// Adds (or finds) the `external_module` node for a dotted name this
    /// project does not contain, and returns its id. See
    /// `crate::project`'s Decision 8 for how "does not contain" is decided.
    pub(crate) fn external_module(&mut self, name: &str, range: Range) -> String {
        let id = node_id(self.path.as_str(), NodeKind::Module, name, Some("external_module"));
        if self.nodes.insert(id.clone()) {
            self.graph.add_external_module(name, range);
        }
        id
    }

    /// An edge onto a declaration of this same file: `resolved: true`, since
    /// within one file nothing is left for core to confirm. Returns its id,
    /// which a repeat shares.
    pub(crate) fn resolved_edge(&mut self, kind: EdgeKind, from: &str, to: &str) -> String {
        let id = edge_id(from, kind, to, None);
        if self.edges.insert(id.clone()) {
            self.graph.resolved_edge(kind, from, to);
        }
        id
    }

    /// An edge onto a placeholder: `resolved: false`, since only core can
    /// confirm it. Returns its id, which a repeat shares.
    pub(crate) fn placeholder_edge(&mut self, kind: EdgeKind, from: &str, to: &str) -> String {
        let id = edge_id(from, kind, to, None);
        if self.edges.insert(id.clone()) {
            self.graph.placeholder_edge(kind, from, to);
        }
        id
    }

    /// An `IMPORTS` edge onto a placeholder, carrying the import's text as
    /// written as its `specifier` (GM-544): what core matches a resolution
    /// delta's `Specifier` selectors against. When two imports draw the same
    /// edge, the first one's text is kept; the id does not include it.
    pub(crate) fn import_edge(&mut self, from: &str, to: &str, specifier: &str) -> String {
        let id = edge_id(from, EdgeKind::Imports, to, None);
        if self.edges.insert(id.clone()) {
            self.graph.import_edge(from, to, specifier);
        }
        id
    }

    /// Records a use site the structural tier could not settle - see
    /// `super`'s module doc, Decision 7.
    pub(crate) fn open_site(&mut self, site: OpenSite) {
        self.graph.open_site(site);
    }

    /// Marks every node of this file as having come from a source with a
    /// syntax error. A normal answer, not a failure.
    pub(crate) fn mark_syntax_errors(&mut self) {
        self.graph.mark_syntax_errors();
    }

    /// The graph, with each overload set's `declarations` attached. Only
    /// here, because a set's list is complete only after its last `def`, and
    /// its node was pushed at the first.
    pub(crate) fn finish(mut self) -> FileGraph {
        for (id, definitions) in std::mem::take(&mut self.definitions) {
            if !definitions.iter().any(|definition| definition.overload) {
                continue;
            }
            let declarations = definitions
                .into_iter()
                .enumerate()
                .map(|(ordinal, definition)| WireDeclaration {
                    ordinal: ordinal as u32,
                    start_line: definition.range.start.line,
                    start_col: definition.range.start.col,
                    end_line: definition.range.end.line,
                    end_col: definition.range.end.col,
                    signature: definition.signature,
                    has_body: !definition.overload,
                })
                .collect();
            self.graph.set_declarations(&id, declarations);
        }
        for spec in std::mem::take(&mut self.reexports) {
            self.graph.add_node(spec);
        }
        self.graph.finish()
    }
}

/// The label a placeholder's `qualifiedName` carries, in the SDK's own
/// rendering (`FileGraphBuilder::add_placeholder`: "`<container>::<key>` for
/// a container scope").
///
/// Reproduced here rather than imported because the SDK keeps its renderer
/// private, and only one node kind needs it - see [`Emitter::reexport`], the
/// one placeholder this plugin has to build a `NodeSpec` for itself. It is a
/// *label*, not an address: core reads the structured `target` row, and all
/// this has to be is injective enough that two placeholders of one file
/// waiting on different things get different ids.
fn render_target(target: &PlaceholderTarget) -> String {
    let key = match &target.key {
        TargetKey::Name(name) => name,
        TargetKey::QualifiedName(qualified_name) => qualified_name,
    };
    match &target.scope {
        TargetScope::File(path) => format!("{path}#{key}"),
        TargetScope::Container(container) => format!("{container}::{key}"),
    }
}

/// A placeholder target addressed at a container, which is every target this
/// plugin builds: Python's cross-file addressing unit is the module, never
/// the file. `from pkg.sub.mod import f` names a *module path* that the
/// import system resolves to a file through `sys.path`, a namespace package's
/// several directories, or a zip importer - so the file is exactly the thing
/// an import statement does not name.
pub(crate) fn container_target(container: &str, key: TargetKey, from_container: &str) -> PlaceholderTarget {
    PlaceholderTarget {
        scope: TargetScope::Container(container.to_string()),
        key,
        from_container: Some(from_container.to_string()),
        key_path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use g_mesh_plugin_sdk::wire::Visibility;

    fn emitter(source: &'static str) -> Emitter<'static> {
        Emitter::new("python", "tree-sitter", &RelPath::new("pkg/mod.py"), source, None)
    }

    fn function(name: &str) -> NodeSpec {
        NodeSpec::new(
            NodeKind::Function,
            name,
            name,
            Range { start: Position { line: 1, col: 0 }, end: Position { line: 1, col: 1 } },
        )
        .native_kind("function")
        .visibility(Visibility::Public)
    }

    /// Two conditional definitions of one name are one node, and the first
    /// one wins - see the module doc.
    #[test]
    fn a_second_declaration_of_one_id_adds_nothing_and_reuses_the_first() {
        let mut emitter = emitter("def f():\n    pass\n");
        let first = emitter.declare(function("f"), true);
        let second = emitter.declare(function("f"), true);
        assert_eq!(first, second);
        let graph = emitter.finish();
        assert_eq!(graph.nodes.len(), 2, "the File node and one `f`: {:?}", graph.nodes);
        assert_eq!(graph.edges.len(), 2, "one DEFINES and one EXPORTS: {:?}", graph.edges);
    }

    #[test]
    fn one_edge_written_twice_is_one_edge() {
        let mut emitter = emitter("def f():\n    pass\n");
        let id = emitter.declare(function("f"), true);
        let file = emitter.file_id().to_string();
        emitter.resolved_edge(EdgeKind::Calls, &file, &id);
        emitter.resolved_edge(EdgeKind::Calls, &file, &id);
        let graph = emitter.finish();
        assert_eq!(graph.edges.iter().filter(|edge| edge.kind == EdgeKind::Calls).count(), 1);
    }

    #[test]
    fn two_placeholders_waiting_on_one_thing_are_one_node() {
        let mut emitter = emitter("import a\n");
        let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } };
        let a = emitter.placeholder(
            PlaceholderKind::PendingSymbol,
            "C",
            container_target("a.b", TargetKey::Name("C".into()), "pkg.mod"),
            range,
        );
        let b = emitter.placeholder(
            PlaceholderKind::PendingSymbol,
            "C",
            container_target("a.b", TargetKey::Name("C".into()), "pkg.mod"),
            range,
        );
        let other = emitter.placeholder(
            PlaceholderKind::PendingSymbol,
            "build",
            container_target("a.b", TargetKey::QualifiedName("C.build".into()), "pkg.mod"),
            range,
        );
        assert_eq!(a, b);
        assert_ne!(a, other, "a different key is a different address");
        let graph = emitter.finish();
        assert_eq!(graph.nodes.len(), 3, "{:?}", graph.nodes);
    }

    #[test]
    fn a_files_end_is_where_a_trailing_space_cannot_move_it() {
        let plain = Positions::new("def a():\n    pass\n").file_range();
        let spaced = Positions::new("def a():\n    pass \n").file_range();
        assert_eq!(plain, spaced);
    }

    /// tree-sitter counts a column in bytes; everything on the wire counts
    /// characters, and a line with a multi-byte character is where the two
    /// part company. Python allows non-ASCII identifiers, so this is not a
    /// comments-only concern.
    #[test]
    fn a_column_is_converted_from_bytes_to_characters() {
        let source = "# é é\ndef a():\n    pass\n";
        let positions = Positions::new(source);
        // `é` is two bytes, so tree-sitter's column for the end of line 0 is
        // 7 where the character count is 5.
        assert_eq!(positions.at(tree_sitter::Point { row: 0, column: 7 }), Position { line: 0, col: 5 });
        assert_eq!(positions.at(tree_sitter::Point { row: 1, column: 3 }), Position { line: 1, col: 3 });
    }
}
