//! Where a server location lands: a file, a node, an overload.

use std::collections::BTreeSet;
use std::path::Path;

use g_mesh_wire::{Position, TargetKey, WireNode};
use serde_json::Value;

use super::super::config::OverloadDisambiguation;
use super::super::position::{line_text, path_from_uri, PositionEncoding};
use super::answers::{address_of, answer_ids, is_addressable};
use g_mesh_plugin_sdk::OpenSite;
use g_mesh_plugin_sdk::RelPath;
use g_mesh_plugin_sdk::SdkIndex;

/// A location a server answered with, in the server's own coordinates.
pub(super) struct ServerLocation {
    pub(super) uri: String,
    pub(super) line: u32,
    pub(super) column: u32,
}

/// The locations in a `definition`/`implementation` result.
///
/// Three shapes are legal and all three appear in the wild: a single
/// `Location`, an array of them, and - because this client advertises
/// `linkSupport` - an array of `LocationLink`s, whose `targetSelectionRange`
/// is the one that points at the declaration's name rather than at its whole
/// body.
pub(super) fn locations(result: &Value) -> Vec<ServerLocation> {
    fn one(value: &Value, into: &mut Vec<ServerLocation>) {
        let link = value.get("targetUri").and_then(Value::as_str);
        let (uri, range) = match link {
            Some(uri) => (uri, value.get("targetSelectionRange").or_else(|| value.get("targetRange"))),
            None => match value.get("uri").and_then(Value::as_str) {
                Some(uri) => (uri, value.get("range")),
                None => return,
            },
        };
        let Some(start) = range.and_then(|range| range.get("start")) else { return };
        let (Some(line), Some(column)) =
            (start.get("line").and_then(Value::as_u64), start.get("character").and_then(Value::as_u64))
        else {
            return;
        };
        into.push(ServerLocation { uri: uri.to_string(), line: line as u32, column: column as u32 });
    }

    let mut out = Vec::new();
    match result {
        Value::Array(values) => values.iter().for_each(|value| one(value, &mut out)),
        Value::Object(_) => one(result, &mut out),
        _ => {}
    }
    out
}

/// The file a location is in, when this index holds it at all.
///
/// Separate from [`node_at`] because the two "no" answers are different
/// questions: a location in a file nothing indexed can never become an edge,
/// while a location in a file this index *does* hold, at a position no
/// declaration covers, is exactly the case the second hop was added for.
pub(super) fn file_at(index: &SdkIndex, roots: [&Path; 2], location: &ServerLocation) -> Option<RelPath> {
    let absolute = path_from_uri(&location.uri)?;
    let relative = roots.iter().find_map(|root| absolute.strip_prefix(root).ok())?;
    let path = RelPath::new(relative.to_string_lossy().replace('\\', "/"));
    index.entry(&path).is_some().then_some(path)
}

/// A server location's position in the wire's own column units.
pub(super) fn wire_position(
    index: &SdkIndex,
    path: &RelPath,
    encoding: PositionEncoding,
    location: &ServerLocation,
) -> Position {
    let source = index.source(path).unwrap_or_default();
    let column = encoding.to_wire_column(line_text(source, location.line), location.column);
    Position { line: location.line, col: column }
}

/// The node a location points at, together with the file it is in.
///
/// The tightest node containing the location, which may be an encloser of
/// what the server meant: a class field or an object-literal method has no
/// node of its own, so a definition at one lands on the class or the const
/// around it. [`lands_on_name`] tells the two apart, for a landing on the
/// referrer or on a node containing the site too, any edge kind.
pub(super) fn node_at<'i>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    location: &ServerLocation,
) -> Option<(RelPath, &'i WireNode)> {
    let path = file_at(index, roots, location)?;
    let at = wire_position(index, &path, encoding, location);
    let node = index.node_at(&path, at)?;
    is_addressable(node).then_some((path, node))
}

/// The one node a multi-location answer names, counting only the locations
/// that sit at their node's own name ([`lands_on_name`]); `None` when none
/// does or when they name different nodes.
///
/// For a `CALLS` answer whose locations disagree in [`record_answer`]: a
/// location inside a body (a call signature declared in another function)
/// names no declaration, so it is no evidence against the one that does.
pub(super) fn named_landing<'i, 'l>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    found: &'l [ServerLocation],
) -> Option<(RelPath, &'i WireNode, &'l ServerLocation)> {
    let mut named = found.iter().filter_map(|location| {
        let (path, node) = node_at(index, roots, encoding, location)?;
        lands_on_name(index, &path, wire_position(index, &path, encoding, location), node)
            .then_some((path, node, location))
    });
    let first = named.next()?;
    named.all(|(_, node, _)| node.id == first.1.id).then_some(first)
}

/// Whether `at` (wire columns) is where `node` names itself: the start of
/// the first whole-word occurrence of its name at or after the start of its
/// range. A definition answer points at a declaration's name, and a node's
/// range starts at the declaration, so a precise answer passes; one that
/// landed on an encloser points at a member's name and fails, even a member
/// sharing the encloser's name, which occurs after the encloser's own.
/// A landing on the referrer or on a node containing the site, any edge
/// kind, passes only here: at that node's name it is recursion or `Self`,
/// elsewhere the encloser fallback.
pub(super) fn lands_on_name(index: &SdkIndex, path: &RelPath, at: Position, node: &WireNode) -> bool {
    let name: Vec<char> = node.name.chars().collect();
    if name.is_empty() {
        return false;
    }
    let Some(source) = index.source(path) else { return false };
    let is_word = |character: char| character.is_alphanumeric() || character == '_' || character == '$';
    let (start, end) = (node.range.start, node.range.end);
    for line in start.line..=end.line {
        let text: Vec<char> = line_text(source, line).chars().collect();
        let from = if line == start.line { start.col as usize } else { 0 };
        let mut col = from;
        while col + name.len() <= text.len() {
            let before = col > 0 && is_word(text[col - 1]);
            let after = text.get(col + name.len()).is_some_and(|character| is_word(*character));
            if !before && !after && text[col..col + name.len()] == name[..] {
                return at == Position { line, col: col as u32 };
            }
            col += 1;
        }
    }
    false
}

/// The overload declaration a location points into: the tightest
/// `declarations` range of any node of its file that contains it, with that
/// node and the declaration's ordinal.
///
/// Not [`node_at`]: a node's own range is not its overload set's extent.
/// Python's node is the first `@overload` stub, so a server pointing at the
/// second one lands, by node range, on the enclosing class or file; and
/// TypeScript's node is the implementation.
pub(super) fn declaration_at<'i>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    location: &ServerLocation,
) -> Option<(RelPath, &'i WireNode, u32)> {
    let path = file_at(index, roots, location)?;
    let at = wire_position(index, &path, encoding, location);
    let at = (at.line, at.col);
    let graph = index.graph(&path)?;
    let mut best: Option<((u32, u32), &WireNode, u32)> = None;
    for node in graph.nodes.iter().filter(|node| is_addressable(node)) {
        for declaration in node.declarations.iter().flatten() {
            let (start, end) = (
                (declaration.start_line, declaration.start_col),
                (declaration.end_line, declaration.end_col),
            );
            if !(start <= at && at <= end) {
                continue;
            }
            let lines = end.0.saturating_sub(start.0);
            let span = (lines, if lines == 0 { end.1.saturating_sub(start.1) } else { end.1 });
            let tighter = best.as_ref().is_none_or(|(held, held_node, held_ordinal)| {
                (span, &node.id, declaration.ordinal) < (*held, &held_node.id, *held_ordinal)
            });
            if tighter {
                best = Some((span, node, declaration.ordinal));
            }
        }
    }
    best.map(|(_, node, ordinal)| (path, node, ordinal))
}

/// Step 1 of overload binding: the one node every location of `found` lands
/// on, by declaration containment first and node containment otherwise, with
/// the ordinals of its declarations they land in. `None` when nothing lands
/// in the index, or two locations land on different nodes (the `agree` rule
/// of a plain `definition` answer).
pub(super) fn overload_landing<'i>(
    index: &'i SdkIndex,
    roots: [&Path; 2],
    encoding: PositionEncoding,
    found: &[ServerLocation],
) -> Option<(RelPath, &'i WireNode, BTreeSet<u32>)> {
    let mut landed: Option<(RelPath, &WireNode, BTreeSet<u32>)> = None;
    for location in found {
        let (path, node, ordinal) = match declaration_at(index, roots, encoding, location) {
            Some((path, node, ordinal)) => (path, node, Some(ordinal)),
            None => match node_at(index, roots, encoding, location) {
                Some((path, node)) => (path, node, None),
                None => continue,
            },
        };
        match &mut landed {
            Some((_, held, _)) if held.id != node.id => return None,
            Some((_, _, ordinals)) => ordinals.extend(ordinal),
            None => landed = Some((path, node, ordinal.into_iter().collect())),
        }
    }
    landed
}

/// What steps 2-4 of overload binding make of the ordinals an answer landed
/// in.
pub(super) enum Choice {
    Bound(u32),
    Unbound,
    /// Ask `hover` at the call and at each of these.
    Hover(Vec<u32>),
}

/// Steps 2-4: exactly one bindable ordinal binds; several go to `hover` when
/// the manifest allows it and stay unbound otherwise. A declaration with a
/// body is never bindable in a set that has bodiless ones: no call binds an
/// implementation, so a server pointing there is answering "the function",
/// not "the overload".
pub(super) fn choose_overload(
    node: &WireNode,
    ordinals: &BTreeSet<u32>,
    disambiguation: OverloadDisambiguation,
) -> Choice {
    let Some(declarations) = &node.declarations else { return Choice::Unbound };
    let stubs = declarations.iter().any(|declaration| !declaration.has_body);
    let bindable = |ordinal: u32| {
        declarations
            .iter()
            .find(|declaration| declaration.ordinal == ordinal)
            .is_some_and(|declaration| !(stubs && declaration.has_body))
    };
    match ordinals.len() {
        1 => {
            let ordinal = *ordinals.iter().next().expect("one ordinal");
            if bindable(ordinal) {
                Choice::Bound(ordinal)
            } else {
                Choice::Unbound
            }
        }
        0 => Choice::Unbound,
        _ if disambiguation == OverloadDisambiguation::Hover => {
            let candidates: Vec<u32> =
                ordinals.iter().copied().filter(|ordinal| bindable(*ordinal)).collect();
            if candidates.is_empty() {
                Choice::Unbound
            } else {
                Choice::Hover(candidates)
            }
        }
        _ => Choice::Unbound,
    }
}

/// Whether an overload answer agrees with the structural edge its site
/// refines: the edge lands on `node`, or on a placeholder that waits on it -
/// one whose id the answer itself would get, or whose key names `node`.
/// Anything else would move the call to another target, which refining
/// never does.
pub(super) fn refines(
    index: &SdkIndex,
    file: &RelPath,
    site: &OpenSite,
    replaced: &str,
    node: &WireNode,
) -> bool {
    let Some(graph) = index.graph(file) else { return false };
    let Some(edge) = graph.edges.iter().find(|edge| edge.id == replaced) else { return false };
    if edge.to_id == node.id {
        return true;
    }
    let address = address_of(node, site.from_container.clone());
    let (_, prospective) = answer_ids(file, &site.from_id, site.edge_kind, &address, None);
    if prospective == replaced {
        return true;
    }
    graph.nodes.iter().find(|held| held.id == edge.to_id).and_then(|held| held.target.as_ref()).is_some_and(
        |target| match &target.key {
            TargetKey::Name(name) => name == &node.name,
            TargetKey::QualifiedName(qualified) => qualified == &node.qualified_name,
        },
    )
}
