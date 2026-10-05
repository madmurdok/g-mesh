//! The per-file draft graph: nodes and edges by id, in insertion order, plus
//! every declaration written for each node.
//!
//! The draft exists because nodes change after they are created: an
//! `export { name }` below a declaration makes it public, and a node written
//! several times (overloads, a merged interface or namespace) only learns its
//! own range, signature and doc comment once the last declaration is seen.
//! [`crate::extractor::emit`] flushes the settled draft into the SDK's
//! builder once, preserving insertion order.
//!
//! Declaration lists follow ADR 0024
//! (`docs/adr/0024-semantic-tier-refines-by-binding-a-declaration.md`).

use std::collections::{HashMap, HashSet};

use g_mesh_plugin_sdk::ids::{edge_id, node_id};
use g_mesh_plugin_sdk::wire::{
    EdgeKind, NodeKind, PathSegment, PlaceholderTarget, Position, Range, WireDeclaration,
};

use g_mesh_plugin_sdk::{OpenSite, RelPath};
use tree_sitter::Node;

use crate::extractor::keys::{is_placeholder_kind, is_sendable_path, qualify, MemberSeparator};
use crate::extractor::scope::Scope;

/// One local name an import binds to a file of this project: a name this
/// file uses and another file declares.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportBinding {
    /// The project file the specifier resolved to.
    pub target_path: RelPath,
    /// The name that file exports: the pre-alias one, or `default`. For a
    /// namespace import, the local name.
    pub imported_name: String,
    /// Where the local name is bound; a placeholder for it spans this.
    pub at: Range,
}

/// A file-level `const`'s initializer, kept unfolded: folding it may need
/// another constant declared anywhere in the file.
#[derive(Debug, Clone)]
pub struct ConstantInitializer<'t> {
    pub value: Node<'t>,
    /// The scope the initializer is written in, which decides the names it
    /// reaches.
    pub scope: Scope,
}

/// How a call names its callee.
#[derive(Debug, Clone, PartialEq)]
pub enum CallReceiver {
    /// `f()`.
    None,
    /// `this.m()`.
    This,
    /// `super.m()`, and `super()` as a call of `constructor`.
    Super,
    /// `Owner.m()`: the receiver is a bare identifier, which may name a type
    /// or namespace of this file, or any other value.
    Qualified(String),
    /// `new Owner()`, a call of `constructor`; resolved as `Owner.constructor()`
    /// but never a receiver call.
    New(String),
}

impl CallReceiver {
    /// The identifier `Owner.m()` and `new Owner()` name their owner by.
    pub fn owner(&self) -> Option<&str> {
        match self {
            CallReceiver::Qualified(owner) | CallReceiver::New(owner) => Some(owner),
            CallReceiver::None | CallReceiver::This | CallReceiver::Super => None,
        }
    }
}

/// A call written in the walk, resolved once every declaration of the file
/// is known.
#[derive(Debug, Clone)]
pub struct PendingCall<'t> {
    /// The callee's name: the function, the member, or `constructor`.
    pub name: String,
    pub receiver: CallReceiver,
    pub scope: Scope,
    /// The callee's name token: where an open site about this call points.
    pub at: Node<'t>,
}

/// A class's or interface's heritage name, resolved once every declaration
/// and import of the file is known.
#[derive(Debug, Clone)]
pub struct PendingSupertype {
    /// The subtype.
    pub from_id: String,
    pub name: String,
    /// The scope the subtype is declared in.
    pub scope: Scope,
}

/// An `<identifier>.<property>` site, kept until every import is known: it is
/// a question for the semantic tier when the identifier is a namespace import
/// of a project file.
#[derive(Debug, Clone)]
pub struct PendingMemberAccess<'t> {
    pub object_name: String,
    /// The property token.
    pub at: Node<'t>,
    pub scope: Scope,
    /// Written as the callee of a call.
    pub is_call: bool,
}

/// A call that produced a `CALLS` edge, kept until the declaration lists are
/// settled: only a call onto an overload set or a placeholder is a question.
#[derive(Debug, Clone)]
pub struct CallSite {
    pub from_id: String,
    pub to_id: String,
    pub name: String,
    pub position: Position,
}

/// A name used in the walk, resolved once every declaration of the file is
/// known.
#[derive(Debug, Clone)]
pub struct PendingReference {
    pub name: String,
    pub scope: Scope,
    /// Written where only a type can go, the one place a type parameter can
    /// shadow the name.
    pub type_position: bool,
}

/// What a declaration (or placeholder) asks the model to add.
#[derive(Debug, Clone)]
pub struct NodeParams {
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    /// Declarations only, always joining to `qualified_name`.
    pub qualified_path: Option<Vec<PathSegment>>,
    /// The span of the syntax node the declaration is, in wire units.
    pub range: Range,
    /// Whether that syntax node carries an implementation.
    pub has_body: bool,
    pub native_kind: Option<String>,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
    pub exported: bool,
    /// Placeholders only.
    pub target: Option<PlaceholderTarget>,
}

impl NodeParams {
    /// A node with no native kind, signature, doc comment or target.
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
            qualified_path: None,
            range,
            has_body: false,
            native_kind: None,
            signature: None,
            doc_comment: None,
            exported: false,
            target: None,
        }
    }
}

/// A node as the walk has settled it so far.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftNode {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub qualified_name: String,
    /// Present only when [`is_sendable_path`] accepts it.
    pub qualified_path: Option<Vec<PathSegment>>,
    pub range: Range,
    /// `public` on the wire when set, else `file`.
    pub exported: bool,
    pub native_kind: Option<String>,
    pub signature: Option<String>,
    pub doc_comment: Option<String>,
    pub target: Option<PlaceholderTarget>,
    /// Set only for a node with two or more declarations.
    pub declarations: Option<Vec<WireDeclaration>>,
}

/// An edge of the draft. `resolved` is decided by its target: `false` onto a
/// placeholder, `true` onto anything declared in this file.
#[derive(Debug, Clone, PartialEq)]
pub struct DraftEdge {
    pub id: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: EdgeKind,
    pub resolved: bool,
}

/// One declaration of a node, before it is known whether the node has more.
#[derive(Debug, Clone)]
struct DeclarationDraft {
    range: Range,
    signature: Option<String>,
    doc_comment: Option<String>,
    has_body: bool,
}

/// One file's draft graph. Node 0 is the `File` node.
#[derive(Debug)]
pub struct FileModel {
    path: String,
    nodes: Vec<DraftNode>,
    node_index: HashMap<String, usize>,
    edges: Vec<DraftEdge>,
    edge_ids: HashSet<String>,
    declarations: HashMap<String, Vec<DeclarationDraft>>,
    /// Declared symbols only, first declaration wins: placeholders never
    /// shadow a real name.
    by_qualified_name: HashMap<String, usize>,
    /// The questions left for a semantic tier, in the order they are asked.
    open_sites: Vec<OpenSite>,
}

impl FileModel {
    /// A model holding only the `File` node: `name` the basename,
    /// `qualifiedName` the path, `range` the whole parse.
    pub fn new(path: &str, range: Range) -> Self {
        let mut model = Self {
            path: path.to_string(),
            nodes: Vec::new(),
            node_index: HashMap::new(),
            edges: Vec::new(),
            edge_ids: HashSet::new(),
            declarations: HashMap::new(),
            by_qualified_name: HashMap::new(),
            open_sites: Vec::new(),
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        model.add_node(NodeParams::new(NodeKind::File, name, path, range));
        model
    }

    /// The `File` node's id.
    pub fn file_id(&self) -> &str {
        &self.nodes[0].id
    }

    /// The node at `index`.
    pub fn node(&self, index: usize) -> &DraftNode {
        &self.nodes[index]
    }

    /// The node with `id`, if this file has one.
    pub fn node_by_id(&self, id: &str) -> Option<&DraftNode> {
        self.node_index.get(id).map(|index| &self.nodes[*index])
    }

    /// Adds a node, or - when one with the same id exists - records another
    /// declaration of it. A second declaration changes nothing on the node
    /// except that an exported one makes it public; the node's own fields are
    /// settled by [`fill_declaration_lists`](Self::fill_declaration_lists).
    pub fn add_node(&mut self, params: NodeParams) -> usize {
        let id = node_id(&self.path, params.kind, &params.qualified_name, params.native_kind.as_deref());
        self.record_declaration(&id, &params);
        if let Some(&index) = self.node_index.get(&id) {
            if params.exported {
                self.nodes[index].exported = true;
            }
            return index;
        }
        let qualified_path = params.qualified_path.filter(|path| is_sendable_path(path, &params.name));
        let index = self.nodes.len();
        self.nodes.push(DraftNode {
            id: id.clone(),
            kind: params.kind,
            name: params.name,
            qualified_name: params.qualified_name,
            qualified_path,
            range: params.range,
            exported: params.exported,
            native_kind: params.native_kind,
            signature: params.signature,
            doc_comment: params.doc_comment,
            target: params.target,
            declarations: None,
        });
        self.node_index.insert(id, index);
        index
    }

    /// Files one declaration under node `id`. Placeholders record none: the
    /// sites that ask for one are uses, not declarations.
    fn record_declaration(&mut self, id: &str, params: &NodeParams) {
        if is_placeholder_kind(params.native_kind.as_deref()) {
            return;
        }
        self.declarations.entry(id.to_string()).or_default().push(DeclarationDraft {
            range: params.range,
            signature: params.signature.clone(),
            doc_comment: params.doc_comment.clone(),
            has_body: params.has_body,
        });
    }

    /// Hangs the declaration list off every node with two or more
    /// declarations and settles that node's own fields:
    ///
    /// - `declarations` sorted by start position, `ordinal` from 0;
    /// - range: the first declaration with a body, else the first;
    /// - signature: the first bodiless declaration that has one, else the
    ///   first declaration's;
    /// - doc comment: the first declaration that has one.
    ///
    /// A node with one declaration is left exactly as built.
    pub fn fill_declaration_lists(&mut self) {
        for (id, drafts) in &self.declarations {
            if drafts.len() < 2 {
                continue;
            }
            let Some(&index) = self.node_index.get(id) else { continue };
            let mut ordered: Vec<&DeclarationDraft> = drafts.iter().collect();
            ordered.sort_by_key(|draft| (draft.range.start.line, draft.range.start.col));
            let node = &mut self.nodes[index];
            node.declarations = Some(
                ordered
                    .iter()
                    .enumerate()
                    .map(|(ordinal, draft)| WireDeclaration {
                        ordinal: ordinal as u32,
                        start_line: draft.range.start.line,
                        start_col: draft.range.start.col,
                        end_line: draft.range.end.line,
                        end_col: draft.range.end.col,
                        signature: draft.signature.clone(),
                        has_body: draft.has_body,
                    })
                    .collect(),
            );
            let primary = ordered.iter().find(|draft| draft.has_body).unwrap_or(&ordered[0]);
            node.range = primary.range;
            let call_signature = ordered.iter().find(|draft| !draft.has_body && draft.signature.is_some());
            if let Some(signature) = &call_signature.unwrap_or(&ordered[0]).signature {
                node.signature = Some(signature.clone());
            }
            if let Some(documented) = ordered.iter().find(|draft| draft.doc_comment.is_some()) {
                node.doc_comment = documented.doc_comment.clone();
            }
        }
    }

    /// Adds a symbol declared in this file: indexed for name lookup, with
    /// `DEFINES` from the file and, when public, `EXPORTS`.
    pub fn declare_symbol(&mut self, params: NodeParams) -> usize {
        let index = self.add_node(params);
        let qualified_name = self.nodes[index].qualified_name.clone();
        self.by_qualified_name.entry(qualified_name).or_insert(index);
        let file_id = self.file_id().to_string();
        let id = self.nodes[index].id.clone();
        self.add_edge(&file_id, EdgeKind::Defines, &id);
        if self.nodes[index].exported {
            self.add_edge(&file_id, EdgeKind::Exports, &id);
        }
        index
    }

    /// Adds an edge between two nodes of this file. An edge whose ends are not
    /// both nodes is dropped (no dangling edge), and an edge already present
    /// is not added twice.
    pub fn add_edge(&mut self, from_id: &str, kind: EdgeKind, to_id: &str) {
        let Some(target) = self.node_by_id(to_id) else { return };
        if !self.node_index.contains_key(from_id) {
            return;
        }
        let resolved = !is_placeholder_kind(target.native_kind.as_deref());
        let id = edge_id(from_id, kind, to_id, None);
        if !self.edge_ids.insert(id.clone()) {
            return;
        }
        self.edges.push(DraftEdge {
            id,
            from_id: from_id.to_string(),
            to_id: to_id.to_string(),
            kind,
            resolved,
        });
    }

    /// Whether the edge `from_id -kind-> to_id` is already in the draft.
    pub fn has_edge(&self, from_id: &str, kind: EdgeKind, to_id: &str) -> bool {
        self.edge_ids.contains(&edge_id(from_id, kind, to_id, None))
    }

    /// The declared symbol whose `qualifiedName` is exactly `qualified_name`.
    pub fn lookup_qualified(&self, qualified_name: &str) -> Option<usize> {
        self.by_qualified_name.get(qualified_name).copied()
    }

    /// Makes node `index` public and adds its `EXPORTS` edge.
    pub fn mark_exported(&mut self, index: usize) {
        self.nodes[index].exported = true;
        let file_id = self.file_id().to_string();
        let id = self.nodes[index].id.clone();
        self.add_edge(&file_id, EdgeKind::Exports, &id);
    }

    /// The declared symbol `name` refers to from inside namespace
    /// `namespace_prefix`: innermost namespace outwards, ending at the module
    /// root, optionally only of `kind`.
    pub fn lookup_by_name(
        &self,
        name: &str,
        namespace_prefix: &str,
        kind: Option<NodeKind>,
    ) -> Option<usize> {
        let mut prefix = namespace_prefix;
        loop {
            if let Some(&index) = self.by_qualified_name.get(&qualify(prefix, name, MemberSeparator::Dot)) {
                if kind.is_none_or(|kind| self.nodes[index].kind == kind) {
                    return Some(index);
                }
            }
            if prefix.is_empty() {
                return None;
            }
            prefix = prefix.rfind('.').map_or("", |at| &prefix[..at]);
        }
    }

    /// Records a question for a semantic tier.
    pub fn add_open_site(&mut self, site: OpenSite) {
        self.open_sites.push(site);
    }

    /// The recorded open sites, in the order they were added.
    pub fn take_open_sites(&mut self) -> Vec<OpenSite> {
        std::mem::take(&mut self.open_sites)
    }

    /// The nodes and edges, in insertion order.
    pub fn into_parts(self) -> (Vec<DraftNode>, Vec<DraftEdge>) {
        (self.nodes, self.edges)
    }
}
