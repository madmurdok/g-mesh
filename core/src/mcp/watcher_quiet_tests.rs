//! A quiet file watcher (`McpHost::watcher_quiet`) is named on a by-name
//! miss and on a `semanticNeighbours` page of every name-resolving tool, as
//! one sentence appended to the text the client already reads; every other
//! answer, and every answer while the host reports no quiet, is unchanged.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolResult, ContentBlock};
use rusqlite::Connection;
use serde_json::{json, Value};

use super::search_code_worker_tests::{call, connect};
use super::GMeshMcpServer;
use crate::embedding::model::EMBEDDING_DIM;
use crate::embedding::pipeline::Embedder;
use crate::embedding::EmbeddingPipeline;
use crate::indexing_status::{IndexingStatus, Phase};
use crate::mcp::host_fake::FakeHost;
use crate::mcp::McpHost;
use crate::model::NodeRecord;
use crate::storage::index_store::IndexStore;
use crate::storage::schema;
use crate::storage::vectors::{insert, register_extension};
use crate::storage::write::upsert_node;

/// A name nothing declares, no file carries and nothing imports.
const UNNAMED: &str = "NoSuchSymbolAnywhere";

/// The quiet the host reports in these tests, past any threshold.
const QUIET: Duration = Duration::from_secs(12);

/// The sentence the spec (GM-725 note, 6.1) appends for `QUIET`.
const NOTE_12S: &str =
    "No file change reported for 12 s. If you just edited the file that should define it, \
                        get_file_outline on that file reads it fresh.";

/// The five tools that resolve a `symbol_name`.
const BY_NAME_TOOLS: [&str; 5] =
    ["find_definition", "find_references", "find_callers", "find_callees", "find_implementations"];

/// `run` (with the one stored vector), `Stat` in `fs.go` (a file-name page
/// for `fs`), and `dup` in two files (an ambiguous page).
fn fixture() -> Connection {
    register_extension();
    let mut conn = Connection::open_in_memory().unwrap();
    schema::apply(&conn).unwrap();
    upsert_node(&mut conn, NodeRecord::new("run", "Function", "run", "pkg::run", "a.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("Stat", "Function", "Stat", "Stat", "fs.go", "go")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("dup_x", "Function", "dup", "x::dup", "x.rs", "rust")).unwrap();
    upsert_node(&mut conn, NodeRecord::new("dup_y", "Function", "dup", "y::dup", "y.rs", "rust")).unwrap();
    insert(&conn, "run", &vec![1.0; EMBEDDING_DIM], "v1").unwrap();
    conn
}

/// Answers every query with the fixture's vector, so `run` scores 1.0.
struct FixedEmbedder;

impl Embedder for FixedEmbedder {
    fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(vec![1.0; EMBEDDING_DIM])
    }
}

/// A server over the fixture and a [`FakeHost`] the test sets the quiet on;
/// `semantic` gives it an embedder, so a name nothing carries reaches the
/// semantic rung's page instead of the refusal.
fn server(dir: &Path, semantic: bool) -> (GMeshMcpServer, Arc<FakeHost>) {
    let root = dir.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let embedding = if semantic {
        Arc::new(EmbeddingPipeline::with_loader(
            &dir.join("model"),
            |_: &Path| Ok(Box::new(FixedEmbedder) as Box<dyn Embedder>),
            None,
        ))
    } else {
        Arc::new(EmbeddingPipeline::disabled())
    };
    let host = Arc::new(FakeHost::new(&root));
    let indexing = IndexingStatus::structural();
    indexing.set_phase(Phase::Ready);
    let server = GMeshMcpServer::new(
        Arc::new(IndexStore::new(fixture())),
        Arc::clone(&host) as Arc<dyn McpHost>,
        indexing,
        embedding,
    );
    (server, host)
}

/// The one text block of an error result; panics on any other shape.
fn only_error_text(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "expected an error result: {:?}", result.content);
    assert_eq!(result.content.len(), 1, "exactly one content block: {:?}", result.content);
    match &result.content[0] {
        ContentBlock::Text(text) => text.text.clone(),
        other => panic!("expected text content, got {other:?}"),
    }
}

fn json_body(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "expected a success result: {:?}", result.content);
    let text = &result.content[0].as_text().expect("text content").text;
    serde_json::from_str(text).expect("a JSON body")
}

/// Every by-name tool's miss, with the quiet past the threshold, reads as
/// the miss it was plus the note; the wire shape (isError, one text block)
/// is the one a miss already had.
///
/// Control: drop `note_watcher_quiet` from any one `*_off_worker` wrapper in
/// `query.rs` (e.g. `callees_off_worker` returns the bare
/// `resolve_lazily_off_worker` answer) - that tool's assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_by_name_miss_names_the_quiet_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let (server, host) = server(dir.path(), false);
    let client = connect(server).await;
    host.set_watcher_quiet(Some(QUIET));

    for tool in BY_NAME_TOOLS {
        let text = only_error_text(&call(&client, tool, json!({ "symbol_name": UNNAMED })).await);
        assert_eq!(text, format!("g-mesh: no symbol named '{UNNAMED}' found. {NOTE_12S}"), "{tool}");
    }
}

/// With no quiet to report (recent events, or no watcher), a by-name miss
/// is the refusal clients have always read, in the same shape.
///
/// Control: in `note_watcher_quiet`, treat `None` as a zero quiet instead of
/// returning the answer unchanged - every tool's miss gains a sentence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_by_name_miss_without_a_quiet_watcher_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let (server, host) = server(dir.path(), false);
    let client = connect(server).await;
    host.set_watcher_quiet(None);

    for tool in BY_NAME_TOOLS {
        let text = only_error_text(&call(&client, tool, json!({ "symbol_name": UNNAMED })).await);
        assert_eq!(text, format!("g-mesh: no symbol named '{UNNAMED}' found"), "{tool}");
    }
}

/// A `semanticNeighbours` page's explanation ends with the note; its label,
/// results and the rest of its explanation are the page a quiet-free call
/// gets.
///
/// Control: make `FileNamePage::note_if_semantic` a no-op - the explanation
/// lacks the note and the first assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_semantic_neighbours_page_names_the_quiet_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let (server, host) = server(dir.path(), true);
    let client = connect(server).await;

    for tool in ["find_definition", "find_callers"] {
        host.set_watcher_quiet(None);
        let plain = json_body(&call(&client, tool, json!({ "symbol_name": UNNAMED })).await);
        host.set_watcher_quiet(Some(QUIET));
        let noted = json_body(&call(&client, tool, json!({ "symbol_name": UNNAMED })).await);

        assert_eq!(noted["resolvedBy"], "semanticNeighbours", "{tool}: {noted}");
        let explanation = plain["explanation"].as_str().expect("an explanation");
        assert_eq!(noted["explanation"], format!("{explanation} {NOTE_12S}"), "{tool}");
        let mut rest = noted.clone();
        rest["explanation"] = plain["explanation"].clone();
        assert_eq!(rest, plain, "{tool}: only the explanation changes");
    }
}

/// With the quiet past the threshold, every answer that is neither a
/// by-name miss nor a `semanticNeighbours` page is byte-identical to the one
/// a quiet-free call gets: a resolved name, a file-name page, an ambiguous
/// page, a by-id miss, and file-anchored answers.
///
/// Controls: make `FileNamePage::note_if_semantic` append unconditionally
/// (the `fs` file-name page changes); or append the note to every
/// `Refusal::Text` in `note_watcher_quiet` (the by-id and file-anchored
/// misses change).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_other_than_a_by_name_miss_ignore_the_quiet_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let (server, host) = server(dir.path(), true);
    let client = connect(server).await;

    let calls = [
        ("find_definition", json!({ "symbol_name": "run" })),
        ("find_callers", json!({ "symbol_name": "run" })),
        ("find_definition", json!({ "symbol_name": "fs" })),
        ("find_definition", json!({ "symbol_name": "dup" })),
        ("find_references", json!({ "symbol_id": "no_such_id" })),
        ("find_definition", json!({ "file_path": "missing.rs", "position": { "line": 0, "col": 0 } })),
        ("get_file_outline", json!({ "file_path": "missing.rs" })),
    ];
    for (tool, arguments) in calls {
        host.set_watcher_quiet(None);
        let plain = call(&client, tool, arguments.clone()).await;
        host.set_watcher_quiet(Some(QUIET));
        let quiet = call(&client, tool, arguments.clone()).await;
        assert_eq!(
            format!("{:?}", quiet.content),
            format!("{:?}", plain.content),
            "{tool} {arguments}: unchanged by the quiet"
        );
        assert_eq!(quiet.is_error, plain.is_error, "{tool} {arguments}");
        assert!(
            !format!("{:?}", quiet.content).contains("No file change reported"),
            "{tool} {arguments}: {:?}",
            quiet.content
        );
    }

    // The samples reach the answers they are meant to.
    let resolved_by = |result: &CallToolResult| json_body(result)["resolvedBy"].clone();
    assert_eq!(
        resolved_by(&call(&client, "find_definition", json!({ "symbol_name": "fs" })).await),
        "fileName"
    );
    let ambiguous = json_body(&call(&client, "find_definition", json!({ "symbol_name": "dup" })).await);
    assert_eq!(ambiguous["ambiguous"], true, "{ambiguous}");
}

/// The descriptions state the freshness rule: by-name answers can lag
/// behind edits, and a file-anchored call reads the file first. The
/// tools/list byte ceiling is pinned by `tools_list_tests`.
///
/// Control: restore either description to its text before the rule was
/// added - its assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_descriptions_state_the_freshness_rule() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _host) = server(dir.path(), false);
    let client = connect(server).await;
    let tools = client.list_all_tools().await.expect("tools/list must answer");
    let description = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.name == name)
            .and_then(|tool| tool.description.as_deref().map(str::to_owned))
            .unwrap_or_else(|| panic!("{name} is listed with a description"))
    };

    let definition = description("find_definition");
    assert!(definition.contains("By name, answers can lag behind edits"), "{definition}");
    assert!(definition.contains("with file_path, the file is re-read first"), "{definition}");
    let outline = description("get_file_outline");
    assert!(outline.contains("Re-reads the file first."), "{outline}");
}
