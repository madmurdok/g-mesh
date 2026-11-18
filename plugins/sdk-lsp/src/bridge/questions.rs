//! Which questions a pass asks.

use std::collections::HashSet;

use g_mesh_wire::{EdgeKind, NodeKind, Position, TargetKey, TargetScope, WireDeclaration, WireNode};
use serde_json::Value;

use super::super::config::SemanticConfig;
use super::super::position::line_text;
use super::Budgets;
use super::{answers::is_addressable, hover::hover_text, landing::locations};
use g_mesh_plugin_sdk::RelPath;
use g_mesh_plugin_sdk::SdkIndex;
use g_mesh_plugin_sdk::{OpenSite, OpenSiteKind, PlaceholderKind};

/// One thing to ask the server, and what to do with the answer.
#[derive(Debug, Clone)]
pub(super) struct Question {
    /// The file the question is asked *in* - the one whose document the
    /// position refers to, and the one this answer's edges are remembered
    /// against.
    pub(super) file: RelPath,
    pub(super) position: Position,
    pub(super) ask: Ask,
}

#[derive(Debug, Clone)]
pub(super) enum Ask {
    /// An open site: where does this name come from.
    Definition(OpenSite),
    /// A declaration other declarations may implement: who implements it.
    Implementation { anchor: String },
    /// The second hop of an implementation answer: what declaration is at the
    /// site the server pointed at - see [`LspBridge`]'s doc on why a site is
    /// not a declaration.
    ///
    /// `for_file` is the *anchor's* file, not this question's, because that is
    /// where the whole sweep's answers are remembered for retraction: a later
    /// pass over the trait re-derives its implementors, and a pass over some
    /// implementor's file has no idea the sweep ever happened.
    Implementor { anchor: String, for_file: RelPath },
    /// An [`OpenSiteKind::OverloadCall`] site: which declarations of the
    /// call's overload set does the server say it reaches - see
    /// [`LspBridge`]'s doc on overload binding.
    Overload(OpenSite),
    /// The second hop of an overload answer the server did not narrow to one
    /// declaration: `hover` at the call, or at one candidate declaration's
    /// name. `pending` indexes [`Answers::pending`], which joins the hops.
    ///
    /// `for_file` is the *call's* file. A candidate's hover is asked in the
    /// file that declares it, but it is part of the call's answer: it is that
    /// file whose pass falls short when the hover does, and a declaring file
    /// is not covered just because a call elsewhere asked about one of its
    /// names.
    OverloadHover { pending: usize, at: Hovered, for_file: RelPath },
}

/// Where an [`Ask::OverloadHover`] is asked.
#[derive(Debug, Clone)]
pub(super) enum Hovered {
    /// At the call - what the server says the call binds.
    Call,
    /// At the name of declaration `ordinal` of `node` - what that overload
    /// looks like through the same printer.
    Candidate { node: String, ordinal: u32 },
}

impl Question {
    pub(super) fn method(&self) -> &'static str {
        match self.ask {
            Ask::Definition(_) | Ask::Implementor { .. } | Ask::Overload(_) => "textDocument/definition",
            Ask::Implementation { .. } => "textDocument/implementation",
            Ask::OverloadHover { .. } => "textDocument/hover",
        }
    }

    /// The file this question's outcome is accounted to: the one that is not
    /// covered when it goes unanswered, and that is covered when it is. Its
    /// own document, except for a candidate hover - see
    /// [`Ask::OverloadHover`].
    pub(super) fn accounted_to(&self) -> &RelPath {
        match &self.ask {
            Ask::OverloadHover { for_file, .. } => for_file,
            _ => &self.file,
        }
    }

    /// Whether `result` says nothing - the answer a busy server gives, which
    /// `run_pass` re-asks once after the server settles.
    pub(super) fn is_empty_answer(&self, result: &Value) -> bool {
        match self.ask {
            Ask::OverloadHover { .. } => hover_text(result).is_none(),
            _ => locations(result).is_empty(),
        }
    }
}

/// Everything this pass will ask, plus what it refused to ask and why.
pub(super) struct Questions {
    pub(super) asking: Vec<Question>,
    /// Open sites of a kind this bridge cannot answer - see [`LspBridge`]'s
    /// doc. Counted for the log, and deliberately *not* a reason to call the
    /// pass incomplete: they are outside what this engine answers by design,
    /// and a pass that stays permanently "incomplete" for a reason no retry
    /// can change would keep a language owed a pass forever.
    pub(super) unanswerable: usize,
    /// Whether [`Budgets::max_sites`] cut the list short - which, per that
    /// field's doc, now means the project is past a ceiling no measured
    /// corpus comes near rather than merely large.
    pub(super) truncated: bool,
}

/// Builds the question list for `scope`, in scope order, stopping at the last
/// **whole file** that fits under [`Budgets::max_sites`].
///
/// The file boundary is load-bearing and was not here before GM-319. A cut
/// taken mid-file leaves that file with some of its questions asked and the
/// rest silently dropped - and `run_pass` then reports it in `covered`,
/// because every question it *did* hold was sent and answered. `retract_stale`
/// reads `covered` as "this pass is the whole truth about that file now" and
/// withdraws every edge an earlier pass emitted for a site this one never
/// reached. Correct edges deleted to make room for questions nobody asked, in
/// other words, which is the one direction `LspBridge`'s retraction rules
/// exist to forbid. Cutting between files makes the hazard unreachable rather
/// than unlikely: a file is either asked about completely or not named at all,
/// and a file not named is not covered, so its earlier answers stand.
///
/// One file may exceed the ceiling on its own - a generated file, a giant
/// table - and is taken anyway when it is the first with anything to ask.
/// The alternative is a pass that asks nothing about it forever, and the
/// deadline still bounds the work either way; an overrun of one file is the
/// smaller of the two failures, and the only one that cannot cost an edge.
/// A `max_sites` of zero is the exception to the exception, and stays a way
/// to ask nothing at all.
///
/// `unanswerable` counts only the files that were admitted, for the same
/// reason: a file the ceiling never reached has not had its sites *skipped*,
/// and reporting them in that log line would describe work this pass declined
/// to consider as work it looked at and turned down.
pub(super) fn questions(
    index: &SdkIndex,
    scope: &[RelPath],
    config: &SemanticConfig,
    budgets: &Budgets,
) -> Questions {
    let mut asking: Vec<Question> = Vec::new();
    let mut unanswerable = 0usize;
    let mut truncated = false;
    // Built on the first `OverloadCall` site, once per pass.
    let mut overloaded: Option<Overloaded> = None;
    for path in scope {
        let Some(entry) = index.entry(path) else { continue };
        let mut for_file = Vec::new();
        let mut skipped = 0usize;
        let mut kept_overloads: HashSet<(String, u32, u32)> = HashSet::new();
        for site in &entry.graph.open_sites {
            let ask = match site.kind {
                OpenSiteKind::Implementation => {
                    skipped += 1;
                    continue;
                }
                OpenSiteKind::OverloadCall => {
                    // Only a call whose structural target really is an
                    // overload set is worth a question: a plugin records the
                    // site for every call it cannot rule out. A dropped site
                    // is not unanswerable - its structural edge is the whole
                    // answer - and neither is one without the `replaces` this
                    // kind requires, which has nothing to refine.
                    let Some(replaced) = &site.replaces else { continue };
                    let overloaded = overloaded.get_or_insert_with(|| Overloaded::of(index));
                    if !overloaded.targets(&entry.graph, replaced) {
                        continue;
                    }
                    kept_overloads.insert(site_key(site));
                    Ask::Overload(site.clone())
                }
                OpenSiteKind::Reference if site.replaces.is_some() => {
                    // A hop through a re-export or a default import, or a
                    // call or supertype by name onto a declaration of a kind
                    // the edge does not accept (a `CALLS` onto a `const`):
                    // asked only while its placeholder is one the linker
                    // cannot settle on its own. Like a dropped overload call,
                    // a site not asked is not unanswerable - its structural
                    // edge stands as the answer.
                    let replaced = site.replaces.as_deref().unwrap_or_default();
                    if !unsettled_hop(index, &entry.graph, replaced) {
                        continue;
                    }
                    Ask::Definition(site.clone())
                }
                OpenSiteKind::ReceiverCall | OpenSiteKind::ReceiverField | OpenSiteKind::Reference => {
                    Ask::Definition(site.clone())
                }
            };
            for_file.push(Question { file: path.clone(), position: site.position, ask });
        }
        // One question per call: a hop site at a call already kept as an
        // overload question is dropped, because the overload binding is the
        // stronger answer and two answers for one edge would leave two rows.
        // After the loop, so the order sites were recorded in does not matter.
        if !kept_overloads.is_empty() {
            for_file.retain(|question| match &question.ask {
                Ask::Definition(site) if site.kind == OpenSiteKind::Reference && site.replaces.is_some() => {
                    !kept_overloads.contains(&site_key(site))
                }
                _ => true,
            });
        }
        if !config.implementation_kinds.is_empty() {
            for node in &entry.graph.nodes {
                let implementable = node
                    .native_kind
                    .as_deref()
                    .is_some_and(|kind| config.implementation_kinds.iter().any(|want| want == kind));
                if !implementable {
                    continue;
                }
                for_file.push(Question {
                    file: path.clone(),
                    position: name_position(&entry.source, node),
                    ask: Ask::Implementation { anchor: node.id.clone() },
                });
            }
        }
        // Room for all of it, or nothing asked yet and a ceiling that is not
        // simply zero - the one case a file is admitted over the ceiling.
        let fits = asking.len() + for_file.len() <= budgets.max_sites;
        if !fits && !(asking.is_empty() && budgets.max_sites > 0) {
            truncated = true;
            break;
        }
        unanswerable += skipped;
        asking.append(&mut for_file);
    }
    Questions { asking, unanswerable, truncated }
}

/// Where a site is, as [`questions`] matches an overload call with a hop
/// site at the same call: the enclosing node and the name's position.
pub(super) fn site_key(site: &OpenSite) -> (String, u32, u32) {
    (site.from_id.clone(), site.position.line, site.position.col)
}

/// Whether structural edge `replaced` of `graph` is a hop the linker cannot
/// settle, so a [`OpenSiteKind::Reference`] site naming it is worth a
/// question: the edge lands on a `pending_symbol` placeholder addressed by
/// file `f` and bare name `n`, `f` is in the index, and `f` itself declares
/// no `n` the linker would link this edge to. A file that does is where the
/// linker lands the edge already; what is left is `n` re-exported from
/// elsewhere, `default`, or `n` declared with a kind the edge does not accept.
///
/// Which kinds count follows core's structural kind rule for a syntactic
/// edge on a name key (`LINKABLE_EDGE_KINDS` in
/// `core/src/graph/symbol_links.rs`): a `CALLS` edge links only to a
/// `Function`, a `SUPERTYPE_OF` edge only to a `Type` or a `Function`, and
/// any other edge kind to any addressable node. So a call of
/// `export const handler = make()` imported by name is asked (GM-596): the
/// linker would leave it unlinked, and only an engine answer widens `CALLS`
/// to a `Variable`.
pub(super) fn unsettled_hop(index: &SdkIndex, graph: &g_mesh_plugin_sdk::FileGraph, replaced: &str) -> bool {
    let Some(edge) = graph.edges.iter().find(|edge| edge.id == replaced) else { return false };
    let Some(placeholder) = graph.nodes.iter().find(|node| node.id == edge.to_id) else { return false };
    if placeholder.native_kind.as_deref() != Some(PlaceholderKind::PendingSymbol.native_kind()) {
        return false;
    }
    let Some(target) = &placeholder.target else { return false };
    let (TargetScope::File(file), TargetKey::Name(name)) = (&target.scope, &target.key) else { return false };
    let Some(declaring) = index.entry(&RelPath::new(file)) else { return false };
    let accepts = |kind: NodeKind| match edge.kind {
        EdgeKind::Calls => kind == NodeKind::Function,
        EdgeKind::SupertypeOf => matches!(kind, NodeKind::Type | NodeKind::Function),
        _ => true,
    };
    let nodes = &declaring.graph.nodes;
    let named: Vec<&WireNode> =
        nodes.iter().filter(|node| node.name == *name && is_addressable(node)).collect();
    // As core's `drop_type_members`, before the kind rule: a name key in a
    // file scope addresses a module-level declaration, so type members go
    // whenever a non-member shares the name. Otherwise a method
    // `Router#handler` would settle the call of a const `handler`.
    let members: Vec<bool> = named.iter().map(|node| is_type_member(nodes, node)).collect();
    let keep_members = members.iter().all(|member| *member);
    !named.iter().zip(&members).any(|(node, member)| (keep_members || !member) && accepts(node.kind))
}

/// Whether `node` is a member of a type `graph` declares: the parent of its
/// `qualifiedPath`, or of one of its alias paths, is the `qualifiedName` of
/// a `Type` node in the same file. Mirrors core's `is_type_member` for a
/// candidate without a container; a receiver type in another file of a Go
/// package is not seen here, which only leaves such a hop settled.
pub(super) fn is_type_member(graph: &[WireNode], node: &WireNode) -> bool {
    node.qualified_path
        .iter()
        .chain(&node.alias_paths)
        .filter(|path| path.len() >= 2)
        .filter_map(|path| path.head())
        .any(|parent| {
            let parent = parent.display();
            graph.iter().any(|ty| ty.kind == NodeKind::Type && ty.qualified_name == parent)
        })
}

/// The declarations an [`OpenSiteKind::OverloadCall`] site may be bound to,
/// as the filter in [`questions`] reads them: every node of the index that
/// carries `declarations`, by id, and by the two keys a placeholder can wait
/// on.
pub(super) struct Overloaded {
    pub(super) ids: HashSet<String>,
    pub(super) names: HashSet<String>,
    pub(super) qualified: HashSet<String>,
}

impl Overloaded {
    pub(super) fn of(index: &SdkIndex) -> Self {
        let mut overloaded = Self { ids: HashSet::new(), names: HashSet::new(), qualified: HashSet::new() };
        for (_, entry) in index.files() {
            for node in entry.graph.nodes.iter().filter(|node| node.declarations.is_some()) {
                overloaded.ids.insert(node.id.clone());
                overloaded.names.insert(node.name.clone());
                overloaded.qualified.insert(node.qualified_name.clone());
            }
        }
        overloaded
    }

    /// Whether structural edge `replaced` of `graph` lands on an overload
    /// set: a node that carries `declarations`, or a placeholder whose key
    /// names one somewhere in the index.
    pub(super) fn targets(&self, graph: &g_mesh_plugin_sdk::FileGraph, replaced: &str) -> bool {
        let Some(edge) = graph.edges.iter().find(|edge| edge.id == replaced) else { return false };
        if self.ids.contains(&edge.to_id) {
            return true;
        }
        graph
            .nodes
            .iter()
            .find(|node| node.id == edge.to_id)
            .and_then(|node| node.target.as_ref())
            .is_some_and(|target| match &target.key {
                TargetKey::Name(name) => self.names.contains(name),
                TargetKey::QualifiedName(qualified) => self.qualified.contains(qualified),
            })
    }
}

/// Where a declaration's *name* starts, for a request that has to land on an
/// identifier.
///
/// A node's range starts at the whole declaration - `pub trait Foo` starts at
/// `pub`, `export class Foo` at `export` - and a server asked at that position
/// resolves the token there, which is a keyword and has no definition or
/// implementations. So the request is aimed at the first occurrence of the
/// node's own name inside its range.
///
/// A heuristic, and a language-agnostic one: it uses only the node's `name`
/// and its own text. It can be fooled by a declaration whose name also appears
/// earlier inside its own range (an attribute or annotation that repeats it),
/// and falls back to the range's start when the name is not found there at
/// all, which is the behaviour there would have been without it.
pub(super) fn name_position(source: &str, node: &WireNode) -> Position {
    if node.name.is_empty() {
        return node.range.start;
    }
    let last = node.range.end.line.max(node.range.start.line);
    for line in node.range.start.line..=last {
        let text = line_text(source, line);
        let from = if line == node.range.start.line { node.range.start.col as usize } else { 0 };
        let chars: Vec<char> = text.chars().collect();
        if from > chars.len() {
            continue;
        }
        let haystack: String = chars[from..].iter().collect();
        if let Some(byte_at) = haystack.find(&node.name) {
            let col = from + haystack[..byte_at].chars().count();
            return Position { line, col: col as u32 };
        }
    }
    node.range.start
}

/// Where `name` is written inside one of an overload set's declarations, for
/// a `hover` that has to land on it.
///
/// [`name_position`]'s search with one difference: the match must be a whole
/// identifier. A declaration's range includes its decorators, and a function
/// called `load` must not be found inside `@overload`. `None` when the name is
/// not there, which leaves that candidate without a hover and so unmatched.
pub(super) fn declaration_name_position(
    source: &str,
    name: &str,
    declaration: &WireDeclaration,
) -> Option<Position> {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if name.is_empty() {
        return None;
    }
    let width = name.chars().count();
    for line in declaration.start_line..=declaration.end_line.max(declaration.start_line) {
        let chars: Vec<char> = line_text(source, line).chars().collect();
        let from = if line == declaration.start_line { declaration.start_col as usize } else { 0 };
        let to = if line == declaration.end_line {
            (declaration.end_col as usize).min(chars.len())
        } else {
            chars.len()
        };
        let mut col = from;
        while col + width <= to {
            let here: String = chars[col..col + width].iter().collect();
            let before = col.checked_sub(1).map(|at| chars[at]);
            let after = chars.get(col + width).copied();
            if here == name && !before.is_some_and(word) && !after.is_some_and(word) {
                return Some(Position { line, col: col as u32 });
            }
            col += 1;
        }
    }
    None
}
