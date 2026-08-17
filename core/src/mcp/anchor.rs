//! Which node does a symbol query hang off? Answered once, here, for the four
//! tools that all hang off exactly one node and differ only in which edges
//! they walk afterwards: `find_references`, `find_callers`, `find_callees`,
//! `find_implementations`.
//!
//! Exactly one of two addressing modes per call. `symbol_id` is the id a
//! caller already holds. `symbol_name` is the whole point of this module: it
//! is resolved by `find_definition::resolve_symbol_name` - the same function
//! `find_definition` itself uses - so an unambiguous name collapses the old
//! mandatory `find_definition`-then-walk pair into a single call. That
//! round-trip is not free: in a stateless CLI-agent conversation every extra
//! turn re-pays the whole resent conversation, which g-mesh-bench measured as
//! a direct cost driver.
//!
//! An ambiguous name is never guessed at and never unioned across candidates:
//! the caller gets `find_definition`'s own ranked candidate page back and
//! picks one, exactly as it would have had it called `find_definition` first.
//! Each candidate carries the `id` to re-call the same tool with, so the
//! fallback is still two calls total, not three - and it terminates, which
//! re-asking by `qualifiedName` would not: the same qualifiedName can name
//! two different declarations (see `DefinitionCandidate::id`).

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination;
use crate::graph::queries;
use crate::storage::write::NodeRecord;

use super::find_definition;
use super::tool_result::{error, internal_error};
use super::SymbolQueryParams;

/// `Ok(Ok(node))`: walk it. `Ok(Err(result))`: a finished response - candidate
/// page or tool-level error - to hand straight back unchanged. The outer
/// `Err` stays reserved for genuine protocol-level failures, as everywhere
/// else in this module tree.
pub(super) type Anchor = Result<Result<NodeRecord, CallToolResult>, ErrorData>;

/// What `symbol_name`/`symbol_id` resolved to, echoed back on every response
/// of the four tools built on [`resolve`] (`find_references`, `find_callers`,
/// `find_callees`, `find_implementations`).
///
/// Exists to remove a round trip, not to add information nothing had before:
/// every field here already lived in the `NodeRecord` each tool resolved and
/// discarded after using it to walk edges. Without an echo, a prompt asking
/// about usages "elsewhere" - i.e. excluding the definition file - forced a
/// `find_definition` call first, purely to learn where the definition lives;
/// `find_definition` was never resolving anything these tools had not already
/// resolved for themselves (task #190, following the round-trip #189
/// measured directly: tt-scenario-impact-requireproject kept a mandatory
/// `find_definition` step even after the trailing-grep habit it originally
/// targeted was fixed).
///
/// Deliberately five fields, not the full node: `startCol`/`endLine`/
/// `endCol`/`signature`/`docComment` answer "what does this symbol look
/// like", which is `find_definition`'s job, not "which node did this call
/// anchor on and where does it live", which is all a follow-up filtering
/// question needs. Widening this to the full node would blur that line and
/// pay full `DefinitionNode` bytes on every page of every one of the four
/// highest-traffic tools for information most callers of this echo do not
/// need.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AnchorInfo {
    pub(super) id: String,
    pub(super) qualified_name: String,
    pub(super) kind: String,
    pub(super) file_path: String,
    pub(super) start_line: i64,
}

impl From<&NodeRecord> for AnchorInfo {
    fn from(node: &NodeRecord) -> Self {
        Self {
            id: node.id.clone(),
            qualified_name: node.qualified_name.clone(),
            kind: node.kind.clone(),
            file_path: node.file_path.clone(),
            start_line: node.start_line,
        }
    }
}

pub(super) fn resolve(conn: &Connection, params: &SymbolQueryParams) -> Anchor {
    // Mirrors `find_definition::handle`'s alternation check: both addressing
    // modes are optional in the schema because JSON Schema cannot say
    // "exactly one of these", so the tool says it instead.
    match (params.symbol_id.as_deref(), params.symbol_name.as_deref()) {
        (Some(symbol_id), None) => by_id(conn, symbol_id),
        (None, Some(name)) => find_definition::resolve_symbol_name(conn, name, params.cursor.as_deref()),
        (Some(_), Some(_)) => error("g-mesh: give either `symbol_id` or `symbol_name`, not both").map(Err),
        (None, None) => {
            error("g-mesh: give either `symbol_id`, or `symbol_name` to have it resolved by name").map(Err)
        }
    }
}

/// Some when `node` is what `symbol_name`/`symbol_id` resolved to, but is a
/// `File` node rather than a declared symbol - reachable because
/// `queries::find_by_name`/`find_by_qualified_name` don't filter by kind, so a
/// name that happens to match a file's basename resolves here instead of
/// erroring. None of the four tools built on this anchor ever walk an edge
/// kind incident on a File node in this direction (imports are `IMPORTS`
/// edges into a Module node - get_dependencies's concern, not this anchor's),
/// so every such call answers a technically-honest but useless empty page.
pub(super) fn file_anchor_hint(node: &NodeRecord) -> Option<&'static str> {
    (node.kind == pagination::FILE_KIND).then_some(
        "This name resolved to a file, not a declared symbol, so the results above are \
         always empty - none of this tool's edge kinds target a file. For \"what imports \
         this file\" use get_dependencies(file_path, direction: \"Incoming\") instead.",
    )
}

fn by_id(conn: &Connection, symbol_id: &str) -> Anchor {
    let anchor = queries::get_node(conn, symbol_id).map_err(|e| internal_error("failed to look up anchor node", e))?;
    match anchor {
        Some(node) => Ok(Ok(node)),
        None => error(format!("g-mesh: no symbol with id '{symbol_id}' found")).map(Err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::queries::upsert_node;
    use crate::storage::schema;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn error_text(result: &CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true), "expected an error result: {:?}", result.content);
        match &result.content[0] {
            rmcp::model::ContentBlock::Text(text) => text.text.clone(),
            other => panic!("expected text content, got {other:?}"),
        }
    }

    /// `Result::unwrap`/`unwrap_err` are unavailable here: `NodeRecord` is
    /// not `Debug`, and making it so for a test's sake is not this change's
    /// business.
    fn expect_finished(anchor: Anchor) -> CallToolResult {
        match anchor.expect("resolution must not fail at the protocol level") {
            Ok(node) => panic!("expected a finished result, got the resolved node {}", node.id),
            Err(result) => result,
        }
    }

    fn expect_node(anchor: Anchor) -> NodeRecord {
        match anchor.expect("resolution must not fail at the protocol level") {
            Ok(node) => node,
            Err(result) => panic!("expected a resolved node, got {:?}", result.content),
        }
    }

    #[test]
    fn neither_addressing_mode_is_a_tool_level_error() {
        let conn = setup();
        let result = expect_finished(resolve(&conn, &SymbolQueryParams::default()));
        let text = error_text(&result);
        assert!(text.contains("symbol_id"), "{text}");
        assert!(text.contains("symbol_name"), "{text}");
    }

    #[test]
    fn both_addressing_modes_at_once_is_a_tool_level_error() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "run", "pkg::run", "a.rs", "rust")).unwrap();

        let params = SymbolQueryParams {
            symbol_id: Some("n1".to_string()),
            symbol_name: Some("run".to_string()),
            ..Default::default()
        };
        // Resolving both to the same node would still be a silent contract
        // violation, so it must fail rather than quietly prefer one.
        let result = expect_finished(resolve(&conn, &params));
        assert!(error_text(&result).contains("not both"));
    }

    #[test]
    fn symbol_name_resolution_prefers_an_exact_qualified_name() {
        let mut conn = setup();
        upsert_node(&mut conn, NodeRecord::new("n1", "Function", "run", "pkg_a::run", "a.rs", "rust")).unwrap();
        upsert_node(&mut conn, NodeRecord::new("n2", "Function", "run", "pkg_b::run", "b.rs", "rust")).unwrap();

        let params = SymbolQueryParams { symbol_name: Some("pkg_b::run".to_string()), ..Default::default() };
        let node = expect_node(resolve(&conn, &params));
        assert_eq!(node.id, "n2", "a qualifiedName must resolve even while the bare name is ambiguous");
    }
}
