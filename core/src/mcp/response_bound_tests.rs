//! The whole-response byte ceiling (`MAX_RESPONSE_BYTES`) on the edge tools
//! that page rows (`find_callers`, `find_callees`, `find_references`,
//! single-hop `find_implementations`) and on the `files` answer, through their
//! real handlers over an in-memory index: every response fits, rows are only
//! cut (never lost) at the budget, and every tally the byte caps shorten says
//! so.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rmcp::model::CallToolResult;
use rusqlite::Connection;

use super::session_hints::SessionHints;
use super::unlinked::CandidateTally;
use super::{find_callers_callees, find_implementations, find_references};
use super::{Answer, FindImplementationsParams, SymbolQueryParams};
use crate::daemon::manifest::{Capabilities, ReceiverCallResolution};
use crate::embedding::EmbeddingPipeline;
use crate::graph::pagination::{self, EXCLUDED_TALLY_MAX_BYTES, FILE_TALLY_MAX_BYTES, MAX_RESPONSE_BYTES};
use crate::mcp::query_shapes::QueryShapes;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord};

/// `provenance::PENDING_FILES_MAX_BYTES`.
const PENDING_FILES_MAX_BYTES: usize = 1_500;

/// `unlinked::MAX_UNLINKED_FILE_TALLY_BYTES`, private to that module.
const UNLINKED_FILE_TALLY_MAX_BYTES: usize = 1_000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Tool {
    Callers,
    Callees,
    References,
    Implementations,
}

impl Tool {
    /// The row field naming the far end of each edge.
    fn row_id(self) -> &'static str {
        match self {
            Tool::Callers => "callerSymbolId",
            Tool::Callees => "calleeSymbolId",
            Tool::References => "referencingSymbolId",
            Tool::Implementations => "implementingSymbolId",
        }
    }

    /// The edge kind and direction (true: into the anchor) its rows walk.
    fn edges(self) -> (&'static str, bool) {
        match self {
            Tool::Callers | Tool::References => ("CALLS", true),
            Tool::Callees => ("CALLS", false),
            Tool::Implementations => ("SUPERTYPE_OF", true),
        }
    }
}

/// One fixture: `rows` far-end functions `n000..` (qualified names padded by
/// `row_pad` bytes, each in its own file `f000<fpad z's>.rs`), plus `ex`
/// `REFERENCES` edges in the same direction to functions `h00..` in distinct
/// files `h00<ex_pad y's>.rs`. `pending` puts the language's semantic pass
/// in flight with every one of those files pending.
#[derive(Clone, Copy, Debug)]
struct Shape {
    rows: usize,
    row_pad: usize,
    fpad: usize,
    ex: usize,
    ex_pad: usize,
    pending: bool,
}

impl Shape {
    fn rows(rows: usize) -> Self {
        Shape { rows, row_pad: 0, fpad: 0, ex: 0, ex_pad: 0, pending: false }
    }
}

fn row_file(i: usize, fpad: usize) -> String {
    format!("f{i:03}{}.rs", "z".repeat(fpad))
}

fn excluded_file(j: usize, ex_pad: usize) -> String {
    format!("h{j:02}{}.rs", "y".repeat(ex_pad))
}

fn build(shape: Shape, tool: Tool) -> Arc<IndexStore> {
    let (kind, incoming) = tool.edges();
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    let mut diff = Diff::default();
    diff.upsert_nodes.push(NodeRecord::new("t", "Function", "t", "pkg::t", "t.rs", "rust"));
    let mut pending_files = Vec::new();
    let mut far_end =
        |diff: &mut Diff, id: String, qualified: String, file: String, edge: String, kind: &str| {
            diff.upsert_nodes.push(NodeRecord::new(&id, "Function", &id, qualified, &file, "rust"));
            let (from, to) = if incoming { (id.as_str(), "t") } else { ("t", id.as_str()) };
            diff.upsert_edges.push(EdgeRecord::new(edge, from, to, kind, "tree-sitter", true));
            pending_files.push(file);
        };
    for i in 0..shape.rows {
        let id = format!("n{i:03}");
        let qualified = format!("pkg::{id}{}", "x".repeat(shape.row_pad));
        far_end(&mut diff, id, qualified, row_file(i, shape.fpad), format!("e{i:03}"), kind);
    }
    for j in 0..shape.ex {
        let id = format!("h{j:02}");
        let qualified = format!("pkg::{id}");
        far_end(&mut diff, id, qualified, excluded_file(j, shape.ex_pad), format!("r{j:02}"), "REFERENCES");
    }
    apply_diff(&mut conn, &diff).unwrap();
    if shape.pending {
        conn.execute(
            "INSERT INTO semantic_pending (language, since) VALUES ('rust', '2026-09-26T10:14:03Z')",
            [],
        )
        .unwrap();
        for file in &pending_files {
            conn.execute(
                "INSERT INTO semantic_pending_files (language, filePath) VALUES ('rust', ?1)",
                [file],
            )
            .unwrap();
        }
    }
    Arc::new(IndexStore::new(conn))
}

fn capabilities(pending: bool) -> HashMap<String, Capabilities> {
    if !pending {
        return HashMap::new();
    }
    HashMap::from([(
        "rust".to_string(),
        Capabilities {
            semantic_pass: true,
            semantic_sweep: false,
            semantic_prepare: false,
            files_created: false,
            resolution_delta: false,
            receiver_calls: ReceiverCallResolution::Resolved,
            receiver_calls_structural: ReceiverCallResolution::Unresolved,
            member_overrides: crate::daemon::manifest::MemberOverrides::None,
        },
    )])
}

fn body(result: &CallToolResult) -> (serde_json::Value, usize) {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    match &result.content[0] {
        rmcp::model::ContentBlock::Text(text) => (serde_json::from_str(&text.text).unwrap(), text.text.len()),
        other => panic!("expected text/json content, got {other:?}"),
    }
}

/// `tool` on anchor `t` with a fresh session (so every once-per-session
/// sentence rides along: the widest response).
fn ask(
    tool: Tool,
    store: &Arc<IndexStore>,
    pending: bool,
    limit: u32,
    cursor: Option<String>,
    answer: Option<Answer>,
) -> (serde_json::Value, usize) {
    let caps = capabilities(pending);
    let embedding = EmbeddingPipeline::disabled();
    let shapes = QueryShapes::shipped();
    let hints = SessionHints::default();
    let params = SymbolQueryParams {
        symbol_id: Some("t".into()),
        limit: Some(limit),
        cursor: cursor.clone(),
        answer,
        ..Default::default()
    };
    let result = match tool {
        Tool::Callers => {
            find_callers_callees::handle_callers(store, &embedding, shapes, &caps, &hints, params)
        }
        Tool::Callees => find_callers_callees::handle_callees(store, &embedding, shapes, &caps, params),
        Tool::References => find_references::handle(store, &embedding, shapes, &caps, &hints, params),
        Tool::Implementations => find_implementations::dispatch(
            store,
            &embedding,
            shapes,
            &caps,
            FindImplementationsParams {
                symbol_id: Some("t".into()),
                limit: Some(limit),
                cursor,
                ..Default::default()
            },
        ),
    };
    body(&result.unwrap())
}

fn rows(body: &serde_json::Value) -> &Vec<serde_json::Value> {
    body["results"].as_array().unwrap_or_else(|| panic!("no results array: {body}"))
}

fn json_len(value: &serde_json::Value) -> usize {
    value.to_string().len()
}

// --- 1. every response fits ---------------------------------------------------

/// Row paddings swept per configuration: fine steps (each shifts where the
/// byte cut lands relative to the envelope), then rows of hundreds and
/// thousands of bytes, where only a handful fit beside the tallies.
fn row_pads() -> Vec<usize> {
    (0..40).step_by(3).chain([97, 401, 1_999, 6_000]).collect()
}

/// `tool` over 300 rows at `limit: 200` for every row padding and each of
/// `configs` (`fpad`, `ex`, `ex_pad`, `pending`): the whole response is
/// inside `MAX_RESPONSE_BYTES` and carries at least one row.
fn sweep(tool: Tool, configs: &[(usize, usize, usize, bool)]) {
    let mut cut_by_bytes = 0;
    for &(fpad, ex, ex_pad, pending) in configs {
        for row_pad in row_pads() {
            let shape = Shape { rows: 300, row_pad, fpad, ex, ex_pad, pending };
            let (body, bytes) = ask(tool, &build(shape, tool), pending, 200, None, None);
            assert!(bytes <= MAX_RESPONSE_BYTES, "{tool:?} {shape:?}: {bytes} bytes");
            let n = rows(&body).len();
            assert!(n >= 1, "{tool:?} {shape:?}: no row although one fits");
            assert_eq!(body["hasMore"], true, "{tool:?} {shape:?}");
            if n < 200 {
                cut_by_bytes += 1;
            }
        }
    }
    assert!(cut_by_bytes > 0, "{tool:?}: the sweep never reached the byte budget");
}

/// 50 excluded files of 30-char paths (once ~1.6 KB over the ceiling) and longer
/// excluded paths, with and without a pending pass naming every file.
/// Control: measure the rows alone in `handle_callees`' `response_len`
/// (`wire_len(&candidate.results)`) -> over.
#[test]
fn every_callee_page_fits_the_ceiling() {
    sweep(Tool::Callees, &[(0, 0, 0, false), (0, 50, 30, false), (60, 50, 100, true)]);
}

/// Long caller file paths fill the `files` tally (200 files of ~205-char
/// paths beside 50 excluded files once came to ~59 KB). Control as for
/// callees, in `handle_callers_in`.
#[test]
fn every_caller_page_fits_the_ceiling() {
    sweep(Tool::Callers, &[(0, 0, 0, false), (200, 50, 30, false), (60, 50, 100, true)]);
}

/// Control as for callees, in `find_references::handle_in`.
#[test]
fn every_reference_page_fits_the_ceiling() {
    sweep(Tool::References, &[(0, 0, 0, false), (200, 50, 30, false), (60, 0, 0, true)]);
}

/// Control as for callees, in `find_implementations::handle_in`.
#[test]
fn every_single_hop_implementation_page_fits_the_ceiling() {
    sweep(Tool::Implementations, &[(0, 0, 0, false), (60, 0, 0, false), (200, 0, 0, true)]);
}

/// The `files` answer over 1..250 distinct files and path lengths up to 200
/// bytes, beside a 50-file excluded tally and a pending block: inside the
/// ceiling, at least one file named, and `total` exact. Control: build the
/// sent `Summary` at `MAX_RESPONSE_BYTES` instead of `fit_budget`'s budget
/// in `answer::respond` -> over.
#[test]
fn every_files_answer_fits_the_ceiling() {
    for tool in [Tool::Callers, Tool::Callees, Tool::References] {
        for files in [1usize, 50, 200, 250] {
            for fpad in [0usize, 60, 200] {
                for (ex, ex_pad, pending) in [(0usize, 0usize, false), (50, 100, true)] {
                    let shape = Shape { rows: files, row_pad: 0, fpad, ex, ex_pad, pending };
                    let (body, bytes) =
                        ask(tool, &build(shape, tool), pending, 200, None, Some(Answer::Files));
                    assert!(bytes <= MAX_RESPONSE_BYTES, "{tool:?} {shape:?}: {bytes} bytes");
                    let named = body["files"].as_array().unwrap_or_else(|| panic!("{tool:?}: {body}"));
                    assert!(!named.is_empty(), "{tool:?} {shape:?}: no file although one fits");
                    // `find_references` counts the REFERENCES edges too.
                    let expected = if tool == Tool::References { files + ex } else { files };
                    assert_eq!(body["total"], expected, "{tool:?} {shape:?}");
                }
            }
        }
    }
}

/// A row longer than the ceiling still comes back, alone, with a cursor:
/// the page is never emptied by its budget.
#[test]
fn a_row_wider_than_the_ceiling_is_sent_alone() {
    for tool in [Tool::Callers, Tool::Callees, Tool::References, Tool::Implementations] {
        let shape = Shape { row_pad: MAX_RESPONSE_BYTES + 100, ..Shape::rows(3) };
        let (body, _) = ask(tool, &build(shape, tool), false, 200, None, None);
        assert_eq!(rows(&body).len(), 1, "{tool:?}");
        assert_eq!(body["hasMore"], true, "{tool:?}");
    }
}

// --- 2. a byte-cut tally says so -------------------------------------------

/// 150 caller files of ~65-byte paths on a 5-row page - under the 200-entry
/// cap, so only the byte cap can cut it: the `files` tally is cut to
/// `FILE_TALLY_MAX_BYTES`, flagged, and names fewer files than the walk
/// reaches while `total` stays exact. Controls: drop the `truncate_to_bytes`
/// call in `tally_edge_files_bounded`, or leave its byte cut out of the
/// flag it returns.
#[test]
fn a_files_tally_cut_by_bytes_is_flagged() {
    for tool in [Tool::Callers, Tool::References] {
        let shape = Shape { fpad: 60, ..Shape::rows(150) };
        let (body, bytes) = ask(tool, &build(shape, tool), false, 5, None, None);
        assert!(bytes <= MAX_RESPONSE_BYTES, "{tool:?}: {bytes}");
        let files = &body["files"];
        assert!(json_len(files) <= FILE_TALLY_MAX_BYTES, "{tool:?}: {} bytes", json_len(files));
        assert_eq!(body["filesTruncated"], true, "{tool:?}: {body}");
        let named = files.as_array().unwrap();
        assert!(!named.is_empty() && named.len() < 200, "{tool:?}: {} files", named.len());
        let tallied: i64 = named.iter().map(|tally| tally["refs"].as_i64().unwrap()).sum();
        assert_eq!(body["total"], 150, "{tool:?}");
        assert!((tallied as usize) < 150, "{tool:?}");
    }
}

/// 201 short-path caller files: the tally holds 200 and says one was left
/// out. Control: query `tally_edge_files_bounded` at `MAX_FILE_TALLY`
/// instead of one past it (200 files, no flag).
#[test]
fn a_files_tally_cut_by_its_entry_cap_is_flagged() {
    for tool in [Tool::Callers, Tool::References] {
        let (body, _) = ask(tool, &build(Shape::rows(201), tool), false, 5, None, None);
        assert_eq!(body["files"].as_array().unwrap().len(), 200, "{tool:?}");
        assert_eq!(body["filesTruncated"], true, "{tool:?}");
    }
}

/// A tally neither cap touches: every file named, refs summing to `total`,
/// and no `filesTruncated` key.
#[test]
fn a_complete_files_tally_has_no_truncation_key() {
    for tool in [Tool::Callers, Tool::References] {
        let (body, _) = ask(tool, &build(Shape::rows(30), tool), false, 5, None, None);
        let named = body["files"].as_array().unwrap();
        assert_eq!(named.len(), 30, "{tool:?}");
        let tallied: i64 = named.iter().map(|tally| tally["refs"].as_i64().unwrap()).sum();
        assert_eq!(body["total"], tallied, "{tool:?}");
        assert!(body.get("filesTruncated").is_none(), "{tool:?}: {body}");
    }
}

/// 30 excluded files of ~105-byte paths - under the 50-entry cap, so only
/// the byte cap can cut it: `excludedReferences.files` is cut to
/// `EXCLUDED_TALLY_MAX_BYTES` and flagged, its `count` still exact; a short
/// one is complete and unflagged. Controls: drop the `truncate_to_bytes` call
/// in `ExcludedReferences::naming_only_new`, or leave its cut out of the flag.
#[test]
fn an_excluded_tally_cut_by_bytes_is_flagged() {
    for tool in [Tool::Callers, Tool::Callees] {
        let shape = Shape { ex: 30, ex_pad: 100, ..Shape::rows(3) };
        let (body, _) = ask(tool, &build(shape, tool), false, 200, None, None);
        let excluded = &body["excludedReferences"];
        assert_eq!(excluded["count"], 30, "{tool:?}: {body}");
        assert!(json_len(&excluded["files"]) <= EXCLUDED_TALLY_MAX_BYTES, "{tool:?}: {excluded}");
        assert!(!excluded["files"].as_array().unwrap().is_empty(), "{tool:?}");
        assert_eq!(excluded["filesTruncated"], true, "{tool:?}: {excluded}");

        let shape = Shape { ex: 10, ex_pad: 1, ..Shape::rows(3) };
        let (body, _) = ask(tool, &build(shape, tool), false, 200, None, None);
        let excluded = &body["excludedReferences"];
        assert_eq!(excluded["count"], 10, "{tool:?}");
        assert_eq!(excluded["files"].as_array().unwrap().len(), 10, "{tool:?}");
        assert!(excluded.get("filesTruncated").is_none(), "{tool:?}: {excluded}");
    }
}

/// 30 pending files of ~105-byte paths, all on the page: `pendingFiles` is
/// cut to `PENDING_FILES_MAX_BYTES` and the rest are counted, so listed plus
/// omitted is every pending file the response touches. Control: drop the
/// `truncate_to_bytes` call in `Resolved::disclose`.
#[test]
fn pending_files_cut_by_bytes_are_counted() {
    for tool in [Tool::Callers, Tool::Callees, Tool::References, Tool::Implementations] {
        let shape = Shape { fpad: 100, pending: true, ..Shape::rows(30) };
        let (body, bytes) = ask(tool, &build(shape, tool), true, 200, None, None);
        assert!(bytes <= MAX_RESPONSE_BYTES, "{tool:?}: {bytes}");
        assert_eq!(rows(&body).len(), 30, "{tool:?}");
        let provenance = &body["provenance"];
        let listed = provenance["pendingFiles"].as_array().unwrap_or_else(|| panic!("{tool:?}: {body}"));
        assert!(
            json_len(&provenance["pendingFiles"]) <= PENDING_FILES_MAX_BYTES,
            "{tool:?}: {}",
            json_len(&provenance["pendingFiles"])
        );
        assert!(!listed.is_empty() && listed.len() < 25, "{tool:?}: {} listed", listed.len());
        let omitted =
            provenance["pendingFilesOmitted"].as_u64().unwrap_or_else(|| panic!("{tool:?}: {body}"));
        assert_eq!(listed.len() + omitted as usize, 30, "{tool:?}: {provenance}");
    }
}

/// The candidate tallies (`unlinkedUsages`, `untypedReceiverCalls`) are
/// capped in bytes as well as entries: twenty ~105-byte paths are cut to the
/// byte cap and flagged, `count` untouched. Control: drop the
/// `truncate_to_bytes` call in `CandidateTally::from_files`.
#[test]
fn a_candidate_tally_cut_by_bytes_is_flagged() {
    let by_file: Vec<(String, i64)> =
        (0..20).map(|i| (format!("f{i:02}{}.rs", "p".repeat(100)), 1)).collect();
    let tally = CandidateTally::from_files(20, by_file, "hint").unwrap();
    let wire = serde_json::to_value(&tally).unwrap();
    assert!(json_len(&wire["files"]) <= UNLINKED_FILE_TALLY_MAX_BYTES, "{}", json_len(&wire["files"]));
    assert!(!wire["files"].as_array().unwrap().is_empty());
    assert_eq!(wire["filesTruncated"], true, "{wire}");
    assert_eq!(wire["count"], 20);
}

/// The `files` answer cut to fit the ceiling says so; one that fits whole
/// does not. Control: in `answer::respond`, report only
/// `counted.files_truncated` (drop `|| cut`).
#[test]
fn a_files_answer_cut_by_the_ceiling_is_flagged() {
    for tool in [Tool::Callers, Tool::Callees, Tool::References] {
        let shape = Shape { fpad: 200, ..Shape::rows(200) };
        let (body, bytes) = ask(tool, &build(shape, tool), false, 200, None, Some(Answer::Files));
        assert!(bytes <= MAX_RESPONSE_BYTES, "{tool:?}: {bytes}");
        assert_eq!(body["total"], 200, "{tool:?}");
        assert!(body["files"].as_array().unwrap().len() < 200, "{tool:?}");
        assert_eq!(body["filesTruncated"], true, "{tool:?}");

        let (body, _) = ask(tool, &build(Shape::rows(40), tool), false, 200, None, Some(Answer::Files));
        assert_eq!(body["files"].as_array().unwrap().len(), 40, "{tool:?}");
        assert!(body.get("filesTruncated").is_none(), "{tool:?}: {body}");
    }
}

/// A `files` or `count` answer carrying `provenance` also carries the
/// once-per-session sentence explaining it, on the first ask of a fresh
/// session: measuring candidate budgets must not spend it before the answer
/// that is sent. Control: in `answer::respond`, measure with `build(budget,
/// true)` -> the sent answer has `provenance` but no sentence.
#[test]
fn a_non_row_answer_keeps_the_provenance_sentence() {
    for tool in [Tool::Callers, Tool::Callees, Tool::References] {
        for answer in [Answer::Files, Answer::Count] {
            let shape = Shape { pending: true, ..Shape::rows(3) };
            let (body, _) = ask(tool, &build(shape, tool), true, 200, None, Some(answer));
            assert!(body.get("provenance").is_some(), "{tool:?} {answer:?}: {body}");
            let hint = body["hint"].as_str().unwrap_or_else(|| panic!("{tool:?} {answer:?}: {body}"));
            assert!(hint.contains(super::session_hints::PROVENANCE), "{tool:?} {answer:?}: {hint}");
        }
    }
}

// --- 3. paging after a byte cut -----------------------------------------------

/// Pages cut by bytes (wide rows beside full tallies and a pending block)
/// still hand every row back exactly once across the cursor chain, each page
/// inside the ceiling. Control: in `bound_page_in_response`, drop the last
/// row of a cut page without moving its cursor -> a row is lost.
#[test]
fn paging_through_byte_cut_pages_returns_every_row_once() {
    for tool in [Tool::Callers, Tool::Callees, Tool::References, Tool::Implementations] {
        let (ex, ex_pad) = if tool == Tool::Implementations { (0, 0) } else { (50, 100) };
        let shape = Shape { rows: 300, row_pad: 300, fpad: 60, ex, ex_pad, pending: true };
        let store = build(shape, tool);
        let mut seen: Vec<String> = Vec::new();
        let mut cursor = None;
        let mut cut_pages = 0;
        for _ in 0..100 {
            let (body, bytes) = ask(tool, &store, true, 200, cursor.take(), None);
            assert!(bytes <= MAX_RESPONSE_BYTES, "{tool:?}: {bytes}");
            let page = rows(&body);
            assert!(!page.is_empty(), "{tool:?}: empty page mid-walk");
            seen.extend(page.iter().map(|row| row[tool.row_id()].as_str().unwrap().to_string()));
            if body["hasMore"] != true {
                break;
            }
            if page.len() < 200 {
                cut_pages += 1;
            }
            cursor = Some(body["nextCursor"].as_str().unwrap().to_string());
        }
        assert!(cut_pages >= 2, "{tool:?}: fixture must cut pages by bytes ({cut_pages})");
        let unique: HashSet<&String> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "{tool:?}: a row came back twice");
        // `find_references` walks the REFERENCES edges as rows too.
        let expected = if tool == Tool::References { 300 + ex } else { 300 };
        assert_eq!(seen.len(), expected, "{tool:?}: rows lost or added");
    }
}

// --- fit_budget -----------------------------------------------------------------

/// The search returns the largest budget at which the whole fits: here the
/// rest of the response is a fixed 5,000 bytes over a part that takes its
/// budget up to 30,000.
#[test]
fn fit_budget_finds_the_budget_at_which_the_whole_fits() {
    let measure = |budget: usize| {
        let part = budget.min(30_000);
        (part, part + 5_000)
    };
    let budget = pagination::fit_budget(measure);
    assert_eq!(budget, MAX_RESPONSE_BYTES - 5_000);
    assert!(measure(budget).1 <= MAX_RESPONSE_BYTES);
}

/// A part at its floor (it no longer shrinks with the budget) ends the
/// search after one more round instead of walking the budget down to 0.
/// Control: drop `part >= last_part` from `fit_budget`'s stop condition
/// (~100 rounds).
#[test]
fn fit_budget_stops_at_a_part_that_no_longer_shrinks() {
    let mut rounds = 0;
    pagination::fit_budget(|_| {
        rounds += 1;
        (1_000, MAX_RESPONSE_BYTES + 10)
    });
    assert!(rounds <= 2, "{rounds} rounds");
}
