use anyhow::{Context, Result};
use base64::prelude::*;
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
/// a JSON payload, carrying only graph data the caller has already seen.
pub fn encode(state: &ResumeState) -> String {
    BASE64_STANDARD.encode(serde_json::to_vec(state).expect("resume state is always serializable"))
}

pub fn decode(raw: &str) -> Result<ResumeState> {
    let bytes = BASE64_STANDARD.decode(raw).context("invalid resume token encoding")?;
    serde_json::from_slice(&bytes).context("invalid resume token payload")
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
