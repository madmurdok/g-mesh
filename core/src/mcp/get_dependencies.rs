//! Real logic behind the `get_dependencies` MCP tool - the one MVP tool whose
//! answer is genuinely transitive rather than a single hop. The walk itself,
//! and the whole truncation contract around it, already live in
//! `graph::traversal`; this module is the "anchor -> bounded `IMPORTS` walk ->
//! JSON" wiring around it, plus the two decisions `traversal` deliberately
//! leaves to its caller: which edge kind to follow, and how much of the
//! result is the caller's business.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::containers::{self, DefiningContainer};
use crate::graph::pagination::{self, Direction};
use crate::graph::queries;
use crate::graph::resume_token::{self, ResumeState, VisitedNode};
use crate::graph::traversal::{self, ReachedNode, TraversalOptions, TraversalResult, TruncatedBy};
use crate::storage::write::NodeRecord;

use super::tool_result::{error, internal_error, success};
use super::GetDependenciesParams;

/// The only edge kind this tool walks: `get_dependencies` answers "what does
/// this import" / "what imports this", not "what does this touch" - the
/// `CALLS`/`REFERENCES` side of the graph belongs to the single-hop tools.
const IMPORT_EDGE: &str = "IMPORTS";

/// The kind an import that resolved to nothing comes back as: a placeholder
/// standing in for a file this index does not have. See
/// [`DependencyNode::file_path`] for why it needs a case of its own.
const MODULE_KIND: &str = "Module";

/// `get_dependencies`'s own default when the caller omits `max_depth` -
/// deliberately far below `traversal::DEFAULT_MAX_DEPTH` (5), which stays
/// `TraversalOptions`' generic, direction-agnostic default for any future
/// caller of the walk engine and is left untouched here.
///
/// This tool's traffic is asymmetric in a way a single shared default can't
/// account for: an `Outgoing` walk (what does this file import) is bounded by
/// how many things one file imports, typically small; an `Incoming` walk
/// (what imports this file) can fan out across an entire codebase from one
/// shared/foundational module, and that fan-out *compounds* with every extra
/// hop of depth - `max_fanout` (default 50, see `traversal::DEFAULT_MAX_FANOUT`)
/// bounds one node's own children, not how wide a whole level gets. Measured
/// on g-mesh-bench's corpus (v0.4.0 outlier findings, `get_dependencies`
/// section): an `Incoming` walk of a shared math-utils entrypoint at the old
/// depth-5 default produced a 115,863-character response the MCP client's
/// transport rejected outright; the identical call at `max_depth: 1` dropped
/// to 10,436 characters and succeeded. Depth 1 alone only answers "who
/// directly depends on this", too narrow for the impact-analysis question
/// this tool mostly exists for ("what would changing this break, and what
/// depends on *that*"); depth 2 answers that shape while staying an order of
/// magnitude more conservative than the walk engine's own default. The size-
/// bounded truncation added alongside this default (`bound_walk`) is the
/// backstop for the remaining cases where even depth 2 is too wide on an
/// unusually shared module.
const DEFAULT_MAX_DEPTH: u32 = 2;

/// One reached dependency, with how many import hops away it is. No
/// `resolved` flag, unlike the single-hop tools: a node several hops out is
/// reached over a *path* of edges, and one flag can only describe one of
/// them, so it would read as a trust claim about the path that it does not
/// make. The edge list the walk collects is left out for the same reason it
/// isn't needed here - impact analysis asks which files an edit reaches, not
/// by which route.
///
/// No `name` field: it never carries information `qualifiedName` doesn't
/// already have (at worst a shorter, less unique view of the same symbol) -
/// same rule the single-hop tools' row shapes follow.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyNode {
    id: String,
    kind: String,
    /// Omitted (not `null`) for a `File`-kind row: a `File` node's
    /// `qualifiedName` IS its own `filePath` by construction (see
    /// `pagination::FILE_KIND`'s doc comment), so a `File` row would otherwise
    /// carry the exact same path string twice. Present for `Module` rows,
    /// where it's the only field that carries the specifier at all - the
    /// mirror image of `file_path` below being absent there.
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    /// The file this dependency *is*, and null when it is not one. An import
    /// `graph::imports` could not link - a package, or a relative path with
    /// nothing indexed behind it - stays a `Module` placeholder whose stored
    /// `filePath` is the *importing* file, because that is where the
    /// specifier is written. Echoing that column here would name the wrong
    /// file twice over: it reads as "the dependency lives there", and it
    /// collides with the importer's own row in the same walk. `qualifiedName`
    /// still carries the specifier, which is all there is to act on for
    /// something with no file to open.
    file_path: Option<String>,
    /// Import hops from the anchor. Always >= 1: the anchor itself is the
    /// walk's depth-0 node and is not reported back to the caller who named it.
    depth: u32,
}

impl From<ReachedNode> for DependencyNode {
    fn from(r: ReachedNode) -> Self {
        let is_file = r.node.kind == pagination::FILE_KIND;
        let file_path = (r.node.kind != MODULE_KIND).then_some(r.node.file_path);
        Self {
            id: r.node.id,
            kind: r.node.kind,
            qualified_name: (!is_file).then_some(r.node.qualified_name),
            file_path,
            depth: r.depth,
        }
    }
}

/// Not the `results`/`hasMore`/`nextCursor` envelope the list-shaped tools
/// share: a truncated walk is not a page, and which continuation field it
/// hands back depends on what cut it. `frontierNodes` is non-empty only for
/// `maxDepth` (re-root the same call on them), `resumeToken` is present only
/// for `explorationBudget` (call again with it, nothing else); `maxFanout`
/// needs neither - the caller re-queries the cut node with the single-hop
/// tools' own pagination.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyWalk {
    results: Vec<DependencyNode>,
    truncated: bool,
    truncated_by: Option<&'static str>,
    frontier_nodes: Vec<String>,
    resume_token: Option<String>,
    /// The node this walk actually started from, when that is not the one the
    /// caller named - see [`entry_point_for`] and [`incoming_from_file`].
    /// Absent (not `null`) whenever the anchor was taken literally, which is
    /// the overwhelming majority of calls and must not pay bytes to say
    /// nothing happened.
    ///
    /// Always present when a substitution *did* happen, and deliberately so:
    /// the tool is answering a question adjacent to the one asked, and a
    /// caller that cannot see which file was chosen cannot tell a right guess
    /// from a wrong one.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_from: Option<ResolvedFrom>,
}

/// What the caller named, and what it was taken to mean.
///
/// Exactly one of `file_path`/`qualified_name` is present, and which one says
/// what kind of node the walk ran from: a file, for the entry-point
/// substitution ([`entry_point_for`]), or a logical container, for the
/// module-anchor one ([`incoming_from_file`]). Both are optional rather than
/// one field of either meaning, so that a caller reading `filePath` keeps
/// reading exactly what it read before this second case existed.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ResolvedFrom {
    requested: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qualified_name: Option<String>,
    hint: &'static str,
}

impl ResolvedFrom {
    /// A substitution that landed on a file.
    fn file(requested: &str, file_path: String) -> Self {
        Self {
            requested: requested.to_string(),
            file_path: Some(file_path),
            qualified_name: None,
            hint: RESOLVED_FROM_HINT,
        }
    }

    /// A substitution that landed on the container a file defines.
    fn module(requested: &str, key: String) -> Self {
        Self {
            requested: requested.to_string(),
            file_path: None,
            qualified_name: Some(key),
            hint: MODULE_ANCHOR_HINT,
        }
    }
}

const RESOLVED_FROM_HINT: &str =
    "The path given is not an indexed file, so the walk started from the single entry point \
     found under it. Pass that file directly to avoid the substitution.";

const MODULE_ANCHOR_HINT: &str =
    "In this language the import graph's nodes are modules, not files: nothing points an import \
     at a file, so an Incoming walk anchored on one is empty by construction rather than because \
     the file is unimported. The walk ran from the module this file defines instead. Pass that \
     `qualifiedName` as the anchor to avoid the substitution.";

/// The wire spelling of each truncation cause, fixed by the contract in
/// `docs/architecture/g-mesh-v1.md`, plus `bound_walk`'s own `"responseSize"` -
/// not part of that contract since it isn't a `TruncatedBy` cause at all (it
/// fires after `traversal` has already finished, on the wire DTO's own
/// serialized size), but spelled the same way for consistency.
fn wire_name(cause: TruncatedBy) -> &'static str {
    match cause {
        TruncatedBy::MaxDepth => "maxDepth",
        TruncatedBy::MaxFanout => "maxFanout",
        TruncatedBy::ExplorationBudget => "explorationBudget",
    }
}

/// A second, independent ceiling on top of `max_depth`/`max_fanout`: neither
/// bounds the *total size* of a whole level, only depth (how many levels) or
/// fanout (one node's own children) individually - a node can be depth- and
/// fanout-bounded and still, summed across every node reached at a given
/// depth, produce a response too large for `pagination::MAX_RESPONSE_BYTES`.
///
/// Reuses the exact resume-token mechanism `traversal::traverse`'s own
/// exploration-budget cut already relies on (`graph::resume_token`), just
/// built from wherever this function's own byte cut lands rather than
/// wherever the CTE's internal row budget ran out - so a caller sees the
/// same `resumeToken` continuation contract regardless of which cause
/// actually cut the walk short. `visited` reseeds every kept node (plus the
/// anchor) exactly the way an exploration-budget resume already does -
/// `ResumeState`'s own doc comment explains why that is the conservative,
/// provably-complete choice, not just cheaper to build. `walked` keeps only
/// the edges whose child landed inside the kept set: an edge to a node this
/// cut dropped must stay undiscovered as far as the token is concerned, or a
/// resumed call would never re-offer it.
///
/// A response that already fits under budget comes back with the original
/// result's `truncated`/`truncated_by`/`frontier_nodes`/`resume_token`
/// completely unchanged - this function is pure headroom for the rare
/// oversized level, never a new default for the common one.
///
/// `prior_visited`/`prior_walked` are the walk's history from *before* this
/// call - empty for a fresh walk (`from_root`), or the incoming
/// `resume_token`'s own `visited`/`walked` for a continuation (`continued`).
/// `TraversalResult.nodes`/`.edges` from a resumed call hold only what that
/// call newly discovered (see `traversal::resume`'s doc comment), so a token
/// built from them alone - without folding in what earlier calls in the
/// chain already reported - would forget that history and risk a later call
/// re-discovering and re-returning an already-reported node. Whether the
/// natural cause (`ExplorationBudget`) or this function's own byte cut is
/// what carries the token forward, the chain must accumulate the same way
/// `traversal::resume` already does internally for its own case (see
/// `ResumeState`'s doc comment) - this is that same accumulation, one layer
/// up, for the case `traversal.rs` cannot see: a response the JSON wire
/// shape made too big.
fn bound_walk(
    result: TraversalResult,
    direction: Direction,
    edge_kind: Option<String>,
    max_depth: u32,
    max_fanout: u32,
    prior_visited: Vec<VisitedNode>,
    prior_walked: Vec<String>,
) -> DependencyWalk {
    // Present only on a fresh walk: a resumed call's `nodes` excludes
    // already-visited nodes (the anchor included), so `prior_visited` already
    // carries it forward instead.
    let anchor_id = result.nodes.first().filter(|n| n.depth == 0).map(|n| n.node.id.clone());

    let mut dtos: Vec<DependencyNode> = Vec::with_capacity(result.nodes.len());
    for node in result.nodes {
        if node.depth > 0 {
            dtos.push(DependencyNode::from(node));
        }
    }

    let Some(cut) = pagination::longest_prefix_fitting(&dtos, pagination::MAX_RESPONSE_BYTES) else {
        return DependencyWalk {
            results: dtos,
            truncated: result.truncated,
            truncated_by: result.truncated_by.map(wire_name),
            frontier_nodes: result.frontier_nodes,
            resume_token: result.resume_token,
            // Set by the anchor arm, which is the only layer that knows
            // whether the root it handed down was the one the caller named.
            resolved_from: None,
        };
    };

    let mut visited = prior_visited;
    if let Some(id) = anchor_id {
        visited.push(VisitedNode { id, depth: 0 });
    }
    visited.extend(dtos[..cut].iter().map(|d| VisitedNode { id: d.id.clone(), depth: d.depth }));

    let kept_ids: HashSet<&str> = dtos[..cut].iter().map(|d| d.id.as_str()).collect();
    let mut walked = prior_walked;
    walked.extend(result.edges.into_iter().filter_map(|e| {
        let child = match direction {
            Direction::Outgoing => &e.to_id,
            Direction::Incoming => &e.from_id,
        };
        kept_ids.contains(child.as_str()).then_some(e.id)
    }));

    let token =
        resume_token::encode(&ResumeState { direction, edge_kind, max_depth, max_fanout, visited, walked });

    DependencyWalk {
        results: dtos.into_iter().take(cut).collect(),
        truncated: true,
        truncated_by: Some("responseSize"),
        frontier_nodes: Vec::new(),
        resume_token: Some(token),
        resolved_from: None,
    }
}

/// A fresh walk's shape minus its root - what both anchor arms forward
/// unchanged once they have resolved the root the caller meant.
struct WalkShape {
    direction: Direction,
    max_depth: Option<u32>,
    max_fanout: Option<u32>,
}

/// The walk itself, at the documented defaults unless the caller narrowed
/// them. The exploration budget is deliberately not one of the caller's
/// dials: it bounds what the query engine visits internally, not what the
/// caller asked to see, so it is always the module default here.
fn from_root(conn: &Connection, root: String, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    from_root_reporting(conn, root, shape, None)
}

/// [`from_root`] for the arm that had to *choose* the root rather than being
/// handed one - see [`entry_point_for`]. The substitution rides back on the
/// response instead of being kept quiet, so the caller can see which file the
/// walk actually ran from.
fn from_root_reporting(
    conn: &Connection,
    root: String,
    shape: &WalkShape,
    resolved_from: Option<ResolvedFrom>,
) -> Result<CallToolResult, ErrorData> {
    let mut options = TraversalOptions::new(root, shape.direction);
    options.edge_kind = Some(IMPORT_EDGE.to_string());
    // `DEFAULT_MAX_DEPTH` here, not `TraversalOptions::new`'s own default -
    // see that constant's doc comment for why this tool needs a stricter one.
    options.max_depth = shape.max_depth.unwrap_or(DEFAULT_MAX_DEPTH);
    if let Some(max_fanout) = shape.max_fanout {
        options.max_fanout = max_fanout;
    }
    let (direction, edge_kind, max_depth, max_fanout) =
        (options.direction, options.edge_kind.clone(), options.max_depth, options.max_fanout);

    let result = traversal::traverse(conn, options)
        .map_err(|e| internal_error("failed to walk the import graph", e))?;
    let mut walk = bound_walk(result, direction, edge_kind, max_depth, max_fanout, Vec::new(), Vec::new());
    walk.resolved_from = resolved_from;
    success(&walk)
}

/// `file_path` anchors a walk three ways, tried in this order, with no new
/// tool parameter to pick among them (GM-267 decision 4 - a parameter is a
/// per-session token tax measured in this project, GM-188, and the
/// `file_path` argument already disambiguates on its own):
///
///  1. **Exact file.** Unchanged from before containers existed, except for
///     `Incoming` on a language whose import graph is made of modules - see
///     [`incoming_from_file`], which is where GM-356's substitution lives.
///  2. **Exact container key** (`github.com/x/pkg`, `ripgrep::search`, ... -
///     Data Model > Logical containers), since GM-267. A key is only unique
///     *within* a language, so more than one match is a real ambiguity - a Go
///     package and a Rust module sharing the string - and is refused rather
///     than guessed at, the same stance `entry_point_for` already takes for
///     two file candidates. This is deliberately **not** reported through
///     `resolvedFrom`: that field exists for a *substitution* - the tool
///     answering a question adjacent to the one asked - and choosing the one
///     node an exact key names outright is not one; `resolvedFrom`'s own doc
///     comment ties it to "the caller cannot tell a right guess from a wrong
///     one", which does not apply to an exact match.
///  3. **Miss-path inference** (`entry_point_for`, unchanged): a directory
///     prefix or a package-name segment with exactly one entry point behind
///     it. Tried last because it is the weakest of the three - a guess from
///     an adjacent fact, not an exact match - and reported via `resolvedFrom`
///     precisely because it is one.
fn from_file(
    conn: &Connection,
    entry_points: &[String],
    file_path: &str,
    shape: &WalkShape,
) -> Result<CallToolResult, ErrorData> {
    let anchor =
        queries::find_file_node(conn, file_path).map_err(|e| internal_error("failed to look up file", e))?;

    if let Some(node) = anchor {
        return match shape.direction {
            Direction::Incoming => incoming_from_file(conn, &node.id, file_path, shape),
            Direction::Outgoing => from_root(conn, node.id, shape),
        };
    }

    let containers = queries::find_containers_by_key(conn, file_path)
        .map_err(|e| internal_error("failed to look up containers by key", e))?;
    match containers.len() {
        0 => {}
        1 => {
            let container = containers.into_iter().next().expect("len checked above");
            return from_root(conn, container.id, shape);
        }
        _ => return error(ambiguous_container_message(file_path, &containers)),
    }

    // Not a file or a container key. Before refusing, see whether what was
    // named has exactly one entry point behind it - `@excalidraw/math`
    // almost always does.
    if let Some(entry) = entry_point_for(conn, entry_points, file_path)? {
        let resolved = ResolvedFrom::file(file_path, entry.file_path);
        return from_root_reporting(conn, entry.id, shape, Some(resolved));
    }
    error(no_file_message(conn, entry_points, file_path)?)
}

/// `Incoming` from an indexed file, which is the one arm of [`from_file`] a
/// literal anchor can answer *wrongly* rather than merely unhelpfully
/// (GM-356).
///
/// THE DEFECT
///
/// Outside TypeScript an `IMPORTS` edge runs from a `File` node to a
/// *container* - `requests.adapters`, `grep_searcher::sink`,
/// `github.com/gin-gonic/gin/render` - because that is what the language's
/// import statement names. Nothing ever points an import at a file, so a
/// walk anchored on one finds no incoming edge, finishes inside its bounds,
/// and returns `results: []` with `truncated: false`. That is the shape this
/// tool uses to say *nothing imports this*, and it is false about
/// `src/requests/adapters.py`, which `sessions.py`, `models.py` and two test
/// files all import. In TypeScript a module *is* a file, so the two coincide
/// and this never surfaced. GM-259's fallback does not reach it either: that
/// one runs when the anchor *misses*, and here it resolves exactly.
///
/// WHY IT SUBSTITUTES RATHER THAN REFUSING
///
/// Both were open, and a refusal naming the container would also have been
/// honest. It loses on the evidence this module already collected for the
/// same choice one arm below: [`entry_point_for`]'s doc comment records that
/// naming an anchor and then declining to use it cost a whole round trip and
/// made two of five benchmark repetitions abandon the tool. The substitution
/// is also on firmer ground here than there - `entry_point_for` infers from
/// an adjacent fact (a directory that shares a package's name), while this
/// reads the index's own membership edges, and
/// [`containers::defining_containers`] records that it picked the right
/// container on all 234 files of three indexed repositories with no ambiguity
/// anywhere. Where it is *not* unambiguous it refuses and names what it
/// found, which is the refusal branch exactly where a refusal is the only
/// honest answer.
///
/// WHAT IT CANNOT CHANGE
///
/// Two guards, both structural, so that no call that answers today answers
/// differently tomorrow:
///
///  - It is `Incoming` only. `Outgoing` from a file is already right - those
///    edges *leave* the file node - and running it from the container would
///    quietly answer about every file in the module instead of the one asked
///    about.
///  - The file must have no incoming `IMPORTS` edge of its own. That is what
///    keeps TypeScript untouched (its imports arrive at files, so the literal
///    anchor stands), and it holds for any future plugin that emits
///    file-level import edges too, without this function knowing a list of
///    languages. A file with no container to substitute keeps its literal
///    answer as well: there is no module to name, so there is nothing to
///    report and nothing to suggest.
fn incoming_from_file(
    conn: &Connection,
    node_id: &str,
    file_path: &str,
    shape: &WalkShape,
) -> Result<CallToolResult, ErrorData> {
    let imported_directly = queries::has_incoming_edge(conn, node_id, IMPORT_EDGE)
        .map_err(|e| internal_error("failed to probe a file's importers", e))?;
    if imported_directly {
        return from_root(conn, node_id.to_string(), shape);
    }

    let mut defined = containers::defining_containers(conn, file_path)
        .map_err(|e| internal_error("failed to look up the containers a file defines", e))?;
    match defined.len() {
        0 => from_root(conn, node_id.to_string(), shape),
        1 => {
            let container = defined.pop().expect("len checked above");
            let resolved = ResolvedFrom::module(file_path, container.key);
            from_root_reporting(conn, container.node_id, shape, Some(resolved))
        }
        _ => error(ambiguous_defining_container_message(file_path, &defined)),
    }
}

/// The refusal for a file that defines more than one module, neither of them
/// inside the other - two sibling inline `mod`s and no items of the file's
/// own, say. Guessing would answer about one of them and look exactly like
/// answering about the file, so this names both and lets the caller pick.
/// Not observed on any of the 234 indexed files
/// [`containers::defining_containers`] was measured over; it exists because
/// the alternative to naming a tie is breaking one silently.
fn ambiguous_defining_container_message(file_path: &str, containers: &[DefiningContainer]) -> String {
    let keys: Vec<&str> = containers.iter().map(|c| c.key.as_str()).collect();
    format!(
        "g-mesh: '{file_path}' defines more than one module ({}), and in this language imports \
         name modules rather than files - so there is no single anchor an Incoming walk from this \
         file could mean. Ask about one of them directly.",
        keys.join(", "),
    )
}

/// The message for an exact container key that names more than one
/// container - a real ambiguity (`containers.key` is only unique *within* a
/// language), not a miss-path guess, so this names the languages rather than
/// suggesting a fallback the way [`no_file_message`] does.
fn ambiguous_container_message(key: &str, containers: &[NodeRecord]) -> String {
    let languages: Vec<&str> = containers.iter().map(|n| n.language.as_str()).collect();
    format!(
        "g-mesh: '{key}' names a container in more than one language ({}) - anchor on one of its files \
         instead, so there is no ambiguity about which one is meant.",
        languages.join(", "),
    )
}

/// How many candidates [`entry_point_for`] asks for. Two would do - the rule
/// only ever inspects the first and whether a second is also an entry point -
/// but the same queries back [`no_file_message`], which wants five to list, and
/// one shared number is worth more than one saved row.
const ENTRY_POINT_CANDIDATES: usize = 5;

/// The single indexed file a non-file anchor unambiguously stands for, or
/// `None` when there isn't one.
///
/// WHY THIS ANSWERS RATHER THAN REFUSES
///
/// This tool takes an exact file path and callers ask about packages. Until
/// now that mismatch produced a good error message naming the entry point, and
/// the caller spent a round trip acting on it. Measured over the 2026-08-26
/// five-repetition benchmark sweep, `ex-deps-package-math-incoming` was the
/// only task in the registry where the g-mesh arm made *zero* native calls:
/// two of five repetitions abandoned the tool and grepped the specifier
/// exactly as the grep-only baseline did, and two more spent a whole `Glob`
/// turn discovering the path this function can compute. Naming the entry point
/// and then declining to use it is the part that bought nothing.
///
/// THE RULE, AND WHY IT IS THIS STRICT
///
/// Both underlying queries rank a file matching one of `entry_points` first
/// (`graph::queries::entry_point_rank_expr` - GM-273 generalized what used to
/// be a hardcoded `index.*` check into this parameter, one declared per
/// discovered language rather than one convention baked into the query), so
/// "the first candidate is an entry point and the second is not" is a
/// complete test for *exactly one* even under their row limit. Anything less
/// unanimous - no entry point, or two (a Rust directory can legitimately hold
/// both `mod.rs` and `lib.rs`) - returns `None` and falls through to the
/// refusal, which lists the candidates. Guessing between two entry points
/// would be a worse failure than refusing, because the walk would succeed and
/// answer about the wrong file - this function does not get to break that tie
/// by, say, preferring whichever entry point sorted first in `entry_points`;
/// nothing about declaration order is a meaningful preference between two
/// files that both plausibly are the package's entry point.
///
/// Two forms are tried, in the same order and for the same reasons
/// [`no_file_message`] tries them: the path as a directory prefix
/// (`packages/math`), then its last segment as a directory name
/// (`@excalidraw/math` -> `math`). The second is the weaker inference - a
/// directory sharing a package's name does not prove the package lives there -
/// which is why every substitution is reported back on the response rather
/// than performed silently.
fn entry_point_for(
    conn: &Connection,
    entry_points: &[String],
    requested: &str,
) -> Result<Option<EntryPoint>, ErrorData> {
    let under = queries::find_files_under(conn, requested, entry_points, ENTRY_POINT_CANDIDATES)
        .map_err(|e| internal_error("failed to look up files under a prefix", e))?;
    if let Some(entry) = sole_entry_point(under, entry_points) {
        return Ok(Some(entry));
    }

    let Some(segment) = requested.rsplit('/').next().filter(|s| !s.is_empty() && *s != requested) else {
        return Ok(None);
    };
    let by_segment = queries::find_files_ending_in_dir(conn, segment, entry_points, ENTRY_POINT_CANDIDATES)
        .map_err(|e| internal_error("failed to look up files by directory name", e))?;
    Ok(sole_entry_point(by_segment, entry_points))
}

/// The two fields an anchor substitution needs off the chosen node: the id to
/// walk from, and the path to report. Kept rather than passing `NodeRecord`
/// around because that type is not `Clone`, and taking ownership of two
/// `String`s is the whole of what this needs.
struct EntryPoint {
    id: String,
    file_path: String,
}

/// The first element of an entry-point-first candidate list, but only when it
/// is an entry point and nothing after it is - see [`entry_point_for`] for why
/// the uniqueness half is what makes this safe to act on.
///
/// Takes the vector by value so the chosen node's strings can be moved out
/// rather than copied.
fn sole_entry_point(candidates: Vec<NodeRecord>, entry_points: &[String]) -> Option<EntryPoint> {
    let first = candidates.first()?;
    if !is_entry_point(&first.file_path, entry_points) {
        return None;
    }
    if candidates.get(1).is_some_and(|n| is_entry_point(&n.file_path, entry_points)) {
        return None;
    }
    let chosen = candidates.into_iter().next()?;
    Some(EntryPoint { id: chosen.id, file_path: chosen.file_path })
}

/// Whether `file_path`'s own file name matches one of `entry_points` - the
/// same two-shape rule `graph::queries::entry_point_rank_expr` sorts by
/// (see its doc comment), kept in exact sync so this Rust-side uniqueness
/// check can never disagree with which row the SQL already put first:
///
/// - an entry with no `.` (`"index"`) matches the file's stem under any
///   extension;
/// - an entry with a `.` (`"mod.rs"`) matches the file name exactly, with
///   nothing after it.
fn is_entry_point(file_path: &str, entry_points: &[String]) -> bool {
    let Some(name) = file_path.rsplit('/').next() else { return false };
    entry_points.iter().any(|entry| {
        if entry.contains('.') {
            name == entry.as_str()
        } else {
            name.starts_with(&format!("{entry}."))
        }
    })
}

/// The not-found answer, with what the index can add to it.
///
/// The caller who reaches this usually asked about a *package* or a directory -
/// "which files import from `@excalidraw/math`" - and this tool takes an
/// exact file path. A bare refusal sends them hunting with Glob for the entry
/// point, which costs a round trip: measured on g-mesh-bench as
/// `get_dependencies[57ch] -> Glob -> Glob -> get_dependencies[16267ch]`.
///
/// So when the path is a prefix of files that *are* indexed, this names them,
/// entry point first. Two forms are tried: the path as given
/// (`packages/math`), then its last segment matched as a directory
/// (`@excalidraw/math` -> `math`), which is how a workspace package name
/// relates to its directory in the layouts this meets. The second is offered
/// as a suggestion and nothing more - a name matching a directory does not
/// establish that the package lives there.
///
/// The specifier itself cannot help: `graph::imports` keeps a placeholder
/// `Module` node per import specifier, but only for the ones that never
/// resolved (`react` survives, `@excalidraw/math` became an edge to a file and
/// its placeholder is gone). So there is nothing to look the package name up
/// in, which is why this matches paths rather than pretending otherwise.
fn no_file_message(conn: &Connection, entry_points: &[String], file_path: &str) -> Result<String, ErrorData> {
    const MAX_FILES: usize = 5;
    let terse = format!("g-mesh: no file '{file_path}' found in the index");

    let under = queries::find_files_under(conn, file_path, entry_points, MAX_FILES)
        .map_err(|e| internal_error("failed to look up files under a prefix", e))?;
    if !under.is_empty() {
        return Ok(format!(
            "{terse} - it is not a file. These indexed files sit under it: {}. \
             This tool walks the import graph from one file, so ask about the entry point.",
            paths_of(&under),
        ));
    }

    // `@excalidraw/math` and the like: the last segment is the directory a
    // workspace package usually lives in, but this only suggests, never claims.
    let Some(segment) = file_path.rsplit('/').next().filter(|s| !s.is_empty() && *s != file_path) else {
        return Ok(terse);
    };
    let by_segment = queries::find_files_ending_in_dir(conn, segment, entry_points, MAX_FILES)
        .map_err(|e| internal_error("failed to look up files by directory name", e))?;
    if by_segment.is_empty() {
        return Ok(terse);
    }
    Ok(format!(
        "{terse} - and it is not a path this index carries. If '{segment}' is the package's \
         directory, these indexed files are under one named that: {}. This tool walks the import \
         graph from one file, so ask about the entry point.",
        paths_of(&by_segment),
    ))
}

fn paths_of(nodes: &[NodeRecord]) -> String {
    nodes.iter().map(|n| n.file_path.clone()).collect::<Vec<_>>().join(", ")
}

/// A module id is already a node id, so this lookup buys nothing but the
/// error message: an unknown id would otherwise walk nothing at all and read
/// as "this module imports nothing", which is the one answer a bounded walk
/// must never fake.
///
/// It falls through to a file lookup because callers reliably put something
/// else here. `module_id` reads as "the module's name" and sits next to
/// `file_path` as its documented alternative, so a caller holding
/// `@excalidraw/math` or `packages/math/src/index.ts` puts *that* in it -
/// observed in every recorded run of g-mesh-bench's
/// `ex-deps-package-math-incoming`, which then cost a refusal, a blind Glob
/// and a second call to get the answer the first one had the input for. A
/// path that this index carries is an answerable question however the caller
/// labelled it, and refusing it on a technicality buys nothing.
fn from_module(
    conn: &Connection,
    entry_points: &[String],
    module_id: &str,
    shape: &WalkShape,
) -> Result<CallToolResult, ErrorData> {
    let anchor =
        queries::get_node(conn, module_id).map_err(|e| internal_error("failed to look up module", e))?;

    match anchor {
        Some(node) => from_root(conn, node.id, shape),
        None => match queries::find_file_node(conn, module_id)
            .map_err(|e| internal_error("failed to look up file", e))?
        {
            Some(node) => from_root(conn, node.id, shape),
            None => error(no_file_message(conn, entry_points, module_id)?),
        },
    }
}

/// Continues a walk the exploration budget cut short. The token carries the
/// anchor, direction and limits of the walk it continues, so nothing about
/// its shape is re-read from the parameters here.
fn continued(conn: &Connection, token: &str) -> Result<CallToolResult, ErrorData> {
    // Decoded a second time here (`traversal::resume` decodes its own copy
    // internally) purely to read the walk's shape back out for `bound_walk` -
    // cheap, and keeps `traversal`'s public surface free of a getter that
    // exists for one caller.
    let state =
        resume_token::decode(token).map_err(|e| internal_error("failed to decode resume token", e))?;
    let ResumeState {
        direction,
        edge_kind,
        max_depth,
        max_fanout,
        visited: prior_visited,
        walked: prior_walked,
    } = state;

    let result = traversal::resume(conn, token, traversal::DEFAULT_EXPLORATION_BUDGET)
        .map_err(|e| internal_error("failed to resume the import walk", e))?;
    success(&bound_walk(result, direction, edge_kind, max_depth, max_fanout, prior_visited, prior_walked))
}

/// `entry_points` is the union of every discovered plugin's
/// `[plugin.workspace] entry_points` - see `daemon::registry::PluginRegistry::entry_points`
/// and `graph::queries::entry_point_rank_expr` for where it comes from and
/// how it is used. The caller (`mcp::GMeshMcpServer::get_dependencies`) reads
/// it off its own `PluginRegistry` once per call, since discovery never
/// changes while a daemon runs (see `daemon::manifest::discover`'s own
/// contract) - there is nothing this function would gain by asking twice.
pub(crate) fn handle(
    conn: &Arc<Mutex<Connection>>,
    entry_points: &[String],
    params: GetDependenciesParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();
    let GetDependenciesParams { file_path, module_id, direction, max_depth, max_fanout, resume_token } =
        params;
    let shape = WalkShape { direction, max_depth, max_fanout };

    match (resume_token, file_path, module_id) {
        (Some(token), None, None) => continued(&conn, &token),
        // An anchor next to a token is a contradiction, not a preference to
        // resolve silently: the token already names the walk it continues,
        // and picking one of the two would answer a question nobody asked.
        (Some(_), _, _) => {
            error("g-mesh: `resume_token` already carries the walk it continues - call it without `file_path`/`module_id`")
        }
        (None, Some(file_path), None) => from_file(&conn, entry_points, &file_path, &shape),
        (None, None, Some(module_id)) => from_module(&conn, entry_points, &module_id, &shape),
        (None, Some(_), Some(_)) => error("g-mesh: give either `file_path` or `module_id`, not both"),
        (None, None, None) => error("g-mesh: give either `file_path` or `module_id` to start from"),
    }
}

#[cfg(test)]
mod tests;
