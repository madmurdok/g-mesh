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
mod tests {
    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::{self, Diff, EdgeRecord, NodeRecord};

    /// What a real daemon feeds `handle`/`from_file`/`from_module`/
    /// `no_file_message` in the bundled, TS-only setup: the bundled plugin's
    /// own manifest declares `entry_points = ["index"]`
    /// (`plugins/typescript/plugin.toml`), so this is the one list that
    /// reproduces the pre-GM-273 hardcoded `index.*` behaviour exactly.
    fn ts_entry_points() -> Vec<String> {
        vec!["index".to_string()]
    }

    /// Shadows [`super::handle`] for every test below that does not care
    /// about entry points at all, or wants the bundled-TS-setup default -
    /// see [`ts_entry_points`]. A test exercising a different declared set
    /// (a fake Rust manifest, an empty one) calls `super::handle` directly
    /// instead of this wrapper.
    fn handle(
        conn: &Arc<Mutex<Connection>>,
        params: GetDependenciesParams,
    ) -> Result<CallToolResult, ErrorData> {
        super::handle(conn, &ts_entry_points(), params)
    }

    /// [`handle`]'s own shadow, for [`super::from_file`].
    fn from_file(conn: &Connection, file_path: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
        super::from_file(conn, &ts_entry_points(), file_path, shape)
    }

    /// [`handle`]'s own shadow, for [`super::from_module`].
    fn from_module(
        conn: &Connection,
        module_id: &str,
        shape: &WalkShape,
    ) -> Result<CallToolResult, ErrorData> {
        super::from_module(conn, &ts_entry_points(), module_id, shape)
    }

    /// [`handle`]'s own shadow, for [`super::no_file_message`].
    fn no_file_message(conn: &Connection, file_path: &str) -> Result<String, ErrorData> {
        super::no_file_message(conn, &ts_entry_points(), file_path)
    }

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn json_body(result: &CallToolResult) -> serde_json::Value {
        assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => serde_json::from_str(&text.text).unwrap(),
            other => panic!("expected text/json content, got {other:?}"),
        }
    }

    fn error_text(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected an error result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    fn file(path: &str) -> NodeRecord {
        NodeRecord::new(path, "File", path, path, path, "rust")
    }

    /// `from` imports `to`, i.e. the edge points the way the dependency does.
    fn imports(conn: &mut Connection, from: &str, to: &str) {
        upsert_edge(
            conn,
            EdgeRecord::new(format!("e_{from}_{to}"), from, to, "IMPORTS", "tree-sitter", true),
        )
        .unwrap();
    }

    /// a.rs -> b.rs -> c.rs, the chain both direction tests read in opposite
    /// ways.
    fn import_chain() -> Connection {
        let mut conn = setup();
        for path in ["a.rs", "b.rs", "c.rs"] {
            upsert_node(&mut conn, file(path)).unwrap();
        }
        imports(&mut conn, "a.rs", "b.rs");
        imports(&mut conn, "b.rs", "c.rs");
        conn
    }

    /// An import nothing could be linked to, stored the way the js-ts
    /// extractor stores it: a `Module` node whose `filePath` is the
    /// *importing* file, because that is where the specifier is written.
    fn unresolved_import(importer: &str, specifier: &str) -> NodeRecord {
        NodeRecord::new(format!("mod_{specifier}"), MODULE_KIND, specifier, specifier, importer, "typescript")
    }

    fn anchored_at(file_path: &str, direction: Direction) -> GetDependenciesParams {
        GetDependenciesParams {
            file_path: Some(file_path.to_string()),
            module_id: None,
            direction,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        }
    }

    /// (id, depth) per result row, in the order the walk reported them.
    fn reached(body: &serde_json::Value) -> Vec<(String, u64)> {
        body["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), r["depth"].as_u64().unwrap()))
            .collect()
    }

    /// Acceptance criteria: a three-file import chain comes back whole, not
    /// one hop of it - this is the only tool that walks past its own anchor.
    #[test]
    fn an_import_chain_comes_back_transitively_with_the_hop_count_per_node() {
        let conn = import_chain();

        let result = handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap();
        let body = json_body(&result);

        assert_eq!(reached(&body), vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]);
        assert_eq!(body["truncated"], false);
        assert!(body["truncatedBy"].is_null());
        assert_eq!(body["frontierNodes"].as_array().unwrap().len(), 0);
        assert!(body["resumeToken"].is_null());
    }

    /// The same chain read the other way: from its far end, `Incoming`
    /// reaches the importers, and the two directions must not agree.
    #[test]
    fn incoming_walks_the_importers_and_outgoing_the_imports() {
        let conn = Arc::new(Mutex::new(import_chain()));

        let upstream = json_body(&handle(&conn, anchored_at("c.rs", Direction::Incoming)).unwrap());
        assert_eq!(reached(&upstream), vec![("b.rs".to_string(), 1), ("a.rs".to_string(), 2)]);

        let downstream = json_body(&handle(&conn, anchored_at("c.rs", Direction::Outgoing)).unwrap());
        assert_eq!(reached(&downstream), vec![], "nothing imports out of the end of the chain");
        assert_eq!(downstream["truncated"], false, "an empty walk is complete, not truncated");
    }

    /// The anchor is what the caller already named; repeating it in the
    /// results would only make "how far away is this" ambiguous.
    #[test]
    fn the_anchor_itself_is_not_reported_as_its_own_dependency() {
        let conn = import_chain();
        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap(),
        );

        let ids: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert!(
            !ids.contains(&"a.rs"),
            "the depth-0 anchor must not appear among its own dependencies: {ids:?}"
        );
    }

    /// A placeholder must not borrow the importing file's path on the way
    /// out: "zod lives in a.rs" is both untrue and indistinguishable from
    /// a.rs's own row in the same walk.
    #[test]
    fn an_unresolved_import_is_reported_without_a_file_path_of_its_own() {
        let mut conn = import_chain();
        upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
        imports(&mut conn, "a.rs", "mod_zod");

        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap(),
        );
        let rows = body["results"].as_array().unwrap();

        let module =
            rows.iter().find(|r| r["kind"] == "Module").expect("the placeholder is still a dependency");
        assert!(module["filePath"].is_null(), "a module placeholder has no file of its own: {module}");
        assert_eq!(module["qualifiedName"], "zod", "the specifier is all there is left to act on");

        let files: Vec<&str> =
            rows.iter().filter(|r| r["kind"] == "File").map(|r| r["filePath"].as_str().unwrap()).collect();
        assert_eq!(files, vec!["b.rs", "c.rs"], "real files are still addressed by their own path");
    }

    /// A `File`-kind row's `qualifiedName` is byte-identical to its own
    /// `filePath` by construction (see `pagination::FILE_KIND`'s doc comment),
    /// so it must be omitted from the wire JSON entirely rather than repeat
    /// the same path string twice. A `Module` placeholder has no `filePath`
    /// of its own, so it keeps `qualifiedName` as the only field carrying the
    /// specifier - the mirror image of the previous test.
    #[test]
    fn a_file_kind_row_omits_qualified_name_a_module_row_keeps_it() {
        let mut conn = import_chain();
        upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
        imports(&mut conn, "a.rs", "mod_zod");

        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap(),
        );
        let rows = body["results"].as_array().unwrap();

        let files: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "File").collect();
        assert!(!files.is_empty());
        for file in files {
            assert!(
                file.get("qualifiedName").is_none(),
                "a File row must not repeat its own filePath as qualifiedName: {file}"
            );
        }

        let module =
            rows.iter().find(|r| r["kind"] == "Module").expect("the placeholder is still a dependency");
        assert_eq!(
            module["qualifiedName"], "zod",
            "a Module row has no filePath, so qualifiedName must stay"
        );
    }

    /// `name` never carries information `qualifiedName` doesn't already have
    /// (at worst a shorter, less unique view of the same symbol) - dropped
    /// entirely from every row, real file and unresolved-module placeholder
    /// alike.
    #[test]
    fn no_row_carries_a_name_field() {
        let mut conn = import_chain();
        upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
        imports(&mut conn, "a.rs", "mod_zod");

        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap(),
        );
        let rows = body["results"].as_array().unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            assert!(row.get("name").is_none(), "the name field must never be present on any row: {row}");
        }
    }

    #[test]
    fn only_import_edges_are_walked() {
        let mut conn = import_chain();
        upsert_node(&mut conn, file("d.rs")).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e_call", "a.rs", "d.rs", "CALLS", "tree-sitter", true))
            .unwrap();

        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap(),
        );

        let ids: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["b.rs", "c.rs"], "a CALLS edge is not a dependency: {ids:?}");
    }

    #[test]
    fn a_module_id_anchors_the_walk_without_a_path_lookup() {
        let conn = import_chain();
        let params = GetDependenciesParams {
            file_path: None,
            module_id: Some("a.rs".to_string()),
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        };

        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
        assert_eq!(reached(&body), vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]);
    }

    #[test]
    fn an_unknown_anchor_is_a_tool_level_error_rather_than_an_empty_walk() {
        let conn = Arc::new(Mutex::new(import_chain()));

        let by_path = handle(&conn, anchored_at("does/not/exist.rs", Direction::Outgoing)).unwrap();
        assert!(error_text(&by_path).contains("does/not/exist.rs"));

        let by_module = GetDependenciesParams {
            file_path: None,
            module_id: Some("no_such_module".to_string()),
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        };
        assert!(error_text(&handle(&conn, by_module).unwrap()).contains("no_such_module"));
    }

    #[test]
    fn every_bad_anchor_combination_is_its_own_tool_level_error() {
        let conn = Arc::new(Mutex::new(import_chain()));
        let base = || GetDependenciesParams {
            file_path: None,
            module_id: None,
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: None,
        };

        let neither = handle(&conn, base()).unwrap();
        assert!(error_text(&neither).contains("file_path"));

        let both = GetDependenciesParams {
            file_path: Some("a.rs".to_string()),
            module_id: Some("a.rs".to_string()),
            ..base()
        };
        assert!(error_text(&handle(&conn, both).unwrap()).contains("not both"));

        let token_and_anchor = GetDependenciesParams {
            file_path: Some("a.rs".to_string()),
            resume_token: Some("whatever".to_string()),
            ..base()
        };
        assert!(error_text(&handle(&conn, token_and_anchor).unwrap()).contains("resume_token"));
    }

    /// Truncation contract, cause one: the walk stopped at the depth limit,
    /// so the caller gets the boundary to re-root on and nothing else.
    #[test]
    fn a_depth_cut_reports_max_depth_and_hands_back_only_the_frontier() {
        let mut conn = setup();
        for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            upsert_node(&mut conn, file(path)).unwrap();
        }
        imports(&mut conn, "a.rs", "b.rs");
        imports(&mut conn, "b.rs", "c.rs");
        imports(&mut conn, "c.rs", "d.rs");

        let params = GetDependenciesParams { max_depth: Some(1), ..anchored_at("a.rs", Direction::Outgoing) };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());

        assert_eq!(reached(&body), vec![("b.rs".to_string(), 1)]);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "maxDepth");
        assert_eq!(
            body["frontierNodes"],
            serde_json::json!(["b.rs"]),
            "the level to re-root the same call on"
        );
        assert!(body["resumeToken"].is_null(), "a depth cut is re-rooted, not resumed");
    }

    /// Cause two: a node had more imports than the fan-out cap. Deliberately
    /// no extra field - the caller re-queries that node with the single-hop
    /// tools' cursor pagination, which already exists.
    #[test]
    fn a_fanout_cut_reports_max_fanout_and_hands_back_no_continuation_field() {
        let mut conn = setup();
        upsert_node(&mut conn, file("a.rs")).unwrap();
        for path in ["b.rs", "c.rs", "d.rs"] {
            upsert_node(&mut conn, file(path)).unwrap();
            imports(&mut conn, "a.rs", path);
        }

        let params =
            GetDependenciesParams { max_fanout: Some(1), ..anchored_at("a.rs", Direction::Outgoing) };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());

        assert_eq!(body["results"].as_array().unwrap().len(), 1, "one of the three imports, and a warning");
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "maxFanout");
        assert_eq!(
            body["frontierNodes"].as_array().unwrap().len(),
            0,
            "a fanout cut is paginated, not re-rooted"
        );
        assert!(body["resumeToken"].is_null());
    }

    /// Cause three, and the only one with state to carry: the internal
    /// budget - not a caller-facing limit - stops the walk mid-way, and the
    /// token it hands back continues it exactly where it left off.
    ///
    /// `DEFAULT_EXPLORATION_BUDGET` rows (5000) of `DependencyNode` JSON is
    /// nowhere near `pagination::MAX_RESPONSE_BYTES` (20,000 bytes), so a
    /// walk wide enough to hit the exploration budget always earns its own,
    /// stricter `responseSize` cut before `explorationBudget` is ever visible
    /// on the wire - see `bound_walk`'s doc comment. That row-count-scale
    /// case is exercised directly at the `traversal` layer instead
    /// (`graph::traversal::tests::exploration_budget_caps_visited_rows_...`,
    /// `..._a_resume_chain_covers_the_whole_walk_exactly_once`); what this
    /// tool-level test proves is the same "no continuation is dropped or
    /// double-counted" property one layer up, at the response-size scale
    /// (`bound_walk`'s `prior_visited`/`prior_walked` accumulation) that
    /// callers actually see.
    #[test]
    fn a_response_size_cut_is_continued_by_its_token_and_the_chain_covers_everything_once() {
        // Comfortably past what one response can return, comfortably short
        // of the exploration budget - so nothing but the byte cap can be
        // what's cutting each call in this chain.
        let wide = 600;
        let mut conn = setup();
        let mut diff = Diff { upsert_nodes: vec![file("a.rs")], ..Default::default() };
        for i in 0..wide {
            let path = format!("dep{i:05}.rs");
            diff.upsert_edges.push(EdgeRecord::new(
                format!("e{i:05}"),
                "a.rs",
                &path,
                "IMPORTS",
                "tree-sitter",
                true,
            ));
            diff.upsert_nodes.push(file(&path));
        }
        write::apply_diff(&mut conn, &diff).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params =
            GetDependenciesParams { max_fanout: Some(10_000), ..anchored_at("a.rs", Direction::Outgoing) };
        let first = json_body(&handle(&conn, params).unwrap());

        let first_len = first["results"].as_array().unwrap().len();
        assert!(
            first_len > 0 && first_len < wide,
            "one response must not hold all {wide} dependencies: {first_len}"
        );
        assert_eq!(first["truncated"], true);
        assert_eq!(first["truncatedBy"], "responseSize");
        assert_eq!(
            first["frontierNodes"].as_array().unwrap().len(),
            0,
            "a size cut is resumed, not re-rooted"
        );

        let mut all: Vec<String> = first["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        let mut token = first["resumeToken"].as_str().map(str::to_string);
        let mut calls = 1;

        while let Some(t) = token {
            let resumed = GetDependenciesParams {
                file_path: None,
                module_id: None,
                // Ignored on a continuation: the token carries the walk's shape.
                direction: Direction::Incoming,
                max_depth: None,
                max_fanout: None,
                resume_token: Some(t),
            };
            let body = json_body(&handle(&conn, resumed).unwrap());
            calls += 1;
            all.extend(
                body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()),
            );
            token = body["resumeToken"].as_str().map(str::to_string);
            assert!(
                calls < 50,
                "the chain must converge, not re-explore itself forever: {calls} calls so far"
            );
        }

        assert!(
            calls > 2,
            "a page far smaller than {wide} deps must take more than one resume: only {calls} calls"
        );

        let mut deduped = all.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), all.len(), "no dependency may be returned twice across the chain");
        assert_eq!(deduped.len(), wide, "the whole chain's union must be every dependency, exactly once");
    }

    /// Reproduces the shape of the two real `get_dependencies` failures in
    /// g-mesh-bench's v0.4.0 outlier findings (a shared module's `Incoming`
    /// fan-in producing a 115,863-character response the MCP client's
    /// transport rejected outright) as a synthetic fixture: a single file
    /// many other files import, none of it anywhere near the exploration
    /// budget or a caller-set `max_fanout`, but still too much JSON for one
    /// response.
    #[test]
    fn a_wide_fan_in_too_big_for_one_response_truncates_with_a_resume_token_instead_of_erroring() {
        let wide = 400;
        let mut conn = setup();
        let core = "packages/core/src/index.ts";
        let mut diff = Diff { upsert_nodes: vec![file(core)], ..Default::default() };
        for i in 0..wide {
            let path = format!("packages/consumer{i:05}/src/index.ts");
            diff.upsert_edges.push(EdgeRecord::new(
                format!("e{i:05}"),
                &path,
                core,
                "IMPORTS",
                "tree-sitter",
                true,
            ));
            diff.upsert_nodes.push(file(&path));
        }
        write::apply_diff(&mut conn, &diff).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params =
            GetDependenciesParams { max_fanout: Some(10_000), ..anchored_at(core, Direction::Incoming) };
        let body = json_body(&handle(&conn, params).unwrap());

        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty(), "at least one row must always come back, even under an oversized level");
        assert!(results.len() < wide, "the full {wide}-wide fan-in must not fit in one response");
        assert_eq!(body["truncated"], true);
        assert_eq!(body["truncatedBy"], "responseSize");
        let raw_len = serde_json::to_vec(results).unwrap().len();
        assert!(
            raw_len <= pagination::MAX_RESPONSE_BYTES,
            "the truncated page itself must respect the budget: {raw_len}"
        );

        let token = body["resumeToken"].as_str().expect("a size cut must carry a resume token").to_string();
        let resumed = GetDependenciesParams {
            file_path: None,
            module_id: None,
            direction: Direction::Outgoing,
            max_depth: None,
            max_fanout: None,
            resume_token: Some(token),
        };
        let second = json_body(&handle(&conn, resumed).unwrap());
        assert!(
            !second["results"].as_array().unwrap().is_empty(),
            "resuming must make forward progress on what the first call dropped"
        );
    }

    /// Problem 2's fix: omitting `max_depth` must stop at this tool's own,
    /// stricter default - not fall through to the walk engine's generic one
    /// (`traversal::DEFAULT_MAX_DEPTH`, 5). A caller that passes `max_depth`
    /// explicitly must still get exactly that depth, unaffected.
    #[test]
    fn omitting_max_depth_uses_this_tools_own_default_not_the_walk_engines() {
        let mut conn = setup();
        let chain = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
        for path in chain {
            upsert_node(&mut conn, file(path)).unwrap();
        }
        for pair in chain.windows(2) {
            imports(&mut conn, pair[0], pair[1]);
        }
        let conn = Arc::new(Mutex::new(conn));

        let defaulted = json_body(&handle(&conn, anchored_at("a.rs", Direction::Outgoing)).unwrap());
        assert_eq!(
            reached(&defaulted),
            vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)],
            "omitting max_depth must stop at DEFAULT_MAX_DEPTH (2), not the walk engine's default (5)"
        );
        assert_eq!(defaulted["truncated"], true);
        assert_eq!(defaulted["truncatedBy"], "maxDepth");
        assert_eq!(defaulted["frontierNodes"], serde_json::json!(["c.rs"]));

        let explicit =
            GetDependenciesParams { max_depth: Some(4), ..anchored_at("a.rs", Direction::Outgoing) };
        let body = json_body(&handle(&conn, explicit).unwrap());
        assert_eq!(
            reached(&body),
            vec![
                ("b.rs".to_string(), 1),
                ("c.rs".to_string(), 2),
                ("d.rs".to_string(), 3),
                ("e.rs".to_string(), 4)
            ],
            "an explicit max_depth must be honored exactly, unaffected by this tool's own default"
        );
        assert_eq!(body["truncated"], false);
    }

    /// The caller asked about a package or a directory, which is what the
    /// prompt they are answering names. A bare refusal sends them hunting with
    /// Glob for the entry point - a round trip, and the recorded trace for
    /// `ex-deps-package-math-incoming` is exactly that hunt.
    #[test]
    fn a_directory_prefix_is_told_which_indexed_files_sit_under_it() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/angle.ts")).unwrap();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

        let message = no_file_message(&conn, "packages/math").unwrap();

        assert!(message.contains("packages/math/src/index.ts"), "{message}");
        // Entry point first: it is what a package specifier resolves to, and
        // what the caller is going to ask about next.
        let idx = message.find("packages/math/src/index.ts").unwrap();
        let other = message.find("packages/math/src/angle.ts").unwrap();
        assert!(idx < other, "the entry point must lead: {message}");
    }

    /// A workspace package name is not a path at all, so the only handle is
    /// its last segment matching a directory - offered as a suggestion, since
    /// a directory of that name does not establish the package lives there.
    #[test]
    fn a_package_name_is_offered_the_directory_that_shares_its_last_segment() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

        let message = no_file_message(&conn, "@excalidraw/math").unwrap();

        assert!(message.contains("packages/math/src/index.ts"), "{message}");
        assert!(message.contains("If 'math' is"), "must read as a suggestion: {message}");
    }

    /// A path matching nothing keeps the short answer. The explanation is only
    /// worth its length where there is something to explain.
    #[test]
    fn a_path_under_which_nothing_is_indexed_keeps_the_terse_answer() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

        let message = no_file_message(&conn, "packages/nowhere").unwrap();

        assert_eq!(message, "g-mesh: no file 'packages/nowhere' found in the index");
    }

    /// Callers put a path in `module_id` - the field reads as "the module's
    /// name" and is documented as the alternative to `file_path`. Every
    /// recorded run of the benchmark task that asks about a package did it,
    /// and paid a refusal plus a blind Glob for the label.
    #[test]
    fn a_path_passed_as_a_module_id_is_answered_rather_than_refused() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
        upsert_node(&mut conn, file("packages/excalidraw/viewport.ts")).unwrap();
        imports(&mut conn, "packages/excalidraw/viewport.ts", "packages/math/src/index.ts");

        let result = from_module(
            &conn,
            "packages/math/src/index.ts",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap();

        let body = json_body(&result);
        assert_eq!(body["results"][0]["filePath"], "packages/excalidraw/viewport.ts");
    }

    /// GM-259, the measured case. `ex-deps-package-math-incoming` was the only
    /// registry task where the g-mesh arm made zero native calls: two of five
    /// repetitions grepped the specifier exactly as the grep-only baseline
    /// did, and two more spent a `Glob` turn finding the path this resolves.
    #[test]
    fn a_package_specifier_with_one_entry_point_is_answered_not_refused() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
        upsert_node(&mut conn, file("packages/math/src/point.ts")).unwrap();
        upsert_node(&mut conn, file("packages/excalidraw/viewport.ts")).unwrap();
        imports(&mut conn, "packages/excalidraw/viewport.ts", "packages/math/src/index.ts");

        let result = from_file(
            &conn,
            "@excalidraw/math",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap();

        let body = json_body(&result);
        assert_eq!(body["results"][0]["filePath"], "packages/excalidraw/viewport.ts");
        assert_eq!(
            body["resolvedFrom"]["requested"], "@excalidraw/math",
            "the substitution has to be visible - the tool answered a question adjacent to the one asked",
        );
        assert_eq!(body["resolvedFrom"]["filePath"], "packages/math/src/index.ts");
    }

    /// The directory-prefix form, which is the stronger of the two inferences:
    /// the caller named a real path, it just is not a file.
    #[test]
    fn a_directory_with_one_entry_point_is_answered_too() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/index.ts")).unwrap();
        upsert_node(&mut conn, file("packages/math/point.ts")).unwrap();
        upsert_node(&mut conn, file("app/viewport.ts")).unwrap();
        imports(&mut conn, "app/viewport.ts", "packages/math/index.ts");

        let body = json_body(
            &from_file(
                &conn,
                "packages/math",
                &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert_eq!(body["results"][0]["filePath"], "app/viewport.ts");
        assert_eq!(body["resolvedFrom"]["filePath"], "packages/math/index.ts");
    }

    /// GM-273's acceptance case at the tool level: a fake manifest declaring
    /// `entry_points = ["mod.rs"]` - Rust's own convention, not TypeScript's
    /// `"index"` - must resolve a directory lookup to `mod.rs` the same way
    /// `a_directory_with_one_entry_point_is_answered_too` resolves one to
    /// `index.ts`. Calls `super::from_file` directly (not the `ts_entry_points`
    /// shadow above) precisely because this is the one test that must NOT get
    /// the bundled-TS default.
    #[test]
    fn a_directory_with_one_declared_rust_entry_point_is_answered_too() {
        let mut conn = setup();
        upsert_node(&mut conn, file("crates/math/mod.rs")).unwrap();
        upsert_node(&mut conn, file("crates/math/point.rs")).unwrap();
        // Shorter than "crates/math/mod.rs" - if entry-point rank did not
        // decide the order, `LENGTH(filePath)` would put this one first
        // instead, and the substitution below would not happen at all.
        upsert_node(&mut conn, file("crates/math/x.rs")).unwrap();
        upsert_node(&mut conn, file("app/main.rs")).unwrap();
        imports(&mut conn, "app/main.rs", "crates/math/mod.rs");

        let body = json_body(
            &super::from_file(
                &conn,
                &["mod.rs".to_string()],
                "crates/math",
                &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert_eq!(body["results"][0]["filePath"], "app/main.rs");
        assert_eq!(
            body["resolvedFrom"]["filePath"], "crates/math/mod.rs",
            "mod.rs must be the file the walk actually started from: {body}"
        );
    }

    /// A directory declaring both of a Rust crate root's two conventional
    /// entry points (`mod.rs` and `lib.rs`) is exactly the "more than one
    /// entry point" case `entry_point_for`'s doc comment calls out by name -
    /// still refused, not guessed at, the same rule
    /// `two_entry_points_still_refuse_and_list_the_candidates` proves for TS.
    #[test]
    fn two_declared_rust_entry_points_in_one_directory_still_refuse() {
        let mut conn = setup();
        upsert_node(&mut conn, file("crates/math/mod.rs")).unwrap();
        upsert_node(&mut conn, file("crates/math/lib.rs")).unwrap();

        let message = error_text(
            &super::from_file(
                &conn,
                &["mod.rs".to_string(), "lib.rs".to_string()],
                "crates/math",
                &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert!(message.contains("crates/math/mod.rs"), "the candidates are still named: {message}");
        assert!(message.contains("crates/math/lib.rs"), "both of them: {message}");
    }

    /// Two entry points is the case where answering would be worse than
    /// refusing: the walk would succeed and describe the wrong file. The old
    /// error, which lists the candidates, is the right outcome.
    #[test]
    fn two_entry_points_still_refuse_and_list_the_candidates() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/index.ts")).unwrap();
        upsert_node(&mut conn, file("packages/math/sub/index.ts")).unwrap();

        let message = error_text(
            &from_file(
                &conn,
                "packages/math",
                &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert!(message.contains("packages/math/index.ts"), "the candidates are still named: {message}");
        assert!(message.contains("packages/math/sub/index.ts"), "both of them: {message}");
    }

    /// No entry point at all - a directory of ordinary modules. Picking the
    /// shortest path would be a guess with nothing behind it.
    #[test]
    fn a_directory_without_an_entry_point_is_not_guessed_at() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/point.ts")).unwrap();
        upsert_node(&mut conn, file("packages/math/vector.ts")).unwrap();

        let message = error_text(
            &from_file(
                &conn,
                "packages/math",
                &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert!(message.contains("no file 'packages/math' found"), "{message}");
    }

    /// An ordinary, exact anchor must stay exactly as it was - including
    /// paying no bytes for a field about a substitution that did not happen.
    #[test]
    fn an_exact_file_anchor_reports_no_substitution() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
        upsert_node(&mut conn, file("app/viewport.ts")).unwrap();
        imports(&mut conn, "app/viewport.ts", "packages/math/src/index.ts");

        let result = from_file(
            &conn,
            "packages/math/src/index.ts",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap();
        let raw = json_body(&result).to_string();

        assert_eq!(json_body(&result)["results"][0]["filePath"], "app/viewport.ts");
        assert!(!raw.contains("resolvedFrom"), "no substitution, no field: {raw}");
    }

    // -----------------------------------------------------------------
    // Containers (GM-267): `File -IMPORTS-> container` edges, walked and
    // anchored on. Built directly at the storage layer - `graph::imports`'s
    // own tests (`graph::imports::tests`) cover the *linking* of a
    // container-scoped placeholder onto one of these edges; this module
    // tests the walk and the anchor resolution once the edge exists, exactly
    // the split the file-import tests above already follow (`imports` builds
    // a resolved edge directly rather than going through the linker).
    // -----------------------------------------------------------------

    fn container_member(id: &str, language: &str, key: &str) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, format!("{key}/{id}.x"), language);
        node.container = Some(key.to_string());
        node
    }

    /// Materializes container `key` (in `language`) with one member - the
    /// minimal fixture `graph::containers::attach` needs - and returns the
    /// container's own node id.
    fn materialize_container(conn: &mut Connection, language: &str, key: &str) -> String {
        upsert_node(conn, container_member(&format!("member:{language}:{key}"), language, key)).unwrap();
        crate::graph::containers::container_id(language, key)
    }

    /// `from` (a File) imports the container at `container_node_id`,
    /// directly - the walk-time shape `graph::imports` produces after
    /// linking a container-scoped placeholder, built without the linker for
    /// the same reason [`imports`] builds a file-to-file edge directly.
    fn imports_container(conn: &mut Connection, from: &str, container_node_id: &str) {
        upsert_edge(
            conn,
            EdgeRecord::new(
                format!("e_{from}_{container_node_id}"),
                from,
                container_node_id,
                "IMPORTS",
                "tree-sitter",
                true,
            ),
        )
        .unwrap();
    }

    /// Acceptance: "Outgoing from a file lists containers as well as files."
    /// Decision 5's row shape, exercised end to end: a container row's
    /// `qualifiedName` carries its key (`ensure_container` writes the key as
    /// both `name` and `qualifiedName`), and its `filePath` is `null` rather
    /// than a fabricated path - the same shape an unresolved import
    /// placeholder's row already has (`an_unresolved_import_is_reported_
    /// without_a_file_path_of_its_own` above), at zero extra bytes: no new
    /// field, because `DependencyNode::from` already branches on `kind !=
    /// MODULE_KIND` for `file_path` and `kind == FILE_KIND` for
    /// `qualified_name`, and a container node's stored `kind` is `"Module"`
    /// (`graph::containers::ensure_container`) - the same branch a
    /// placeholder already took.
    #[test]
    fn outgoing_from_a_file_lists_a_container_alongside_files() {
        let mut conn = setup();
        upsert_node(&mut conn, file("main.go")).unwrap();
        upsert_node(&mut conn, file("other.go")).unwrap();
        imports(&mut conn, "main.go", "other.go");
        let container_id = materialize_container(&mut conn, "go", "github.com/x/pkg");
        imports_container(&mut conn, "main.go", &container_id);

        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("main.go", Direction::Outgoing)).unwrap(),
        );
        let rows = body["results"].as_array().unwrap();

        let container_row =
            rows.iter().find(|r| r["id"] == container_id).expect("the container must be a result row");
        assert_eq!(container_row["kind"], "Module");
        assert_eq!(
            container_row["qualifiedName"], "github.com/x/pkg",
            "decision 5: a container row names its key, not a path"
        );
        assert!(
            container_row["filePath"].is_null(),
            "decision 5: no fabricated filePath for a node with none: {container_row}"
        );

        let files: Vec<&str> =
            rows.iter().filter(|r| r["kind"] == "File").map(|r| r["filePath"].as_str().unwrap()).collect();
        assert_eq!(files, vec!["other.go"], "an ordinary file dependency is unaffected");
    }

    /// Acceptance: "Incoming get_dependencies on a container returns
    /// importing files" - anchoring directly on an exact container key
    /// (decision 4), which is not a substitution and so carries no
    /// `resolvedFrom` (unlike the miss-path/`entry_point_for` case exercised
    /// by `a_package_specifier_with_one_entry_point_is_answered_not_refused`
    /// above).
    #[test]
    fn incoming_on_a_container_key_returns_the_importing_files() {
        let mut conn = setup();
        upsert_node(&mut conn, file("a.go")).unwrap();
        upsert_node(&mut conn, file("b.go")).unwrap();
        let container_id = materialize_container(&mut conn, "go", "github.com/x/pkg");
        imports_container(&mut conn, "a.go", &container_id);
        imports_container(&mut conn, "b.go", &container_id);

        let result = from_file(
            &conn,
            "github.com/x/pkg",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap();
        let body = json_body(&result);

        let mut files: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        files.sort_unstable();
        assert_eq!(files, vec!["a.go", "b.go"]);
        assert!(
            !body.to_string().contains("resolvedFrom"),
            "an exact container key is a direct anchor, not a substitution (decision 4): {body}"
        );
    }

    /// Decision 4's refusal case: a key that names a container in more than
    /// one language is a real ambiguity (`containers.key` is only unique
    /// *within* a language), refused with the candidates named rather than
    /// guessed at - the same stance `two_entry_points_still_refuse_and_list_
    /// the_candidates` already takes for two file candidates.
    #[test]
    fn a_container_key_ambiguous_across_languages_is_refused_with_the_languages_named() {
        let mut conn = setup();
        materialize_container(&mut conn, "go", "shared");
        materialize_container(&mut conn, "rust", "shared");

        let message = error_text(
            &from_file(
                &conn,
                "shared",
                &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert!(message.contains("go"), "{message}");
        assert!(message.contains("rust"), "{message}");
    }

    /// Decision 6: two containers with the storage-level `filePath = ''` in
    /// common must not collapse into a single result row - a walk-level
    /// version of `graph::imports::tests::two_containers_imported_by_the_
    /// same_file_both_link_independently`, at the layer (`ReachedNode`/
    /// `DependencyNode`) where a `filePath`-keyed dedup would actually bite
    /// if one existed. `traversal::traverse` dedups by node id
    /// (`seen_nodes: HashSet<String>` keyed on `node.id`), never by
    /// `filePath`, so this passed without any code change - it is coverage
    /// for that fact, not a fix.
    #[test]
    fn two_containers_imported_by_the_same_file_are_two_separate_rows() {
        let mut conn = setup();
        upsert_node(&mut conn, file("main.go")).unwrap();
        let a = materialize_container(&mut conn, "go", "pkg/a");
        let b = materialize_container(&mut conn, "go", "pkg/b");
        imports_container(&mut conn, "main.go", &a);
        imports_container(&mut conn, "main.go", &b);

        let body = json_body(
            &handle(&Arc::new(Mutex::new(conn)), anchored_at("main.go", Direction::Outgoing)).unwrap(),
        );
        let ids: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();

        assert_eq!(ids.len(), 2, "two distinct containers must not collapse into one row: {ids:?}");
        assert!(ids.contains(&a.as_str()), "{ids:?}");
        assert!(ids.contains(&b.as_str()), "{ids:?}");
    }

    // -----------------------------------------------------------------
    // GM-356: `Incoming` anchored on a file of a module-graph language.
    //
    // Every fixture below is the shape a real plugin emits, read off the
    // three indexed repositories the diagnosis used: declarations carrying
    // `container`, the `File -IMPORTS-> container` edge, and - for Python
    // and Rust - the `Module` declarations that make "which container does
    // this file define" a question with a wrong answer available.
    // -----------------------------------------------------------------

    /// A `File` node with a real extent. The default [`file`] helper leaves
    /// every position at 0, which would make *every* member look like it
    /// spans the whole file - exactly the distinction
    /// `containers::defining_containers` turns on - so a fixture about that
    /// distinction has to state the extent it means.
    fn source_file(path: &str, language: &str, end_line: i64) -> NodeRecord {
        let mut node = NodeRecord::new(path, "File", path, path, path, language);
        node.end_line = end_line;
        node
    }

    /// An ordinary declaration inside `file_path`, belonging to container
    /// `key` - a Go func, a Rust item, a Python class. Spans a few lines
    /// somewhere inside the file, never all of it.
    fn member_in(id: &str, language: &str, key: &str, parent: Option<&str>, file_path: &str) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, file_path, language);
        node.container = Some(key.to_string());
        node.container_parent = parent.map(str::to_string);
        node.start_line = 5;
        node.end_line = 7;
        node
    }

    /// The `Module` declaration a Python file gets for *itself*: it spans the
    /// whole file, and its container is the package the file sits in - the
    /// parent, not the module the file defines. Counting it would offer
    /// `requests` as a candidate for every file in `requests/`.
    fn whole_file_module(
        id: &str,
        language: &str,
        package: &str,
        file_path: &str,
        end_line: i64,
    ) -> NodeRecord {
        let mut node = NodeRecord::new(id, MODULE_KIND, id, id, file_path, language);
        node.native_kind = Some("module".to_string());
        node.container = Some(package.to_string());
        node.end_line = end_line;
        node
    }

    /// The `Module` declaration a Rust `mod sinks { .. }` gets: nested inside
    /// the file, so it evidences the container it is declared in rather than
    /// standing for the file.
    fn nested_module(
        id: &str,
        language: &str,
        declared_in: &str,
        file_path: &str,
        (start_line, end_line): (i64, i64),
    ) -> NodeRecord {
        let mut node = NodeRecord::new(id, MODULE_KIND, id, id, file_path, language);
        node.native_kind = Some("module".to_string());
        node.container = Some(declared_in.to_string());
        node.start_line = start_line;
        node.end_line = end_line;
        node
    }

    fn incoming(max_depth: u32) -> WalkShape {
        WalkShape { direction: Direction::Incoming, max_depth: Some(max_depth), max_fanout: Some(50) }
    }

    /// The defect itself, on the Python shape that was measured:
    /// `get_dependencies("src/requests/adapters.py", Incoming)` returned
    /// `results: []` with `truncated: false` while `sessions.py` held a
    /// `from .adapters import HTTPAdapter`. The importers arrive at the
    /// container, so the walk has to start there - and say that it did.
    #[test]
    fn incoming_from_a_python_file_walks_the_module_that_file_defines() {
        let mut conn = setup();
        upsert_node(&mut conn, source_file("src/requests/adapters.py", "python", 748)).unwrap();
        upsert_node(&mut conn, source_file("src/requests/sessions.py", "python", 800)).unwrap();
        upsert_node(
            &mut conn,
            member_in(
                "HTTPAdapter",
                "python",
                "requests.adapters",
                Some("requests"),
                "src/requests/adapters.py",
            ),
        )
        .unwrap();
        // The file's own module node, a member of the *package*.
        upsert_node(
            &mut conn,
            whole_file_module("adapters", "python", "requests", "src/requests/adapters.py", 748),
        )
        .unwrap();
        upsert_node(&mut conn, member_in("Session", "python", "requests", None, "src/requests/sessions.py"))
            .unwrap();
        let adapters = crate::graph::containers::container_id("python", "requests.adapters");
        imports_container(&mut conn, "src/requests/sessions.py", &adapters);

        let body = json_body(&from_file(&conn, "src/requests/adapters.py", &incoming(1)).unwrap());

        let importers: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        assert_eq!(
            importers,
            vec!["src/requests/sessions.py"],
            "the importer of requests.adapters is the answer to 'what imports adapters.py': {body}"
        );
        assert_eq!(body["resolvedFrom"]["requested"], "src/requests/adapters.py");
        assert_eq!(
            body["resolvedFrom"]["qualifiedName"], "requests.adapters",
            "the substitution names the anchor that would have worked: {body}"
        );
        assert!(
            body["resolvedFrom"]["filePath"].is_null(),
            "a container substitution landed on a container, not a file: {body}"
        );
    }

    /// Go's shape: one package per directory, no nesting, and the file that
    /// GMB-163 measured returning zero - `render/render.go` against three
    /// real importers.
    #[test]
    fn incoming_from_a_go_file_walks_the_package_that_file_defines() {
        let mut conn = setup();
        const PKG: &str = "github.com/gin-gonic/gin/render";
        upsert_node(&mut conn, source_file("render/render.go", "go", 60)).unwrap();
        upsert_node(&mut conn, source_file("context.go", "go", 900)).unwrap();
        upsert_node(&mut conn, member_in("Render", "go", PKG, None, "render/render.go")).unwrap();
        upsert_node(&mut conn, member_in("Context", "go", "github.com/gin-gonic/gin", None, "context.go"))
            .unwrap();
        let render = crate::graph::containers::container_id("go", PKG);
        imports_container(&mut conn, "context.go", &render);

        let body = json_body(&from_file(&conn, "render/render.go", &incoming(1)).unwrap());

        let importers: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        assert_eq!(importers, vec!["context.go"], "{body}");
        assert_eq!(body["resolvedFrom"]["qualifiedName"], PKG, "{body}");
    }

    /// Rust's shape, and the half of the rule Go and Python never exercise: a
    /// `mod sinks { .. }` written inside `sink.rs` puts members of
    /// `grep_searcher::sink::sinks` in that file too. The file still defines
    /// `grep_searcher::sink`; the nested module is something it *contains*.
    /// Anchoring on the descendant would answer about a module nobody
    /// imports.
    #[test]
    fn a_module_nested_inside_a_rust_file_is_not_the_module_that_file_defines() {
        let mut conn = setup();
        const SINK: &str = "grep_searcher::sink";
        const SINKS: &str = "grep_searcher::sink::sinks";
        upsert_node(&mut conn, source_file("crates/searcher/src/sink.rs", "rust", 663)).unwrap();
        upsert_node(&mut conn, source_file("crates/printer/src/standard.rs", "rust", 400)).unwrap();
        upsert_node(
            &mut conn,
            member_in("Sink", "rust", SINK, Some("grep_searcher"), "crates/searcher/src/sink.rs"),
        )
        .unwrap();
        upsert_node(
            &mut conn,
            nested_module("sinks", "rust", SINK, "crates/searcher/src/sink.rs", (516, 662)),
        )
        .unwrap();
        upsert_node(&mut conn, member_in("UTF8", "rust", SINKS, Some(SINK), "crates/searcher/src/sink.rs"))
            .unwrap();
        upsert_node(
            &mut conn,
            member_in("Standard", "rust", "grep_printer", None, "crates/printer/src/standard.rs"),
        )
        .unwrap();
        let sink = crate::graph::containers::container_id("rust", SINK);
        imports_container(&mut conn, "crates/printer/src/standard.rs", &sink);

        let body = json_body(&from_file(&conn, "crates/searcher/src/sink.rs", &incoming(1)).unwrap());

        let importers: Vec<&str> =
            body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
        assert_eq!(importers, vec!["crates/printer/src/standard.rs"], "{body}");
        assert_eq!(
            body["resolvedFrom"]["qualifiedName"], SINK,
            "the outermost container the file declares, not the one declared inside it: {body}"
        );
    }

    /// The TypeScript control, and the reason the guard is "this file has no
    /// importers of its own" rather than a list of languages: a TS import
    /// arrives at a file, so the literal anchor is already the right one and
    /// nothing about this call may change.
    #[test]
    fn a_typescript_file_keeps_its_literal_incoming_anchor() {
        let mut conn = setup();
        upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
        upsert_node(&mut conn, file("app/viewport.ts")).unwrap();
        imports(&mut conn, "app/viewport.ts", "packages/math/src/index.ts");

        let result = from_file(&conn, "packages/math/src/index.ts", &incoming(1)).unwrap();
        let body = json_body(&result);

        assert_eq!(body["results"][0]["filePath"], "app/viewport.ts");
        assert_eq!(body["results"][0]["kind"], "File", "a file, not a container: {body}");
        assert!(!body.to_string().contains("resolvedFrom"), "no substitution: {body}");
    }

    /// The other TypeScript control: an empty answer stays an empty answer.
    /// A file nothing imports, in a language with no containers at all, has
    /// nothing to substitute - and the zero it returns is the true one.
    #[test]
    fn a_typescript_file_nothing_imports_still_answers_a_plain_empty_walk() {
        let mut conn = setup();
        upsert_node(&mut conn, file("app/main.ts")).unwrap();
        upsert_node(&mut conn, file("app/util.ts")).unwrap();
        imports(&mut conn, "app/main.ts", "app/util.ts");

        let body = json_body(&from_file(&conn, "app/main.ts", &incoming(1)).unwrap());

        assert!(body["results"].as_array().unwrap().is_empty(), "{body}");
        assert_eq!(body["truncated"], false);
        assert!(!body.to_string().contains("resolvedFrom"), "nothing was substituted: {body}");
    }

    /// `Outgoing` is deliberately outside the substitution: those edges leave
    /// the file node, so the literal anchor answers the question actually
    /// asked - "what does *this file* import" - and running the walk from the
    /// container would silently widen it to every file in the module.
    #[test]
    fn outgoing_from_a_module_graph_file_is_left_alone() {
        let mut conn = setup();
        upsert_node(&mut conn, source_file("render/render.go", "go", 60)).unwrap();
        upsert_node(
            &mut conn,
            member_in("Render", "go", "github.com/gin-gonic/gin/render", None, "render/render.go"),
        )
        .unwrap();
        let http = materialize_container(&mut conn, "go", "net/http");
        imports_container(&mut conn, "render/render.go", &http);

        let body = json_body(
            &from_file(
                &conn,
                "render/render.go",
                &WalkShape { direction: Direction::Outgoing, max_depth: Some(1), max_fanout: Some(50) },
            )
            .unwrap(),
        );

        assert_eq!(body["results"][0]["qualifiedName"], "net/http", "{body}");
        assert!(!body.to_string().contains("resolvedFrom"), "no substitution on Outgoing: {body}");
    }

    /// A file that declares nothing the index carries a container for - a
    /// `doc.go`, a `setup.py`, a Rust integration-test binary. There is no
    /// module to name, so the literal answer stands rather than being dressed
    /// up as something better. The residual half of the defect, recorded
    /// rather than papered over.
    #[test]
    fn a_file_that_defines_no_container_keeps_its_literal_answer() {
        let mut conn = setup();
        upsert_node(&mut conn, source_file("src/requests/certs.py", "python", 18)).unwrap();
        // Only the file's own module node, which is a member of the package.
        upsert_node(&mut conn, whole_file_module("certs", "python", "requests", "src/requests/certs.py", 18))
            .unwrap();

        let body = json_body(&from_file(&conn, "src/requests/certs.py", &incoming(1)).unwrap());

        assert!(body["results"].as_array().unwrap().is_empty(), "{body}");
        assert!(
            !body.to_string().contains("resolvedFrom"),
            "the package is not what this file defines, so it is not substituted: {body}"
        );
    }

    /// Two sibling modules declared in one file, neither inside the other:
    /// no single anchor means "this file", so both are named and the caller
    /// picks. Guessing here would answer about one of them while looking
    /// exactly like answering about the file.
    #[test]
    fn a_file_defining_two_sibling_modules_is_refused_with_both_named() {
        let mut conn = setup();
        upsert_node(&mut conn, source_file("crates/x/src/pair.rs", "rust", 200)).unwrap();
        upsert_node(
            &mut conn,
            member_in("a_item", "rust", "x::pair::a", Some("x::pair"), "crates/x/src/pair.rs"),
        )
        .unwrap();
        upsert_node(
            &mut conn,
            member_in("b_item", "rust", "x::pair::b", Some("x::pair"), "crates/x/src/pair.rs"),
        )
        .unwrap();

        let message = error_text(&from_file(&conn, "crates/x/src/pair.rs", &incoming(1)).unwrap());

        assert!(message.contains("x::pair::a"), "{message}");
        assert!(message.contains("x::pair::b"), "{message}");
    }
}
