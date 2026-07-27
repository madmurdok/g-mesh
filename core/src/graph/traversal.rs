use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::graph::pagination::Direction;
use crate::graph::queries::map_node_row;
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
/// differs per cause (single-hop pagination / re-rooting on frontier nodes /
/// resume token) - see the architecture doc; none of it is built here.
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
}

/// Builds the bounded walk CTE. Three independent bounds:
///
/// - `maxFanout` (?3): the child-selection subquery is the *driver* of the
///   join (`e.id IN (...)`, no usable index constraint on the edge endpoint),
///   so SQLite evaluates it once per expanded node and then seeks each of at
///   most `maxFanout` children by primary key. Capping expansion this way,
///   rather than filtering an already-scanned adjacency list, is what keeps a
///   hub node's step bounded inside the engine.
/// - `maxDepth` (?4): plain depth cap in the recursive term.
/// - exploration budget (?5): `LIMIT` on the CTE itself - SQLite stops the
///   recursion once initial + recursive rows reach it, regardless of how much
///   frontier is left.
///
/// Cycle safety: each row carries the delimited id path it was reached by and
/// refuses to re-enter a node already on that path. SQLite forbids a second
/// reference to the recursive table, so a global visited set isn't
/// expressible; per-path is the correct-and-expressible option, at the cost
/// of re-visiting diamond-shaped nodes once per path (bounded by the budget,
/// deduplicated in `TraversalResult`).
fn walk_cte(direction: &Direction) -> String {
    let (this_endpoint, other_endpoint) = match direction {
        Direction::Outgoing => ("fromId", "toId"),
        Direction::Incoming => ("toId", "fromId"),
    };

    format!(
        "WITH RECURSIVE walk(node_id, edge_id, depth, path) AS ( \
             SELECT ?1, NULL, 0, '|' || ?1 || '|' \
             UNION ALL \
             SELECT e.{other_endpoint}, e.id, w.depth + 1, w.path || e.{other_endpoint} || '|' \
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
             LIMIT ?5 \
         )"
    )
}

/// Runs a bounded transitive walk from `start_node_id` over `edges`, in one
/// recursive CTE. Reports which limit (if any) cut the result short, so a
/// caller never reads an exhausted budget as "there is nothing more".
pub fn traverse(conn: &Connection, options: TraversalOptions) -> Result<TraversalResult> {
    let cte = walk_cte(&options.direction);
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
        .query_map(
            params![
                options.start_node_id,
                options.edge_kind,
                options.max_fanout,
                options.max_depth,
                options.exploration_budget
            ],
            |row| {
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
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to run bounded traversal")?;

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
        let (fanout_cut, depth_cut) = analyze_cuts(conn, &cte, &options)?;
        match (fanout_cut, depth_cut) {
            (true, _) => Some(TruncatedBy::MaxFanout),
            (false, true) => Some(TruncatedBy::MaxDepth),
            (false, false) => None,
        }
    };

    Ok(TraversalResult {
        nodes,
        edges,
        visited_rows,
        truncated: truncated_by.is_some(),
        truncated_by,
    })
}

/// Re-runs the walk to ask whether either response-level limit actually
/// dropped something: `fanout_cut` - some expanded node had more matching
/// edges than `maxFanout`; `depth_cut` - some node at the depth boundary
/// leads to a node the walk never reached.
fn analyze_cuts(conn: &Connection, cte: &str, options: &TraversalOptions) -> Result<(bool, bool)> {
    let (this_endpoint, other_endpoint) = match options.direction {
        Direction::Outgoing => ("fromId", "toId"),
        Direction::Incoming => ("toId", "fromId"),
    };

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

    conn.query_row(
        &sql,
        params![
            options.start_node_id,
            options.edge_kind,
            options.max_fanout,
            options.max_depth,
            options.exploration_budget
        ],
        |row| Ok((row.get("fanoutCut")?, row.get("depthCut")?)),
    )
    .context("failed to analyse traversal truncation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
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
}
