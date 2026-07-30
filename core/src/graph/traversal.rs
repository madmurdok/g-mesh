use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{Connection, ToSql};

use crate::graph::pagination::Direction;
use crate::graph::queries::map_node_row;
use crate::graph::resume_token::{self, ResumeState, VisitedNode};
use crate::storage::write::{EdgeRecord, NodeRecord};

/// Response-level limits: how deep the walk goes and how many children a
/// single node expands per level. Unvalidated defaults per the requirements
/// doc - confirm on a prototype benchmark.
pub const DEFAULT_MAX_DEPTH: u32 = 5;
pub const DEFAULT_MAX_FANOUT: u32 = 50;
/// Internal exploration budget: a hard cap on rows the recursive CTE
/// generates, independent of the response-level limits above. Bounds what
/// the query engine visits on a hub node, not what the caller gets back.
pub const DEFAULT_EXPLORATION_BUDGET: u32 = 5000;

/// Why a traversal stopped short of the full transitive closure. Continuation
/// differs per cause: `MaxFanout` needs no new mechanism (the caller
/// paginates the cut node's edges with `pagination::paginate_edges`),
/// `MaxDepth` hands back `frontier_nodes` to re-root from, `ExplorationBudget`
/// hands back a `resume_token` because the budget is spent across the whole
/// walk rather than at one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncatedBy {
    MaxDepth,
    MaxFanout,
    ExplorationBudget,
}

pub struct TraversalOptions {
    pub start_node_id: String,
    pub direction: Direction,
    pub edge_kind: Option<String>,
    pub max_depth: u32,
    pub max_fanout: u32,
    pub exploration_budget: u32,
}

impl TraversalOptions {
    /// Options at the documented defaults; override the fields directly.
    pub fn new(start_node_id: impl Into<String>, direction: Direction) -> Self {
        Self {
            start_node_id: start_node_id.into(),
            direction,
            edge_kind: None,
            max_depth: DEFAULT_MAX_DEPTH,
            max_fanout: DEFAULT_MAX_FANOUT,
            exploration_budget: DEFAULT_EXPLORATION_BUDGET,
        }
    }
}

pub struct ReachedNode {
    pub node: NodeRecord,
    /// Hops from the start node; the start node itself is depth 0.
    pub depth: u32,
}

pub struct TraversalResult {
    /// Reached nodes, deduplicated, each at the shallowest depth it was
    /// reached at, ordered by depth then id.
    pub nodes: Vec<ReachedNode>,
    /// Edges actually walked, deduplicated, in the same order.
    pub edges: Vec<EdgeRecord>,
    /// Rows the recursive CTE generated internally, i.e. what the exploration
    /// budget is spent on. Larger than `nodes.len()` when a node is reachable
    /// by several paths.
    pub visited_rows: u32,
    pub truncated: bool,
    pub truncated_by: Option<TruncatedBy>,
    /// Ids of the nodes on the depth boundary (`depth == max_depth`) - the
    /// last level the walk expanded, which the caller re-roots on to go one
    /// hop further. Populated only for `TruncatedBy::MaxDepth`.
    pub frontier_nodes: Vec<String>,
    /// Opaque continuation state for `resume`. Populated only for
    /// `TruncatedBy::ExplorationBudget`.
    pub resume_token: Option<String>,
}

fn endpoints(direction: &Direction) -> (&'static str, &'static str) {
    match direction {
        Direction::Outgoing => ("fromId", "toId"),
        Direction::Incoming => ("toId", "fromId"),
    }
}

/// One expansion step, shared verbatim by the plain walk and the resumed one.
/// `extra_filter` appends resume-only predicates; it is empty for `walk_cte`,
/// which must keep producing exactly the query plan below.
///
/// `maxFanout` (?3): the child-selection subquery is the *driver* of the join
/// (`e.id IN (...)`, no usable index constraint on the edge endpoint), so
/// SQLite evaluates it once per expanded node and then seeks each of at most
/// `maxFanout` children by primary key. Capping expansion this way, rather
/// than filtering an already-scanned adjacency list, is what keeps a hub
/// node's step bounded inside the engine. `maxDepth` (?4) is a plain depth cap.
///
/// Cycle safety: each row carries the delimited id path it was reached by and
/// refuses to re-enter a node already on that path. SQLite forbids a second
/// reference to the recursive table, so a global visited set isn't
/// expressible; per-path is the correct-and-expressible option, at the cost
/// of re-visiting diamond-shaped nodes once per path (bounded by the budget,
/// deduplicated in `TraversalResult`).
fn recursive_term(direction: &Direction, extra_filter: &str) -> String {
    let (this_endpoint, other_endpoint) = endpoints(direction);

    format!(
        "SELECT e.{other_endpoint}, e.id, w.depth + 1, w.path || e.{other_endpoint} || '|' \
         FROM walk w \
         JOIN edges e ON e.id IN ( \
             SELECT c.id FROM edges c \
             WHERE c.{this_endpoint} = w.node_id \
               AND (?2 IS NULL OR c.kind = ?2) \
             ORDER BY c.resolved DESC, c.id ASC \
             LIMIT ?3 \
         ) \
         WHERE w.depth < ?4 \
           AND instr(w.path, '|' || e.{other_endpoint} || '|') = 0 \
           {extra_filter}"
    )
}

/// The bounded walk from a single root (?1). The exploration budget (?5) is a
/// `LIMIT` on the CTE itself - SQLite stops the recursion once initial +
/// recursive rows reach it, regardless of how much frontier is left.
fn walk_cte(direction: &Direction) -> String {
    let term = recursive_term(direction, "");
    format!(
        "WITH RECURSIVE walk(node_id, edge_id, depth, path) AS ( \
             SELECT ?1, NULL, 0, '|' || ?1 || '|' \
             UNION ALL {term} \
             LIMIT ?5 \
         )"
    )
}

/// The same walk seeded from a token's visited set (?1, JSON) instead of one
/// root, with the edges the chain has already reported in ?6. Seed rows are
/// the only ones with a NULL `edge_id`, which is what the two extra predicates
/// key off:
///
/// - a row landing back on an already-visited node is never expanded again
///   (only the seeds themselves are), so the fresh budget buys new ground;
/// - a seed does not re-emit a hop the chain already reported - but it is the
///   *edge* that decides that, not the child node. A seed is re-expanded from
///   scratch, so it re-offers every one of its edges, and the earlier call may
///   well have reported only some of them: it can have been cut mid-expansion
///   by the budget, or have reached the child by a different edge entirely
///   (a diamond, or two parallel edges between the same pair). Gating on the
///   child node instead would drop those still-unreported hops for good, since
///   no later call ever offers them again. A node first discovered by *this*
///   call needs no gate at all - nothing has walked its edges yet, and none of
///   them can be in ?6.
///
/// Gating on the edge alone is not just narrower, it is exactly equivalent on
/// the case the old node gate got right: every reported edge's endpoint is a
/// reported node, so `e.id IN walked` already implies the child is visited.
fn resume_cte(direction: &Direction) -> String {
    let term = recursive_term(
        direction,
        "AND (w.edge_id IS NULL OR w.node_id NOT IN (SELECT id FROM visited)) \
         AND (w.edge_id IS NOT NULL OR e.id NOT IN (SELECT id FROM walked))",
    );

    format!(
        "WITH RECURSIVE \
         visited(id) AS (SELECT json_extract(value, '$.id') FROM json_each(?1)), \
         walked(id) AS (SELECT value FROM json_each(?6)), \
         walk(node_id, edge_id, depth, path) AS ( \
             SELECT json_extract(value, '$.id'), NULL, json_extract(value, '$.depth'), \
                    '|' || json_extract(value, '$.id') || '|' \
             FROM json_each(?1) \
             UNION ALL {term} \
             LIMIT ?5 \
         )"
    )
}

/// Runs a bounded transitive walk from `start_node_id` over `edges`, in one
/// recursive CTE. Reports which limit (if any) cut the result short, so a
/// caller never reads an exhausted budget as "there is nothing more".
pub fn traverse(conn: &Connection, options: TraversalOptions) -> Result<TraversalResult> {
    let cte = walk_cte(&options.direction);
    let params: [&dyn ToSql; 5] = [
        &options.start_node_id,
        &options.edge_kind,
        &options.max_fanout,
        &options.max_depth,
        &options.exploration_budget,
    ];
    let rows = run_walk(conn, &cte, &params)?;

    let mut nodes: Vec<ReachedNode> = Vec::new();
    let mut edges: Vec<EdgeRecord> = Vec::new();
    let mut seen_nodes: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<String> = HashSet::new();
    let visited_rows = rows.len() as u32;

    for (node, depth, edge) in rows {
        if seen_nodes.insert(node.id.clone()) {
            nodes.push(ReachedNode { node, depth });
        }
        if let Some(edge) = edge {
            if seen_edges.insert(edge.id.clone()) {
                edges.push(edge);
            }
        }
    }

    // Budget exhaustion outranks the response-level limits: it is the only
    // cause that needs a resume token rather than a cheap re-query, and it
    // makes the depth/fanout analysis below meaningless anyway (the walk it
    // would analyse is itself partial). Reported when the CTE produced
    // exactly the budget - deliberately conservative, a graph whose closure
    // is exactly `exploration_budget` rows reports truncated.
    let truncated_by = if visited_rows >= options.exploration_budget {
        Some(TruncatedBy::ExplorationBudget)
    } else {
        let (fanout_cut, depth_cut) = analyze_cuts(conn, &cte, &options.direction, &params)?;
        match (fanout_cut, depth_cut) {
            (true, _) => Some(TruncatedBy::MaxFanout),
            (false, true) => Some(TruncatedBy::MaxDepth),
            (false, false) => None,
        }
    };

    let reached: Vec<VisitedNode> = nodes
        .iter()
        .map(|r| VisitedNode { id: r.node.id.clone(), depth: r.depth })
        .collect();
    // Every row this call produced was reported, so the edges it walked are
    // exactly the hops a resumed call must not offer a second time.
    let walked: Vec<String> = edges.iter().map(|e| e.id.clone()).collect();

    Ok(TraversalResult {
        frontier_nodes: frontier_nodes(truncated_by, &reached, options.max_depth),
        resume_token: continuation(
            truncated_by,
            ResumeState {
                direction: options.direction,
                edge_kind: options.edge_kind.clone(),
                max_depth: options.max_depth,
                max_fanout: options.max_fanout,
                visited: reached,
                walked,
            },
        ),
        nodes,
        edges,
        visited_rows,
        truncated: truncated_by.is_some(),
        truncated_by,
    })
}

/// Continues a walk that the exploration budget cut short, with a budget of
/// its own. The token carries the walk's shape as well as its state, so a
/// resumed call cannot silently drift into a different traversal; only the
/// budget is the caller's to choose again.
///
/// The returned result holds *only* what this call added: nodes the earlier
/// calls never returned, and the edges walked to reach them. Combining the
/// results of a call chain is a plain union - nothing is reported twice.
pub fn resume(conn: &Connection, token: &str, exploration_budget: u32) -> Result<TraversalResult> {
    let state = resume_token::decode(token)?;
    let seeds =
        serde_json::to_string(&state.visited).expect("resume state is always serializable");
    let walked = serde_json::to_string(&state.walked).expect("resume state is always serializable");
    let already_returned: HashSet<&str> = state.visited.iter().map(|v| v.id.as_str()).collect();

    // Replaying the seeds costs rows the budget must not be billed for: they
    // carry no new information, they only put the queue back where it was.
    let limit = state.visited.len() as i64 + exploration_budget as i64;
    let cte = resume_cte(&state.direction);
    let params: [&dyn ToSql; 6] =
        [&seeds, &state.edge_kind, &state.max_fanout, &state.max_depth, &limit, &walked];
    let rows = run_walk(conn, &cte, &params)?;

    let mut nodes: Vec<ReachedNode> = Vec::new();
    let mut edges: Vec<EdgeRecord> = Vec::new();
    let mut seen_nodes: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<String> = HashSet::new();
    let mut visited_rows = 0;

    for (node, depth, edge) in rows {
        // Seed rows are the replay, not exploration; a row with an edge but an
        // already-returned node is an edge the earlier call never walked.
        let Some(edge) = edge else { continue };
        visited_rows += 1;
        if !already_returned.contains(node.id.as_str()) && seen_nodes.insert(node.id.clone()) {
            nodes.push(ReachedNode { node, depth });
        }
        if seen_edges.insert(edge.id.clone()) {
            edges.push(edge);
        }
    }

    let truncated_by = if visited_rows >= exploration_budget {
        Some(TruncatedBy::ExplorationBudget)
    } else {
        let (fanout_cut, depth_cut) = analyze_cuts(conn, &cte, &state.direction, &params)?;
        match (fanout_cut, depth_cut) {
            (true, _) => Some(TruncatedBy::MaxFanout),
            (false, true) => Some(TruncatedBy::MaxDepth),
            (false, false) => None,
        }
    };

    // The chain's state is cumulative: the next token has to seed from every
    // node the whole chain has returned, not just this call's share. Ordered
    // by (depth, id) so the resumed queue stays breadth-first.
    let mut reached = state.visited;
    reached.extend(nodes.iter().map(|r| VisitedNode { id: r.node.id.clone(), depth: r.depth }));
    reached.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.id.cmp(&b.id)));

    // The edge side of the same accumulation, and for the same reason: the
    // seed gate only holds if it knows every hop the *chain* reported, not
    // just this call's share. Sorted so the token is a function of the walk
    // rather than of row order.
    let mut walked_edges = state.walked;
    walked_edges.extend(edges.iter().map(|e| e.id.clone()));
    walked_edges.sort_unstable();

    Ok(TraversalResult {
        frontier_nodes: frontier_nodes(truncated_by, &reached, state.max_depth),
        resume_token: continuation(
            truncated_by,
            ResumeState {
                direction: state.direction,
                edge_kind: state.edge_kind,
                max_depth: state.max_depth,
                max_fanout: state.max_fanout,
                visited: reached,
                walked: walked_edges,
            },
        ),
        nodes,
        edges,
        visited_rows,
        truncated: truncated_by.is_some(),
        truncated_by,
    })
}

fn run_walk(
    conn: &Connection,
    cte: &str,
    params: &[&dyn ToSql],
) -> Result<Vec<(NodeRecord, u32, Option<EdgeRecord>)>> {
    let sql = format!(
        "{cte} \
         SELECT n.*, w.depth AS depth, \
                e.id AS walkedEdgeId, e.fromId AS walkedFromId, e.toId AS walkedToId, \
                e.kind AS walkedKind, e.source AS walkedSource, e.resolved AS walkedResolved \
         FROM walk w \
         JOIN nodes n ON n.id = w.node_id \
         LEFT JOIN edges e ON e.id = w.edge_id \
         ORDER BY w.depth ASC, w.node_id ASC"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params, |row| {
            let node = map_node_row(row)?;
            let depth: u32 = row.get("depth")?;
            let edge = match row.get::<_, Option<String>>("walkedEdgeId")? {
                Some(id) => Some(EdgeRecord {
                    id,
                    from_id: row.get("walkedFromId")?,
                    to_id: row.get("walkedToId")?,
                    kind: row.get("walkedKind")?,
                    source: row.get("walkedSource")?,
                    resolved: row.get("walkedResolved")?,
                }),
                None => None,
            };
            Ok((node, depth, edge))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to run bounded traversal")?;

    Ok(rows)
}

/// The deepest level the walk expanded - what a `maxDepth` caller re-roots on.
/// Empty for every other cause: a frontier is only meaningful when depth, not
/// something else, is what stopped the walk.
fn frontier_nodes(
    truncated_by: Option<TruncatedBy>,
    reached: &[VisitedNode],
    max_depth: u32,
) -> Vec<String> {
    match truncated_by {
        Some(TruncatedBy::MaxDepth) => reached
            .iter()
            .filter(|v| v.depth == max_depth)
            .map(|v| v.id.clone())
            .collect(),
        _ => Vec::new(),
    }
}

/// A resume token only for the one cause that cannot be re-queried around:
/// `maxFanout` is paginated per node, `maxDepth` is re-rooted per frontier
/// node, but the budget is spent across the whole walk.
fn continuation(truncated_by: Option<TruncatedBy>, state: ResumeState) -> Option<String> {
    match truncated_by {
        Some(TruncatedBy::ExplorationBudget) => Some(resume_token::encode(&state)),
        _ => None,
    }
}

/// Re-runs the walk to ask whether either response-level limit actually
/// dropped something: `fanout_cut` - some expanded node had more matching
/// edges than `maxFanout`; `depth_cut` - some node at the depth boundary
/// leads to a node the walk never reached. `params` are the walk's own five,
/// so this works against either CTE shape.
fn analyze_cuts(
    conn: &Connection,
    cte: &str,
    direction: &Direction,
    params: &[&dyn ToSql],
) -> Result<(bool, bool)> {
    let (this_endpoint, other_endpoint) = endpoints(direction);

    let sql = format!(
        "{cte} \
         SELECT \
             EXISTS ( \
                 SELECT 1 FROM (SELECT DISTINCT node_id FROM walk WHERE depth < ?4) p \
                 JOIN edges e ON e.{this_endpoint} = p.node_id AND (?2 IS NULL OR e.kind = ?2) \
                 GROUP BY p.node_id HAVING COUNT(*) > ?3 \
             ) AS fanoutCut, \
             EXISTS ( \
                 SELECT 1 FROM (SELECT DISTINCT node_id FROM walk WHERE depth = ?4) f \
                 JOIN edges e ON e.{this_endpoint} = f.node_id AND (?2 IS NULL OR e.kind = ?2) \
                 WHERE e.{other_endpoint} NOT IN (SELECT node_id FROM walk) \
             ) AS depthCut"
    );

    conn.query_row(&sql, params, |row| Ok((row.get("fanoutCut")?, row.get("depthCut")?)))
        .context("failed to analyse traversal truncation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
    use rusqlite::params;
    use std::time::Instant;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn make_node(conn: &Connection, id: &str) {
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', ?1, ?1, 'src/lib.rs', 0, 0, 0, 0, 'rust')",
            params![id],
        )
        .unwrap();
    }

    fn make_edge(conn: &Connection, id: &str, from: &str, to: &str, kind: &str) {
        conn.execute(
            "INSERT INTO edges (id, fromId, toId, kind, source, resolved) VALUES (?1, ?2, ?3, ?4, 'tree-sitter', 1)",
            params![id, from, to, kind],
        )
        .unwrap();
    }

    /// `root` -> `parents` children, each with `children` distinct children of
    /// its own - generated in SQL so the wide/hub fixtures stay fast.
    fn make_wide_graph(conn: &Connection, parents: usize, children: usize) {
        let total = parents + parents * children;
        let mut sql = format!(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
                 VALUES ('root', 'Function', 'root', 'root', 'src/lib.rs', 0, 0, 0, 0, 'rust');
             WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < {total})
                 INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
                 SELECT 'n' || printf('%08d', i), 'Function', 'n', 'n', 'src/lib.rs', 0, 0, 0, 0, 'rust' FROM seq;
             WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < {parents})
                 INSERT INTO edges (id, fromId, toId, kind, source, resolved)
                 SELECT 'e' || printf('%08d', i), 'root', 'n' || printf('%08d', i), 'CALLS', 'tree-sitter', 1 FROM seq;"
        );
        if children > 0 {
            sql.push_str(&format!(
                "WITH RECURSIVE seq(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM seq WHERE i < {parents} * {children})
                     INSERT INTO edges (id, fromId, toId, kind, source, resolved)
                     SELECT 'g' || printf('%08d', i),
                            'n' || printf('%08d', (i - 1) / {children} + 1),
                            'n' || printf('%08d', {parents} + i),
                            'CALLS', 'tree-sitter', 1
                     FROM seq;"
            ));
        }
        conn.execute_batch(&sql).unwrap();
    }

    fn node_ids(result: &TraversalResult) -> Vec<&str> {
        result.nodes.iter().map(|r| r.node.id.as_str()).collect()
    }

    /// splitmix64 - good dispersion for a benchmark id, no need for a
    /// cryptographic hash. Only the *length and entropy class* need to match
    /// a real sha256-derived id (`plugins/js-ts/src/extract.ts::hash`, 32 hex
    /// chars, effectively uniform): `make_wide_graph`'s zero-padded counter
    /// ids compress far better than a real id would and would understate
    /// resume-token size.
    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn realistic_id(n: u64) -> String {
        let a = splitmix64(n);
        let b = splitmix64(n ^ 0xDEAD_BEEF_CAFE_BABE);
        format!("{a:016x}{b:016x}")
    }

    /// Same shape as `make_wide_graph`, but with 32-hex-char, high-entropy
    /// ids (see `realistic_id`) instead of short zero-padded counters, so a
    /// resume-token-size measurement isn't skewed by unrealistically
    /// compressible ids. Ids are pre-generated in Rust and inserted through a
    /// prepared statement reused across the loop, not re-prepared per row,
    /// so tens of thousands of rows stay fast without needing SQL-side hash
    /// arithmetic (which, for small sequential inputs, leaves the high bits
    /// zero and is no more realistic than a padded counter).
    fn make_wide_graph_with_realistic_ids(conn: &Connection, parents: usize, children: usize) -> String {
        let root_id = realistic_id(0);
        conn.execute(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
             VALUES (?1, 'Function', 'root', 'root', 'src/lib.rs', 0, 0, 0, 0, 'rust')",
            params![root_id],
        )
        .unwrap();

        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut insert_node = tx
                .prepare(
                    "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language)
                     VALUES (?1, 'Function', 'n', 'n', 'src/lib.rs', 0, 0, 0, 0, 'rust')",
                )
                .unwrap();
            let mut insert_edge = tx
                .prepare("INSERT INTO edges (id, fromId, toId, kind, source, resolved) VALUES (?1, ?2, ?3, 'CALLS', 'tree-sitter', 1)")
                .unwrap();

            for p in 0..parents {
                // Offset ids so parent/child/edge namespaces never collide.
                let pid = realistic_id(1_000_000 + p as u64);
                insert_node.execute(params![pid]).unwrap();
                let eid = realistic_id(2_000_000 + p as u64);
                insert_edge.execute(params![eid, root_id, pid]).unwrap();

                for c in 0..children {
                    let idx = (p * children + c) as u64;
                    let cid = realistic_id(3_000_000 + idx);
                    insert_node.execute(params![cid]).unwrap();
                    let geid = realistic_id(4_000_000 + idx);
                    insert_edge.execute(params![geid, pid, cid]).unwrap();
                }
            }
        }
        tx.commit().unwrap();

        root_id
    }

    #[test]
    fn small_graph_under_every_limit_returns_the_full_transitive_closure() {
        let conn = setup();
        for id in ["a", "b", "c", "d", "e"] {
            make_node(&conn, id);
        }
        // Diamond a -> {b, d} -> c -> e: c is reachable by two paths.
        make_edge(&conn, "e_ab", "a", "b", "CALLS");
        make_edge(&conn, "e_ad", "a", "d", "CALLS");
        make_edge(&conn, "e_bc", "b", "c", "CALLS");
        make_edge(&conn, "e_dc", "d", "c", "CALLS");
        make_edge(&conn, "e_ce", "c", "e", "CALLS");

        let result = traverse(&conn, TraversalOptions::new("a", Direction::Outgoing)).unwrap();

        assert_eq!(node_ids(&result), vec!["a", "b", "d", "c", "e"]);
        assert_eq!(result.nodes.iter().map(|r| r.depth).collect::<Vec<_>>(), vec![0, 1, 1, 2, 3]);

        let mut edge_ids: Vec<&str> = result.edges.iter().map(|e| e.id.as_str()).collect();
        edge_ids.sort_unstable();
        assert_eq!(edge_ids, vec!["e_ab", "e_ad", "e_bc", "e_ce", "e_dc"]);

        assert!(!result.truncated, "nothing was dropped, so nothing may be reported as truncated");
        assert_eq!(result.truncated_by, None);
        // 7 rows for 5 nodes: c and e are each walked once per path through
        // the diamond, then deduplicated in the result.
        assert_eq!(result.visited_rows, 7);
    }

    #[test]
    fn incoming_direction_and_edge_kind_filter_select_what_is_walked() {
        let conn = setup();
        for id in ["a", "b", "c", "x"] {
            make_node(&conn, id);
        }
        // Transitive callers of c: b calls c, a calls b. x only references b.
        make_edge(&conn, "e_ab", "a", "b", "CALLS");
        make_edge(&conn, "e_bc", "b", "c", "CALLS");
        make_edge(&conn, "e_xb", "x", "b", "REFERENCES");

        let mut options = TraversalOptions::new("c", Direction::Incoming);
        options.edge_kind = Some("CALLS".to_string());
        let result = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&result), vec!["c", "b", "a"]);
        assert!(!result.truncated);

        let unfiltered = traverse(&conn, TraversalOptions::new("c", Direction::Incoming)).unwrap();
        assert_eq!(node_ids(&unfiltered), vec!["c", "b", "a", "x"], "REFERENCES is walked when unfiltered");
    }

    #[test]
    fn fanout_cut_keeps_resolved_edges_over_unresolved_ones() {
        let conn = setup();
        for id in ["a", "b", "c", "d"] {
            make_node(&conn, id);
        }
        make_edge(&conn, "e1", "a", "b", "CALLS");
        make_edge(&conn, "e2", "a", "c", "CALLS");
        conn.execute("UPDATE edges SET resolved = 0 WHERE id IN ('e1', 'e2')", []).unwrap();
        make_edge(&conn, "e3", "a", "d", "CALLS");

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.max_fanout = 1;
        let result = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&result), vec!["a", "d"], "the resolved edge outranks lower-id unresolved ones");
        assert_eq!(result.truncated_by, Some(TruncatedBy::MaxFanout));
    }

    #[test]
    fn cycles_terminate_without_repeating_nodes() {
        let conn = setup();
        for id in ["a", "b", "c"] {
            make_node(&conn, id);
        }
        make_edge(&conn, "e_ab", "a", "b", "CALLS");
        make_edge(&conn, "e_bc", "b", "c", "CALLS");
        make_edge(&conn, "e_ca", "c", "a", "CALLS");

        let result = traverse(&conn, TraversalOptions::new("a", Direction::Outgoing)).unwrap();

        assert_eq!(node_ids(&result), vec!["a", "b", "c"]);
        assert_eq!(result.visited_rows, 3, "the edge closing the cycle must not re-enter a");
        assert!(!result.truncated, "a fully explored cycle is complete, not truncated");
    }

    #[test]
    fn max_depth_bounds_the_walk_with_fanout_unconstrained() {
        let conn = setup();
        let chain = ["a", "b", "c", "d", "e", "f", "g"];
        for id in chain {
            make_node(&conn, id);
        }
        for pair in chain.windows(2) {
            make_edge(&conn, &format!("e_{}{}", pair[0], pair[1]), pair[0], pair[1], "CALLS");
        }

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.max_depth = 2;
        options.max_fanout = 10_000;
        let bounded = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&bounded), vec!["a", "b", "c"]);
        assert_eq!(bounded.truncated_by, Some(TruncatedBy::MaxDepth));

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.max_depth = 10;
        options.max_fanout = 10_000;
        let full = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&full), chain.to_vec(), "a generous depth reaches the whole chain");
        assert!(!full.truncated);
    }

    #[test]
    fn max_fanout_bounds_children_per_node_per_level_with_depth_generous() {
        let conn = setup();
        make_wide_graph(&conn, 10, 10);

        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        options.max_fanout = 3;
        options.max_depth = 5;
        let result = traverse(&conn, options).unwrap();

        // Per node per level: 3 of root's 10 children, then 3 of each of
        // those children's 10 children - not a single global cap of 3.
        assert_eq!(result.nodes.len(), 1 + 3 + 9);
        assert_eq!(result.nodes.iter().filter(|r| r.depth == 1).count(), 3);
        assert_eq!(result.nodes.iter().filter(|r| r.depth == 2).count(), 9);
        assert_eq!(
            node_ids(&result)[1..4],
            ["n00000001", "n00000002", "n00000003"],
            "which children survive the cap is deterministic, not whatever the engine scanned first"
        );
        assert_eq!(result.visited_rows, 13, "capped expansion, not capped output");
        assert_eq!(result.truncated_by, Some(TruncatedBy::MaxFanout));

        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        options.max_fanout = 1000;
        options.max_depth = 5;
        let full = traverse(&conn, options).unwrap();
        assert_eq!(full.nodes.len(), 1 + 10 + 100, "a generous fanout reaches every node");
        assert!(!full.truncated);
    }

    #[test]
    fn fanout_cap_keeps_a_hub_node_bounded_inside_the_engine() {
        let conn = setup();
        make_wide_graph(&conn, 20_000, 0);

        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        options.max_fanout = 50;
        let started = Instant::now();
        let result = traverse(&conn, options).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(result.visited_rows, 51, "the CTE expands 50 of the hub's 20000 edges, not all of them");
        assert!(elapsed.as_millis() < 2000, "hub expansion must not scale with hub degree: {elapsed:?}");
        assert_eq!(result.truncated_by, Some(TruncatedBy::MaxFanout));
    }

    #[test]
    fn exploration_budget_caps_visited_rows_regardless_of_graph_size() {
        let conn = setup();
        make_wide_graph(&conn, 20_000, 0);
        let edge_count: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0)).unwrap();
        assert_eq!(edge_count, 20_000, "the hub really is far larger than the budget under test");

        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        // Response-level limits deliberately wide open: the only thing that
        // can stop this walk is the internal exploration budget.
        options.max_fanout = 1_000_000;
        options.max_depth = 5;
        options.exploration_budget = 100;

        let started = Instant::now();
        let result = traverse(&conn, options).unwrap();
        let elapsed = started.elapsed();

        assert_eq!(result.visited_rows, 100, "the CTE must stop at the budget, not at the graph's edge");
        assert_eq!(result.nodes.len(), 100);
        assert_eq!(result.truncated_by, Some(TruncatedBy::ExplorationBudget));
        assert!(elapsed.as_millis() < 2000, "a budgeted walk must not pay for the whole graph: {elapsed:?}");
    }

    #[test]
    fn budget_outranks_the_response_level_limits() {
        let conn = setup();
        make_wide_graph(&conn, 10, 10);

        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        options.max_fanout = 3;
        options.max_depth = 1;
        options.exploration_budget = 2;
        let result = traverse(&conn, options).unwrap();

        assert_eq!(result.visited_rows, 2);
        assert_eq!(
            result.truncated_by,
            Some(TruncatedBy::ExplorationBudget),
            "all three limits bind here; the budget is the one the caller cannot re-query around"
        );
    }

    /// The contract itself: which continuation field each cause hands back.
    /// A caller reads the presence of the field, not just the enum, so an
    /// extra one is as wrong as a missing one.
    #[test]
    fn each_truncation_cause_carries_only_its_own_continuation_field() {
        let conn = setup();
        let chain = ["a", "b", "c", "d", "e"];
        for id in chain {
            make_node(&conn, id);
        }
        for pair in chain.windows(2) {
            make_edge(&conn, &format!("e_{}{}", pair[0], pair[1]), pair[0], pair[1], "CALLS");
        }

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.max_depth = 2;
        options.max_fanout = 10_000;
        let by_depth = traverse(&conn, options).unwrap();

        assert!(by_depth.truncated);
        assert_eq!(by_depth.truncated_by, Some(TruncatedBy::MaxDepth));
        assert_eq!(by_depth.frontier_nodes, vec!["c"], "the last fully expanded level, to re-root on");
        assert_eq!(by_depth.resume_token, None, "a depth cut is re-rooted, not resumed");

        let fanout_conn = setup();
        make_wide_graph(&fanout_conn, 10, 10);
        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        options.max_fanout = 3;
        options.max_depth = 5;
        let by_fanout = traverse(&fanout_conn, options).unwrap();

        assert!(by_fanout.truncated);
        assert_eq!(by_fanout.truncated_by, Some(TruncatedBy::MaxFanout));
        assert!(by_fanout.frontier_nodes.is_empty(), "a fanout cut is paginated per node, not re-rooted");
        assert_eq!(by_fanout.resume_token, None);

        let mut options = TraversalOptions::new("root", Direction::Outgoing);
        options.max_fanout = 1_000_000;
        options.max_depth = 5;
        options.exploration_budget = 4;
        let by_budget = traverse(&fanout_conn, options).unwrap();

        assert!(by_budget.truncated);
        assert_eq!(by_budget.truncated_by, Some(TruncatedBy::ExplorationBudget));
        assert!(by_budget.frontier_nodes.is_empty(), "a budget cut has no meaningful depth boundary");
        assert!(by_budget.resume_token.is_some(), "only the budget needs opaque state to continue");

        let complete = traverse(&conn, TraversalOptions::new("a", Direction::Outgoing)).unwrap();
        assert!(!complete.truncated);
        assert_eq!(complete.truncated_by, None);
        assert!(complete.frontier_nodes.is_empty());
        assert_eq!(complete.resume_token, None);
    }

    #[test]
    fn a_resume_chain_covers_the_whole_walk_exactly_once() {
        let conn = setup();
        let chain = ["a", "b", "c", "d", "e", "f", "g"];
        for id in chain {
            make_node(&conn, id);
        }
        for pair in chain.windows(2) {
            make_edge(&conn, &format!("e_{}{}", pair[0], pair[1]), pair[0], pair[1], "CALLS");
        }

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.max_depth = 10;
        options.exploration_budget = 3;
        let first = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&first), vec!["a", "b", "c"]);
        assert_eq!(first.truncated_by, Some(TruncatedBy::ExplorationBudget));

        let mut all_nodes: Vec<String> = first.nodes.iter().map(|r| r.node.id.clone()).collect();
        let mut all_edges: Vec<String> = first.edges.iter().map(|e| e.id.clone()).collect();
        let mut token = first.resume_token.clone().expect("a budget cut hands back a token");
        let mut calls = 1;

        let last = loop {
            // A fresh budget per call is the caller's to choose; the walk's
            // shape rides along in the token and is not re-negotiable.
            let next = resume(&conn, &token, 3).unwrap();
            calls += 1;
            all_nodes.extend(next.nodes.iter().map(|r| r.node.id.clone()));
            all_edges.extend(next.edges.iter().map(|e| e.id.clone()));

            match next.resume_token.clone() {
                Some(t) => token = t,
                None => break next,
            }
            assert!(calls < 10, "the chain must converge, not re-explore itself forever");
        };

        assert!(calls > 2, "the budget really did split this walk across calls, {calls} of them");
        assert!(!last.truncated, "the final call ran out of graph, not of budget");
        assert_eq!(last.truncated_by, None);
        assert_eq!(last.resume_token, None);

        let mut deduped = all_nodes.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(deduped.len(), all_nodes.len(), "no node may be returned by two calls: {all_nodes:?}");
        assert_eq!(deduped, chain.to_vec(), "the chain's union is the full closure");

        let mut deduped_edges = all_edges.clone();
        deduped_edges.sort();
        deduped_edges.dedup();
        assert_eq!(deduped_edges.len(), all_edges.len(), "no edge may be returned twice: {all_edges:?}");
        assert_eq!(deduped_edges.len(), chain.len() - 1, "every hop of the chain is reported once");
    }

    /// Task #68 (resume token size): `ResumeState.visited`/`walked` are
    /// cumulative across a whole resume chain (see the doc comment on
    /// `ResumeState`), so the encoded token grows every call. On a graph
    /// with realistic (32-hex-char, high-entropy) ids - short test ids like
    /// "a" or zero-padded counters compress far better than a real id and
    /// would hide the problem - a single `DEFAULT_EXPLORATION_BUDGET` (5000)
    /// cutoff alone produces a token in the hundreds of KB. `resume_token`'s
    /// `encode`/`decode` now gzip the JSON before/after base64 for exactly
    /// this reason, measured at ~2.3x smaller on this kind of payload. This
    /// does not make growth O(1) - it rescales the constant - so the
    /// assertion below is a concrete ceiling for this test's own graph size,
    /// not a claim that no larger graph could ever exceed it.
    #[test]
    fn resume_token_stays_a_fraction_of_its_uncompressed_size_across_a_long_chain_on_a_large_graph() {
        let conn = setup();
        // 100 * 140 + 100 + 1 = 14,101 nodes - enough to force several
        // exploration-budget cutoffs (5000 each) with realistic ids.
        let root_id = make_wide_graph_with_realistic_ids(&conn, 100, 140);

        let mut options = TraversalOptions::new(root_id, Direction::Outgoing);
        options.max_fanout = 1_000_000;
        options.max_depth = 10;
        options.exploration_budget = DEFAULT_EXPLORATION_BUDGET;
        let first = traverse(&conn, options).unwrap();
        assert_eq!(first.truncated_by, Some(TruncatedBy::ExplorationBudget));

        let mut token = first.resume_token.clone();
        let mut calls = 1;
        let mut nodes_seen = first.nodes.len();
        let mut max_token_len = token.as_ref().map(|t| t.len()).unwrap_or(0);

        while let Some(t) = token {
            let next = resume(&conn, &t, DEFAULT_EXPLORATION_BUDGET).unwrap();
            calls += 1;
            nodes_seen += next.nodes.len();
            max_token_len = max_token_len.max(next.resume_token.as_ref().map(|t| t.len()).unwrap_or(0));
            token = next.resume_token;
            assert!(calls < 100, "the chain must converge, not re-explore itself forever");
        }

        assert!(calls >= 3, "this graph must force several resumes, not just one: {calls}");
        assert_eq!(nodes_seen, 14_101, "the chain's union must be the full closure");
        // Measured: ~523 KB compressed vs. ~1.08 MB the equivalent
        // uncompressed token reached at the same point in the chain - the
        // ceiling below sits between the two, so a regression that dropped
        // compression would fail this test.
        assert!(
            max_token_len < 700_000,
            "resume token grew to {max_token_len} bytes - compression regression?"
        );
    }

    /// A re-expanded seed re-offers all of its edges, and the first call may
    /// have reported only some of them. Gating on the child *node* dropped
    /// the rest for good; the gate is on the edge id.
    #[test]
    fn a_resumed_seed_reports_the_hop_the_first_call_never_walked() {
        let conn = setup();
        for id in ["a", "b", "c", "d"] {
            make_node(&conn, id);
        }
        // b -> c closes a diamond: the first call reaches c via a -> c and
        // stops, so b -> c is a real edge nobody has walked yet.
        make_edge(&conn, "e_ab", "a", "b", "CALLS");
        make_edge(&conn, "e_ac", "a", "c", "CALLS");
        make_edge(&conn, "e_bc", "b", "c", "CALLS");
        make_edge(&conn, "e_cd", "c", "d", "CALLS");

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.max_depth = 10;
        options.exploration_budget = 3;
        let first = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&first), vec!["a", "b", "c"]);
        assert_eq!(first.edges.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["e_ab", "e_ac"]);
        assert_eq!(first.truncated_by, Some(TruncatedBy::ExplorationBudget));

        let second = resume(&conn, &first.resume_token.unwrap(), 10).unwrap();

        let mut edge_ids: Vec<&str> = second.edges.iter().map(|e| e.id.as_str()).collect();
        edge_ids.sort_unstable();
        assert_eq!(
            edge_ids,
            vec!["e_bc", "e_cd"],
            "b -> c lands on an already-visited node but was never reported, so it is still owed"
        );
        assert_eq!(node_ids(&second), vec!["d"], "c was already returned and must not be returned again");
    }

    /// The same gap at its sharpest: two distinct edges between the same pair
    /// (the schema keys edges by their own id, so parallel call sites are a
    /// real shape). Node-level filtering cannot tell the unreported one from
    /// the reported one.
    #[test]
    fn parallel_edges_between_the_same_pair_are_not_lost_across_a_resume() {
        let conn = setup();
        for id in ["a", "b", "c"] {
            make_node(&conn, id);
        }
        make_edge(&conn, "e1", "a", "b", "CALLS");
        make_edge(&conn, "e2", "a", "b", "CALLS");
        make_edge(&conn, "e3", "a", "c", "CALLS");

        let mut options = TraversalOptions::new("a", Direction::Outgoing);
        options.exploration_budget = 2;
        let first = traverse(&conn, options).unwrap();

        assert_eq!(node_ids(&first), vec!["a", "b"]);
        assert_eq!(first.edges.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["e1"]);
        assert_eq!(first.truncated_by, Some(TruncatedBy::ExplorationBudget));

        let second = resume(&conn, &first.resume_token.unwrap(), 10).unwrap();

        let mut edge_ids: Vec<&str> = second.edges.iter().map(|e| e.id.as_str()).collect();
        edge_ids.sort_unstable();
        assert_eq!(edge_ids, vec!["e2", "e3"], "e2 is a second, unreported edge to an already-visited node");
        assert_eq!(node_ids(&second), vec!["c"], "b is not a new node just because a new edge reaches it");
        assert!(!second.truncated);
    }
}
