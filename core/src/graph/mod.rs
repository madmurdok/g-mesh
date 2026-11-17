pub mod containers;
pub mod pagination;
pub mod queries;
pub mod resume_token;
pub mod traversal;

use serde::{Deserialize, Serialize};

/// Which way to follow edges out of an anchor node.
// Doc comments here are user-facing: `JsonSchema` is derived so
// `get_dependencies`' MCP tool schema can name this exact enum rather than
// restating its variants, and schemars publishes the prose verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Direction {
    /// Edges going out of the anchor node (`fromId = anchor`).
    Outgoing,
    /// Edges coming into the anchor node (`toId = anchor`).
    Incoming,
}
