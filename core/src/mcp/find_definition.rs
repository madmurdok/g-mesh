//! Real logic behind the `find_definition` MCP tool. Kept out of `mcp/mod.rs`
//! so that file stays pure tool-router wiring - this is where the actual
//! "name or position -> node(s)" decision lives.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rmcp::model::CallToolResult;
use rmcp::ErrorData;
use rusqlite::{Connection, Row};
use serde::Serialize;

use crate::graph::pagination;
use crate::graph::queries;
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::storage::write::NodeRecord;

use super::source;
use super::tool_result::{error, internal_error, success};
use super::FindDefinitionParams;

/// Nothing in the ticket specifies a page size for the ambiguous-candidate
/// list, so 20 is a plain, generous-enough default - there's no existing
/// constant for this shape of list to reuse.
const CANDIDATE_PAGE_SIZE: usize = 20;

/// The full node, returned whenever the lookup is unambiguous: a
/// file+position query, an exact qualifiedName match, or a bare name that
/// happens to match exactly one node.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionNode {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
    start_line: i64,
    start_col: i64,
    end_line: i64,
    end_col: i64,
    signature: Option<String>,
    doc_comment: Option<String>,
    /// Which rung of the ladder reached this - see [`ResolvedBy`]. Absent on a
    /// file+position lookup, which cannot be anything but exact.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_by: Option<ResolvedBy>,
    /// The declaration's own text - see [`source`] for why this is worth its
    /// payload and how it is bounded.
    ///
    /// Absent when the caller opted out with `include_source: false`, or when
    /// the file cannot be read at those coordinates (deleted, or edited since
    /// the walk). Coordinates without text are still a correct answer, so a
    /// snippet that cannot be produced is omitted rather than failing the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<source::Snippet>,
}

impl DefinitionNode {
    /// The node with the rung that reached it, for the name-addressed path.
    fn resolved(node: NodeRecord, by: ResolvedBy) -> Self {
        Self { resolved_by: Some(by), ..Self::from(node) }
    }

    /// Attaches the declaration's text, if a root was given and the file can
    /// still be read at these coordinates.
    ///
    /// `None` for the root means the caller passed `include_source: false` -
    /// the opt-out and the failure both land here, deliberately: from the
    /// response's point of view they are the same thing, a node without a
    /// snippet, and giving them two different shapes would make every consumer
    /// handle two cases to learn nothing.
    fn with_source(mut self, project_root: Option<&Path>) -> Self {
        self.source = project_root
            .and_then(|root| source::read_span(root, &self.file_path, self.start_line, self.end_line));
        self
    }
}

impl From<NodeRecord> for DefinitionNode {
    fn from(n: NodeRecord) -> Self {
        Self {
            id: n.id,
            kind: n.kind,
            name: n.name,
            qualified_name: n.qualified_name,
            file_path: n.file_path,
            start_line: n.start_line,
            start_col: n.start_col,
            end_line: n.end_line,
            end_col: n.end_col,
            signature: n.signature,
            doc_comment: n.doc_comment,
            resolved_by: None,
            source: None,
        }
    }
}

/// One entry in a ranked candidate list for an ambiguous bare name - a
/// preview, not the full node, since the caller is expected to re-query once
/// it picks one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DefinitionCandidate {
    /// The handle to re-query with, and the only one guaranteed to resolve:
    /// `qualifiedName` is not unique either. Excalidraw has two distinct
    /// `getNonDeletedElements` functions - `packages/element/src/index.ts`
    /// and `packages/element/src/Scene.ts` - whose qualifiedName is bare
    /// `getNonDeletedElements` in both, so picking one and asking again by
    /// name returns this very page a second time. Anchoring on `id` always
    /// terminates.
    id: String,
    qualified_name: String,
    file_path: String,
    kind: String,
    /// Signature over docstring when both exist - it's denser and more
    /// identifying in a ranked list than prose.
    preview: Option<String>,
}

/// The standard cursor-pagination envelope, serialized: `Page<T>` itself
/// isn't `Serialize` since it's shared by every list-shaped tool and none of
/// them agree on an item type.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidatePage {
    /// Always `true`, and the reason this field exists at all: the four
    /// symbol-anchored tools return this page *in place of* their own result
    /// shape when a `symbol_name` turns out ambiguous, and both shapes are
    /// `{results, hasMore, nextCursor}`. Without a marker a caller would have
    /// to sniff item fields to tell "here are your callers" from "say which
    /// symbol you meant".
    ambiguous: bool,
    /// Which rung produced this page - always `nameAmbiguous` here. Present so
    /// a caller reads one field to tell an ambiguity from the other kind of
    /// candidate page (`fileName`), rather than inferring it from `ambiguous`.
    resolved_by: ResolvedBy,
    results: Vec<DefinitionCandidate>,
    has_more: bool,
    next_cursor: Option<String>,
}

/// The file-name rung's page. Shares `{ambiguous, resolvedBy, results}` with
/// [`CandidatePage`] so a caller re-queries either the same way, and adds the
/// one thing that page cannot carry: why a name that is plainly in the source
/// resolved to nothing here.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileNamePage {
    resolved_by: ResolvedBy,
    /// Always `false`: these are not competing readings of one name.
    ambiguous: bool,
    explanation: String,
    results: Vec<DefinitionCandidate>,
}

/// Ranks candidates by inbound `REFERENCES`+`CALLS` edge count, descending -
/// a rough proxy for "how central is this symbol", since nothing else in the
/// graph orders same-named definitions across files.
fn find_candidates_by_name(
    conn: &Connection,
    name: &str,
    cursor: Option<&str>,
) -> anyhow::Result<pagination::Page<DefinitionCandidate>> {
    // The `nativeKind` filter matches `graph::queries`': a pending-symbol
    // placeholder is named after the symbol it is waiting for and a re-export
    // one after the symbol it passes through, so without it every file
    // importing or republishing `foo` would offer itself as a candidate `foo`.
    let base_sql = "SELECT n.id AS id, n.qualifiedName AS qualifiedName, n.filePath AS filePath, \
                    n.kind AS kind, n.signature AS signature, n.docComment AS docComment, \
                    CAST((SELECT COUNT(*) FROM edges e WHERE e.toId = n.id AND e.kind IN ('REFERENCES', 'CALLS')) AS REAL) AS score \
                    FROM nodes n WHERE n.name = ?1 AND n.nativeKind IS NOT ?2 AND n.nativeKind IS NOT ?3";

    fn map_row(row: &Row) -> rusqlite::Result<(DefinitionCandidate, f64, String)> {
        let id: String = row.get("id")?;
        let score: f64 = row.get("score")?;
        let candidate = DefinitionCandidate {
            id: id.clone(),
            qualified_name: row.get("qualifiedName")?,
            file_path: row.get("filePath")?,
            kind: row.get("kind")?,
            preview: row
                .get::<_, Option<String>>("signature")?
                .or(row.get::<_, Option<String>>("docComment")?),
        };
        Ok((candidate, score, id))
    }

    pagination::paginate_by_score(
        conn,
        base_sql,
        &[&name, &PENDING_SYMBOL_NATIVE_KIND, &REEXPORT_NATIVE_KIND],
        CANDIDATE_PAGE_SIZE,
        cursor,
        map_row,
    )
}

/// Resolves `find_definition`'s file+position input - always unambiguous by
/// construction, so the answer is a single node, never a candidate list.
fn by_position(
    conn: &Connection,
    project_root: Option<&Path>,
    file_path: &str,
    line: u32,
    col: u32,
) -> Result<CallToolResult, ErrorData> {
    let found = queries::find_by_position(conn, file_path, line, col)
        .map_err(|e| internal_error("failed to resolve file+position", e))?;

    match found {
        Some(node) => success(&DefinitionNode::from(node).with_source(project_root)),
        None => error(format!("g-mesh: no symbol found at {file_path}:{line}:{col}")),
    }
}

/// Which rung of the resolution ladder produced an answer - see
/// `docs/architecture/symbol-resolution-ladder.md`.
///
/// Echoed on every response so a *suggestion* can never be read as a
/// *resolution*. `Id`, `QualifiedName` and `Name` establish that this is the
/// symbol asked for; `NameAmbiguous` and `FileName` establish only that these
/// are candidates worth re-querying. Without the label the two are
/// indistinguishable in the response, and this codebase's standing rule is
/// that a missing edge beats a wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum ResolvedBy {
    /// An exact node id, given by the caller.
    Id,
    /// An exact `qualifiedName` - how a caller re-queries a candidate it picked.
    QualifiedName,
    /// A bare name matching exactly one declaration.
    Name,
    /// A bare name matching several: the page is ranked candidates, not an answer.
    NameAmbiguous,
    /// No declaration carries the name, but a file does - so these are that
    /// file's declarations, offered because a default import binds an export
    /// under a local name this index never sees.
    FileName,
}

/// A resolved anchor and the rung that reached it.
pub(super) struct Resolved {
    pub(super) node: NodeRecord,
    pub(super) by: ResolvedBy,
}

/// Resolves a symbol name to the single node it names: an exact
/// qualifiedName match is tried first as a fast path (this is how a caller
/// re-queries a candidate it picked off a previous ambiguous page), then
/// falls back to a bare-name lookup.
///
/// `Ok(Ok(node))` is that node. `Ok(Err(result))` is a finished response the
/// caller must return unchanged - the ranked candidate page when the name is
/// ambiguous, or the not-found tool error - which is what lets the four
/// symbol-anchored tools accept a `symbol_name` (see `mcp::anchor`) and mean
/// exactly what `find_definition` means by it, down to the error text.
/// Shaped like `find_callers_callees`' old `resolve_anchor` rather than a
/// bespoke enum so every call site is the same two-line `match`.
pub(super) fn resolve_symbol_name(
    conn: &Connection,
    name: &str,
    cursor: Option<&str>,
) -> Result<Result<Resolved, CallToolResult>, ErrorData> {
    let mut exact = queries::find_by_qualified_name(conn, name, None)
        .map_err(|e| internal_error("failed to look up node by qualifiedName", e))?;
    if exact.len() == 1 {
        return Ok(Ok(Resolved { node: exact.remove(0), by: ResolvedBy::QualifiedName }));
    }

    let matches =
        queries::find_by_name(conn, name, None).map_err(|e| internal_error("failed to look up node by name", e))?;

    match matches.len() {
        0 => by_file_name(conn, name),
        1 => Ok(Ok(Resolved {
            node: matches.into_iter().next().expect("len checked above"),
            by: ResolvedBy::Name,
        })),
        _ => {
            let page = find_candidates_by_name(conn, name, cursor)
                .map_err(|e| internal_error("failed to rank ambiguous candidates", e))?;
            success(&CandidatePage {
                ambiguous: true,
                resolved_by: ResolvedBy::NameAmbiguous,
                results: page.results,
                has_more: page.has_more,
                next_cursor: page.next_cursor,
            })
            .map(Err)
        }
    }
}

/// The last rung before a refusal: no declaration carries the name, but a file
/// does.
///
/// 2.9.0 answered this case with prose in the error text and measured well -
/// `ex-default-export-dropdownmenu-group` went from 4 turns to 2. This returns
/// the same facts as the candidate page the ambiguous rung already produces,
/// because the caller then has one contract to know rather than two: re-query
/// by a candidate's `id`. The prose said "re-query by one of those"; the page
/// *is* those.
///
/// `ambiguous: false`, because these are not several readings of one name -
/// they are what a differently-named thing declares. The rung label is what
/// says so.
fn by_file_name(conn: &Connection, name: &str) -> Result<Result<Resolved, CallToolResult>, ErrorData> {
    const MAX_SUGGESTIONS: usize = 5;
    let in_file = queries::find_in_file_named(conn, name, MAX_SUGGESTIONS)
        .map_err(|e| internal_error("failed to look up nodes by file name", e))?;
    if in_file.is_empty() {
        return error(format!("g-mesh: no symbol named '{name}' found")).map(Err);
    }

    let file_path = in_file[0].file_path.clone();
    success(&FileNamePage {
        resolved_by: ResolvedBy::FileName,
        ambiguous: false,
        explanation: format!(
            "No declaration is named '{name}'. The file {file_path} is, and declares these. A \
             default import binds a file's export under whatever local name the importing file \
             chose, and that local name is not indexed - so a name read at a use site can be \
             absent here while the declaration it refers to is present under its own name. \
             Re-query by one of these ids."
        ),
        results: in_file
            .iter()
            .map(|n| DefinitionCandidate {
                id: n.id.clone(),
                qualified_name: n.qualified_name.clone(),
                file_path: n.file_path.clone(),
                kind: n.kind.clone(),
                preview: n.signature.clone().or_else(|| n.doc_comment.clone()),
            })
            .collect(),
    })
    .map(Err)
}

/// `find_definition`'s own symbol-name input: the shared resolution above,
/// with the resolved case formatted as the full node this tool promises.
fn by_name(
    conn: &Connection,
    project_root: Option<&Path>,
    name: &str,
    cursor: Option<&str>,
) -> Result<CallToolResult, ErrorData> {
    match resolve_symbol_name(conn, name, cursor)? {
        Ok(resolved) => success(&DefinitionNode::resolved(resolved.node, resolved.by).with_source(project_root)),
        Err(finished) => Ok(finished),
    }
}

pub(super) fn handle(
    conn: &Arc<Mutex<Connection>>,
    project_root: &Path,
    params: FindDefinitionParams,
) -> Result<CallToolResult, ErrorData> {
    let conn = conn.lock().unwrap();
    // Defaults to on. The snippet is the point of the field - a caller who
    // wants coordinates alone has to say so, rather than every caller having
    // to ask for the thing that saves them a round trip.
    let project_root = params.include_source.unwrap_or(true).then_some(project_root);

    match (params.file_path, params.position, params.symbol_name) {
        (Some(file_path), Some(position), _) => {
            by_position(&conn, project_root, &file_path, position.line, position.col)
        }
        (None, None, Some(name)) => by_name(&conn, project_root, &name, params.cursor.as_deref()),
        (None, None, None) if params.cursor.is_some() => {
            error("g-mesh: `cursor` continues a previous ambiguous symbol_name lookup - give the same symbol_name again")
        }
        (None, None, None) => {
            error("g-mesh: give either `symbol_name`, or both `file_path` and `position`")
        }
        _ => error("g-mesh: `file_path` and `position` must be given together"),
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::graph::queries::{upsert_edge, upsert_node};
    use crate::storage::schema;
    use crate::storage::write::EdgeRecord;

    /// A project root with nothing in it, for the tests that are about
    /// resolution rather than about source. Every snippet lookup under it
    /// misses, so `source` is absent and these assertions read exactly as
    /// they did before the field existed - which is the point: adding the
    /// field must not quietly change what they are testing.
    fn no_sources() -> std::path::PathBuf {
        std::env::temp_dir().join("g-mesh-tests-with-no-sources")
    }

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn node_with_span(id: &str, name: &str, qualified_name: &str, file_path: &str, end: (i64, i64)) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", name, qualified_name, file_path, "rust");
        node.end_line = end.0;
        node.end_col = end.1;
        node
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

    #[test]
    fn ambiguous_bare_name_returns_both_as_ranked_candidates() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();
        // A third, unrelated node calls n2 twice so its inbound CALLS count
        // outranks n1's zero - exercises the ranking, not just presence.
        upsert_node(&mut conn, NodeRecord::new("caller1", "Function", "caller1", "pkg_c::caller1", "c/lib.rs", "rust"))
            .unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e1", "caller1", "n2", "CALLS", "tree-sitter", true)).unwrap();
        upsert_edge(&mut conn, EdgeRecord::new("e2", "caller1", "n2", "REFERENCES", "tree-sitter", true)).unwrap();

        let params = FindDefinitionParams {
            symbol_name: Some("run".to_string()),
            file_path: None,
            position: None,
            cursor: None,
            include_source: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), &no_sources(), params).unwrap();
        let body = json_body(&result);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2, "both same-named symbols must come back as candidates");
        assert_eq!(results[0]["qualifiedName"], "pkg_b::run", "the higher inbound-edge count must rank first");
        assert_eq!(results[1]["qualifiedName"], "pkg_a::run");
    }

    /// Neither placeholder kind is a definition: one is named after a symbol
    /// this file imports, the other after one it only republishes, and a
    /// monorepo has a barrel republishing almost everything - so without the
    /// filter a plain name lookup would turn ambiguous project-wide.
    #[test]
    fn placeholders_named_after_a_symbol_are_not_definition_candidates() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "mutate", "mutate", "target.ts", (5, 0))).unwrap();

        for (id, native_kind, file) in [
            ("pending", "pending_symbol", "caller.ts"),
            ("reexported", "reexport", "index.ts"),
        ] {
            let mut placeholder =
                NodeRecord::new(id, "Module", "mutate", "target.ts#mutate", file, "typescript");
            placeholder.native_kind = Some(native_kind.to_string());
            placeholder.end_line = 5;
            upsert_node(&mut conn, placeholder).unwrap();
        }

        let params = FindDefinitionParams {
            symbol_name: Some("mutate".to_string()),
            file_path: None,
            position: None,
            cursor: None,
            include_source: None,
        };
        let body = json_body(&handle(&Arc::new(Mutex::new(conn)), &no_sources(), params).unwrap());
        assert_eq!(body["ambiguous"], serde_json::Value::Null, "only one node is a real definition");
        assert_eq!(body["filePath"], "target.ts");
    }

    #[test]
    fn file_and_position_query_returns_a_single_node_not_a_list() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();

        let params = FindDefinitionParams {
            symbol_name: None,
            file_path: Some("a/lib.rs".to_string()),
            position: Some(crate::protocol::types::Position { line: 2, col: 0 }),
            cursor: None,
            include_source: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), &no_sources(), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["id"], "n1");
        assert_eq!(body["qualifiedName"], "pkg_a::run");
        assert!(body.get("results").is_none(), "an unambiguous file+position query must not be wrapped in a list");
    }

    #[test]
    fn qualified_name_requery_returns_the_exact_node() {
        let mut conn = setup();
        upsert_node(&mut conn, node_with_span("n1", "run", "pkg_a::run", "a/lib.rs", (5, 0))).unwrap();
        upsert_node(&mut conn, node_with_span("n2", "run", "pkg_b::run", "b/lib.rs", (5, 0))).unwrap();

        let params = FindDefinitionParams {
            symbol_name: Some("pkg_b::run".to_string()),
            file_path: None,
            position: None,
            cursor: None,
            include_source: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), &no_sources(), params).unwrap();
        let body = json_body(&result);
        assert_eq!(body["id"], "n2");
        assert_eq!(body["qualifiedName"], "pkg_b::run");
    }

    #[test]
    fn no_match_is_a_tool_level_error() {
        let conn = setup();
        let params = FindDefinitionParams {
            symbol_name: Some("does_not_exist".to_string()),
            file_path: None,
            position: None,
            cursor: None,
            include_source: None,
        };
        let result = handle(&Arc::new(Mutex::new(conn)), &no_sources(), params).unwrap();
        assert!(error_text(&result).contains("does_not_exist"));
    }

    #[test]
    fn neither_name_nor_position_is_a_tool_level_error() {
        let conn = setup();
        let params = FindDefinitionParams { symbol_name: None, file_path: None, position: None, cursor: None, include_source: None };
        let result = handle(&Arc::new(Mutex::new(conn)), &no_sources(), params).unwrap();
        assert!(error_text(&result).contains("symbol_name"));
    }

    #[test]
    fn ambiguous_candidates_paginate_across_cursor_continuation() {
        let mut conn = setup();
        // One more node than CANDIDATE_PAGE_SIZE, so the first page is
        // truncated and a second `handle()` call with its cursor must
        // return exactly the remainder, with nothing repeated or skipped.
        let total = CANDIDATE_PAGE_SIZE + 1;
        for i in 0..total {
            let id = format!("n{i}");
            upsert_node(&mut conn, node_with_span(&id, "run", &format!("pkg{i}::run"), "a/lib.rs", (5, 0))).unwrap();
        }
        let conn = Arc::new(Mutex::new(conn));

        let first_params =
            FindDefinitionParams { symbol_name: Some("run".to_string()), file_path: None, position: None, cursor: None, include_source: None };
        let first = handle(&conn, &no_sources(), first_params).unwrap();
        let first_body = json_body(&first);
        let first_results = first_body["results"].as_array().unwrap();
        assert_eq!(first_results.len(), CANDIDATE_PAGE_SIZE);
        assert_eq!(first_body["hasMore"], true);
        let cursor = first_body["nextCursor"].as_str().unwrap().to_string();

        let second_params = FindDefinitionParams {
            symbol_name: Some("run".to_string()),
            file_path: None,
            position: None,
            cursor: Some(cursor),
            include_source: None,
        };
        let second = handle(&conn, &no_sources(), second_params).unwrap();
        let second_body = json_body(&second);
        let second_results = second_body["results"].as_array().unwrap();
        assert_eq!(second_results.len(), 1, "the one remaining candidate must land on the second page");
        assert_eq!(second_body["hasMore"], false);

        let mut all_ids: Vec<String> = first_results
            .iter()
            .chain(second_results.iter())
            .map(|c| c["qualifiedName"].as_str().unwrap().to_string())
            .collect();
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), total, "every candidate must appear exactly once across both pages");
    }


    /// The ladder's contract: every answer says which rung reached it, so a
    /// caller can tell "this is your symbol" from "these might be".
    #[test]
    fn an_exact_name_reports_the_rung_that_resolved_it() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new("run", "Function", "run", "pkg::run", "src/run.rs", "rust"),
        )
        .unwrap();

        let by_qualified = json_body(&by_name(&conn, None, "pkg::run", None).unwrap());
        assert_eq!(by_qualified["resolvedBy"], "qualifiedName");

        let by_bare = json_body(&by_name(&conn, None, "run", None).unwrap());
        assert_eq!(by_bare["resolvedBy"], "name");
    }

    /// The rung 2.9.0 shipped as prose in an error, now the same facts in the
    /// shape the ambiguous rung already uses - so a caller has one contract to
    /// know (re-query by a candidate's id) rather than two.
    #[test]
    fn a_name_only_a_file_carries_returns_that_files_declarations_as_candidates() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new(
                "menu_group",
                "Function",
                "MenuGroup",
                "MenuGroup",
                "src/components/DropdownMenuGroup.tsx",
                "typescript",
            ),
        )
        .unwrap();

        let body = json_body(&by_name(&conn, None, "DropdownMenuGroup", None).unwrap());

        assert_eq!(body["resolvedBy"], "fileName");
        // Not an ambiguity: these are not competing readings of one name, they
        // are what a differently-named thing declares. The rung says which.
        assert_eq!(body["ambiguous"], false);
        assert_eq!(body["results"][0]["id"], "menu_group");
        assert_eq!(body["results"][0]["qualifiedName"], "MenuGroup");
        assert!(
            body["explanation"].as_str().expect("an explanation").contains("default import"),
            "the page has to say why a name that is plainly in the source resolved to nothing: {body}"
        );
    }

    /// A name that is nowhere keeps the short refusal. A page of candidates
    /// with nothing in it would be a worse answer than saying so.
    #[test]
    fn a_name_matching_neither_a_declaration_nor_a_file_is_still_refused() {
        let mut conn = setup();
        upsert_node(
            &mut conn,
            NodeRecord::new("run", "Function", "run", "pkg::run", "src/run.rs", "rust"),
        )
        .unwrap();

        let result = by_name(&conn, None, "NoSuchThingAnywhere", None).unwrap();

        assert_eq!(error_text(&result), "g-mesh: no symbol named 'NoSuchThingAnywhere' found");
    }

    /// The ambiguous rung keeps its own label, so the three candidate-shaped
    /// answers stay distinguishable by one field rather than by sniffing.
    #[test]
    fn an_ambiguous_name_labels_its_page_as_the_ambiguity_it_is() {
        let mut conn = setup();
        for (id, file) in [("a", "a.rs"), ("b", "b.rs")] {
            upsert_node(&mut conn, NodeRecord::new(id, "Function", "run", "run", file, "rust")).unwrap();
        }

        let body = json_body(&by_name(&conn, None, "run", None).unwrap());

        assert_eq!(body["ambiguous"], true);
        assert_eq!(body["resolvedBy"], "nameAmbiguous");
    }

    // --- the declaration's source ------------------------------------------

    /// A project whose one file really contains `body`, so the snippet is read
    /// off disk exactly as a real answer would read it.
    fn project_with(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("failed to create a temp project");
        std::fs::create_dir_all(dir.path().join("a")).expect("failed to create the fixture directory");
        std::fs::write(dir.path().join("a/lib.rs"), body).expect("failed to write the fixture");
        dir
    }

    fn definition_of(name: &str, project_root: &std::path::Path, span: (i64, i64)) -> serde_json::Value {
        let mut conn = setup();
        let mut node = node_with_span("n1", name, &format!("pkg::{name}"), "a/lib.rs", (span.1, 0));
        node.start_line = span.0;
        upsert_node(&mut conn, node).unwrap();
        json_body(&by_name(&conn, Some(project_root), name, None).unwrap())
    }

    /// The point of the whole change: the answer to "where is this defined"
    /// carries what is there, so the caller does not spend a round trip - worth
    /// 18,000-22,000 tokens at this tool's prompt prefix - reading it.
    #[test]
    fn a_definition_carries_the_declarations_own_source() {
        let project = project_with("mod a;\nfn run() {\n    work();\n}\nfn other() {}\n");

        let body = definition_of("run", project.path(), (1, 3));

        assert_eq!(body["source"]["text"], "fn run() {\n    work();\n}");
        assert_eq!(body["source"]["firstLine"], 2, "1-based, as an editor shows it");
        assert!(body["source"]["omittedLines"].is_null(), "a complete snippet says nothing about omissions");
        // The coordinates are still there and still 0-based: the snippet is an
        // addition, not a replacement, and anything doing arithmetic on
        // startLine must keep working.
        assert_eq!(body["startLine"], 1);
    }

    #[test]
    fn include_source_false_leaves_the_response_exactly_as_it_was() {
        let project = project_with("fn run() {\n    work();\n}\n");
        let mut conn = setup();
        let mut node = node_with_span("n1", "run", "pkg::run", "a/lib.rs", (2, 0));
        node.start_line = 0;
        upsert_node(&mut conn, node).unwrap();
        let conn = Arc::new(Mutex::new(conn));

        let params = |include| FindDefinitionParams {
            symbol_name: Some("run".to_string()),
            file_path: None,
            position: None,
            cursor: None,
            include_source: include,
        };

        let opted_out = json_body(&handle(&conn, project.path(), params(Some(false))).unwrap());
        let default_on = json_body(&handle(&conn, project.path(), params(None)).unwrap());

        assert!(opted_out["source"].is_null(), "include_source: false must omit it");
        assert!(!default_on["source"].is_null(), "omitting the flag must default to on");
        assert_eq!(opted_out["id"], default_on["id"], "nothing else about the answer changes");
        assert_eq!(opted_out["startLine"], default_on["startLine"]);
    }

    /// The index outliving the file it describes is ordinary - a file edited
    /// or deleted since the last walk. Coordinates are still a correct answer,
    /// so the snippet goes missing rather than the call failing.
    #[test]
    fn a_definition_whose_file_no_longer_matches_still_answers_with_coordinates() {
        let project = project_with("fn run() {}\n");

        // The node claims lines 40-45; the file has one line.
        let body = definition_of("run", project.path(), (40, 45));

        assert_eq!(body["qualifiedName"], "pkg::run", "the definition still resolves");
        assert_eq!(body["startLine"], 40);
        assert!(body["source"].is_null(), "a snippet that cannot be read honestly is absent");
    }

    /// The cap has to be exercised on a real declaration, and the truncation
    /// has to be visible: a caller reading a cut body as a complete one draws
    /// conclusions from code that is not there.
    #[test]
    fn a_declaration_past_the_cap_is_cut_visibly() {
        let long: String = (0..source::MAX_LINES + 20).map(|n| format!("    let x{n} = {n};\n")).collect();
        let project = project_with(&format!("fn run() {{\n{long}}}\n"));

        let body = definition_of("run", project.path(), (0, (source::MAX_LINES + 21) as i64));

        let text = body["source"]["text"].as_str().expect("a snippet");
        assert_eq!(text.lines().count(), source::MAX_LINES);
        assert_eq!(body["source"]["omittedLines"], 22, "the cut says exactly how much is missing");
    }
}
