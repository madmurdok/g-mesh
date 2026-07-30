use std::io::{Read, Write};

use anyhow::{Context, Result};
use base64::prelude::*;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::graph::pagination::Direction;

/// One node a resumed walk restarts from, at the depth it was originally
/// reached at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisitedNode {
    pub id: String,
    pub depth: u32,
}

/// Everything a walk cut short by the exploration budget needs to continue:
/// the shape of the query (so a resumed call cannot silently become a
/// different traversal) plus its state.
///
/// `visited` is both the visited set and the frontier queue. Nothing in the
/// CTE's output records which of the returned nodes had already been fully
/// expanded when the budget ran out, so every one of them is re-seeded -
/// conservative, but it is what makes "no branch is silently dropped" hold
/// without depending on SQLite's internal queue bookkeeping. Kept ordered by
/// (depth, id) so the resumed walk stays breadth-first across the boundary.
///
/// `walked` is the edge side of the same idea: re-expanding a seed re-offers
/// hops the earlier call already reported, and the edge id is the only thing
/// that tells them apart from hops it never got to.
///
/// Both lists are cumulative across an entire resume chain, not just the last
/// call - the seed gate above only holds if the full history rides along -
/// so the encoded token grows with every call and is O(total nodes/edges
/// reported so far), not O(one page). Measured on a synthetic 40,201-node
/// graph (`core/tests/resume_token_size_scratch.rs`, since removed) with
/// realistic 32-hex-char sha256 ids (`plugins/js-ts/src/extract.ts::hash`)
/// and the real `encode`/`decode` below: a single `DEFAULT_EXPLORATION_BUDGET`
/// (5000) cutoff alone already produces a ~580 KB token before compression
/// (~250 KB after); chaining resumes to the graph's edge pushes it to ~4.6 MB
/// raw / ~2 MB compressed. That is nowhere near the 64 MiB NDJSON frame limit
/// (`protocol::ndjson_frame::MAX_LINE_BYTES`) - the actual, concrete transport
/// this token crosses (`mcp/get_dependencies.rs`'s `resume_token` field, one
/// JSON-RPC response over that framing) - so no request is ever rejected.
/// The real cost is different: this token is not just emitted once, it is
/// also the caller's job to echo verbatim as an argument on the *next* call,
/// and the caller in practice is an LLM agent paying for both directions out
/// of a finite context budget. A few hundred KB of opaque base64 for a single
/// budget cutoff - on a graph no larger than a real large monorepo's `IMPORTS`
/// closure - is a real, avoidable tax on that budget, which is why `encode`/
/// `decode` gzip the JSON before/after base64: ~2.3x smaller on this
/// realistic (high-entropy hex id) payload, measured, for zero change to
/// `traversal.rs`'s semantics or its resume-chain correctness. Compression
/// rescales the constant rather than changing the O(n) growth - a
/// restructuring that carried less than the full accumulated lists was
/// considered and rejected: `resume_cte` (traversal.rs) needs every
/// previously-visited node re-seeded, and every previously-walked edge
/// id gated, precisely because the CTE cannot tell which nodes were fully
/// expanded when the budget cut it off (see the doc comment above) - dropping
/// any of that history for graphs with diamonds/cross-edges risks
/// reintroducing the edge-loss bug this design fixed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeState {
    pub direction: Direction,
    pub edge_kind: Option<String>,
    pub max_depth: u32,
    pub max_fanout: u32,
    pub visited: Vec<VisitedNode>,
    pub walked: Vec<String>,
}

/// Opaque to the caller, same convention as `pagination`'s cursors: base64 of
/// a gzipped JSON payload, carrying only graph data the caller has already
/// seen. Gzip (not just base64) is deliberate: this state accumulates across
/// a whole resume chain (see `ResumeState`'s doc comment for measurements),
/// and compression is a free, semantics-preserving way to cut what a caller
/// has to carry and re-transmit call after call.
pub fn encode(state: &ResumeState) -> String {
    let json = serde_json::to_vec(state).expect("resume state is always serializable");
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&json).expect("gzip encoding into an in-memory buffer cannot fail");
    let compressed = gz.finish().expect("gzip encoding into an in-memory buffer cannot fail");
    BASE64_STANDARD.encode(compressed)
}

pub fn decode(raw: &str) -> Result<ResumeState> {
    let compressed = BASE64_STANDARD.decode(raw).context("invalid resume token encoding")?;
    let mut json = Vec::new();
    GzDecoder::new(&compressed[..])
        .read_to_end(&mut json)
        .context("invalid resume token compression")?;
    serde_json::from_slice(&json).context("invalid resume token payload")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ResumeState {
        ResumeState {
            direction: Direction::Incoming,
            edge_kind: Some("CALLS".to_string()),
            max_depth: 3,
            max_fanout: 7,
            visited: vec![
                VisitedNode { id: "a".to_string(), depth: 0 },
                VisitedNode { id: "b".to_string(), depth: 1 },
            ],
            walked: vec!["e_ab".to_string()],
        }
    }

    #[test]
    fn token_round_trips_the_whole_state() {
        let decoded = decode(&encode(&state())).unwrap();

        assert!(matches!(decoded.direction, Direction::Incoming));
        assert_eq!(decoded.edge_kind.as_deref(), Some("CALLS"));
        assert_eq!(decoded.max_depth, 3);
        assert_eq!(decoded.max_fanout, 7);
        assert_eq!(decoded.visited.iter().map(|v| v.id.as_str()).collect::<Vec<_>>(), vec!["a", "b"]);
        assert_eq!(decoded.visited[1].depth, 1);
        assert_eq!(decoded.walked, vec!["e_ab"], "the reported edges gate the resumed seeds");
    }

    #[test]
    fn token_is_opaque_and_rejects_garbage() {
        let token = encode(&state());
        assert!(!token.contains("CALLS"), "the payload must not be readable as plain text");
        assert!(decode("not base64 !!").is_err());
        assert!(decode(&BASE64_STANDARD.encode("{\"nope\":1}")).is_err());
    }
}
