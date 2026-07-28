//! The three response shapes every MCP tool handler needs - success payload,
//! tool-level failure, and protocol-level failure - factored out once
//! `find_references` needed the exact same trio `find_definition` already
//! had, rather than growing a third private copy with the next tool.

use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::ErrorData;
use serde::Serialize;

pub(super) fn success<T: Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::success(vec![ContentBlock::json(value)?]))
}

pub(super) fn error(message: impl Into<String>) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(message.into())]))
}

pub(super) fn internal_error(context: &str, err: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("g-mesh: {context}: {err:#}"), None)
}
