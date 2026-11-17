//! The protocol-level failure every MCP tool handler shares. Answers go
//! through `query::ToolAnswer` and `response_bound::render`.

use rmcp::ErrorData;

pub(super) fn internal_error(context: &str, err: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("g-mesh: {context}: {err:#}"), None)
}
