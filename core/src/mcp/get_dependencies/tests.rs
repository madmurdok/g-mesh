use super::*;
use crate::graph::queries::{upsert_edge, upsert_node};
use crate::storage::schema;
use crate::storage::write::{self, Diff, EdgeRecord, NodeRecord};

/// What a real daemon feeds `handle`/`from_file`/`from_module`/
/// `no_file_message` in the bundled, TS-only setup: the bundled plugin's
/// own manifest declares `entry_points = ["index"]`
/// (`plugins/typescript/plugin.toml`), so this is the one list that
/// reproduces the pre-GM-273 hardcoded `index.*` behaviour exactly.
fn ts_entry_points() -> Vec<String> {
    vec!["index".to_string()]
}

/// Shadows [`super::handle`] for every test below that does not care
/// about entry points at all, or wants the bundled-TS-setup default -
/// see [`ts_entry_points`]. A test exercising a different declared set
/// (a fake Rust manifest, an empty one) calls `super::handle` directly
/// instead of this wrapper.
fn handle(conn: &Arc<Mutex<Connection>>, params: GetDependenciesParams) -> Result<CallToolResult, ErrorData> {
    super::handle(conn, &ts_entry_points(), params)
}

/// [`handle`]'s own shadow, for [`super::from_file`].
fn from_file(conn: &Connection, file_path: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    super::from_file(conn, &ts_entry_points(), file_path, shape)
}

/// [`handle`]'s own shadow, for [`super::from_module`].
fn from_module(conn: &Connection, module_id: &str, shape: &WalkShape) -> Result<CallToolResult, ErrorData> {
    super::from_module(conn, &ts_entry_points(), module_id, shape)
}

/// [`handle`]'s own shadow, for [`super::no_file_message`].
fn no_file_message(conn: &Connection, file_path: &str) -> Result<String, ErrorData> {
    super::no_file_message(conn, &ts_entry_points(), file_path)
}

fn setup() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    conn
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

fn file(path: &str) -> NodeRecord {
    NodeRecord::new(path, "File", path, path, path, "rust")
}

/// `from` imports `to`, i.e. the edge points the way the dependency does.
fn imports(conn: &mut Connection, from: &str, to: &str) {
    upsert_edge(conn, EdgeRecord::new(format!("e_{from}_{to}"), from, to, "IMPORTS", "tree-sitter", true))
        .unwrap();
}

/// a.rs -> b.rs -> c.rs, the chain both direction tests read in opposite
/// ways.
fn import_chain() -> Connection {
    let mut conn = setup();
    for path in ["a.rs", "b.rs", "c.rs"] {
        upsert_node(&mut conn, file(path)).unwrap();
    }
    imports(&mut conn, "a.rs", "b.rs");
    imports(&mut conn, "b.rs", "c.rs");
    conn
}

/// An import nothing could be linked to, stored the way the js-ts
/// extractor stores it: a `Module` node whose `filePath` is the
/// *importing* file, because that is where the specifier is written.
fn unresolved_import(importer: &str, specifier: &str) -> NodeRecord {
    NodeRecord::new(format!("mod_{specifier}"), MODULE_KIND, specifier, specifier, importer, "typescript")
}

fn anchored_at(file_path: &str, direction: Direction) -> GetDependenciesParams {
    GetDependenciesParams {
        file_path: Some(file_path.to_string()),
        module_id: None,
        direction,
        max_depth: None,
        max_fanout: None,
        resume_token: None,
    }
}

/// (id, depth) per result row, in the order the walk reported them.
fn reached(body: &serde_json::Value) -> Vec<(String, u64)> {
    body["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().to_string(), r["depth"].as_u64().unwrap()))
        .collect()
}

/// Acceptance criteria: a three-file import chain comes back whole, not
/// one hop of it - this is the only tool that walks past its own anchor.
#[test]
fn an_import_chain_comes_back_transitively_with_the_hop_count_per_node() {
    let conn = import_chain();

    let result = handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap();
    let body = json_body(&result);

    assert_eq!(reached(&body), vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]);
    assert_eq!(body["truncated"], false);
    assert!(body["truncatedBy"].is_null());
    assert_eq!(body["frontierNodes"].as_array().unwrap().len(), 0);
    assert!(body["resumeToken"].is_null());
}

/// The same chain read the other way: from its far end, `Incoming`
/// reaches the importers, and the two directions must not agree.
#[test]
fn incoming_walks_the_importers_and_outgoing_the_imports() {
    let conn = Arc::new(Mutex::new(import_chain()));

    let upstream = json_body(&handle(&conn, anchored_at("c.rs", Direction::Incoming)).unwrap());
    assert_eq!(reached(&upstream), vec![("b.rs".to_string(), 1), ("a.rs".to_string(), 2)]);

    let downstream = json_body(&handle(&conn, anchored_at("c.rs", Direction::Outgoing)).unwrap());
    assert_eq!(reached(&downstream), vec![], "nothing imports out of the end of the chain");
    assert_eq!(downstream["truncated"], false, "an empty walk is complete, not truncated");
}

/// The anchor is what the caller already named; repeating it in the
/// results would only make "how far away is this" ambiguous.
#[test]
fn the_anchor_itself_is_not_reported_as_its_own_dependency() {
    let conn = import_chain();
    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());

    let ids: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert!(!ids.contains(&"a.rs"), "the depth-0 anchor must not appear among its own dependencies: {ids:?}");
}

/// A placeholder must not borrow the importing file's path on the way
/// out: "zod lives in a.rs" is both untrue and indistinguishable from
/// a.rs's own row in the same walk.
#[test]
fn an_unresolved_import_is_reported_without_a_file_path_of_its_own() {
    let mut conn = import_chain();
    upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
    imports(&mut conn, "a.rs", "mod_zod");

    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());
    let rows = body["results"].as_array().unwrap();

    let module = rows.iter().find(|r| r["kind"] == "Module").expect("the placeholder is still a dependency");
    assert!(module["filePath"].is_null(), "a module placeholder has no file of its own: {module}");
    assert_eq!(module["qualifiedName"], "zod", "the specifier is all there is left to act on");

    let files: Vec<&str> =
        rows.iter().filter(|r| r["kind"] == "File").map(|r| r["filePath"].as_str().unwrap()).collect();
    assert_eq!(files, vec!["b.rs", "c.rs"], "real files are still addressed by their own path");
}

/// A `File`-kind row's `qualifiedName` is byte-identical to its own
/// `filePath` by construction (see `pagination::FILE_KIND`'s doc comment),
/// so it must be omitted from the wire JSON entirely rather than repeat
/// the same path string twice. A `Module` placeholder has no `filePath`
/// of its own, so it keeps `qualifiedName` as the only field carrying the
/// specifier - the mirror image of the previous test.
#[test]
fn a_file_kind_row_omits_qualified_name_a_module_row_keeps_it() {
    let mut conn = import_chain();
    upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
    imports(&mut conn, "a.rs", "mod_zod");

    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());
    let rows = body["results"].as_array().unwrap();

    let files: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "File").collect();
    assert!(!files.is_empty());
    for file in files {
        assert!(
            file.get("qualifiedName").is_none(),
            "a File row must not repeat its own filePath as qualifiedName: {file}"
        );
    }

    let module = rows.iter().find(|r| r["kind"] == "Module").expect("the placeholder is still a dependency");
    assert_eq!(module["qualifiedName"], "zod", "a Module row has no filePath, so qualifiedName must stay");
}

/// `name` never carries information `qualifiedName` doesn't already have
/// (at worst a shorter, less unique view of the same symbol) - dropped
/// entirely from every row, real file and unresolved-module placeholder
/// alike.
#[test]
fn no_row_carries_a_name_field() {
    let mut conn = import_chain();
    upsert_node(&mut conn, unresolved_import("a.rs", "zod")).unwrap();
    imports(&mut conn, "a.rs", "mod_zod");

    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());
    let rows = body["results"].as_array().unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        assert!(row.get("name").is_none(), "the name field must never be present on any row: {row}");
    }
}

#[test]
fn only_import_edges_are_walked() {
    let mut conn = import_chain();
    upsert_node(&mut conn, file("d.rs")).unwrap();
    upsert_edge(&mut conn, EdgeRecord::new("e_call", "a.rs", "d.rs", "CALLS", "tree-sitter", true)).unwrap();

    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("a.rs", Direction::Outgoing)).unwrap());

    let ids: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["b.rs", "c.rs"], "a CALLS edge is not a dependency: {ids:?}");
}

#[test]
fn a_module_id_anchors_the_walk_without_a_path_lookup() {
    let conn = import_chain();
    let params = GetDependenciesParams {
        file_path: None,
        module_id: Some("a.rs".to_string()),
        direction: Direction::Outgoing,
        max_depth: None,
        max_fanout: None,
        resume_token: None,
    };

    let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());
    assert_eq!(reached(&body), vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)]);
}

#[test]
fn an_unknown_anchor_is_a_tool_level_error_rather_than_an_empty_walk() {
    let conn = Arc::new(Mutex::new(import_chain()));

    let by_path = handle(&conn, anchored_at("does/not/exist.rs", Direction::Outgoing)).unwrap();
    assert!(error_text(&by_path).contains("does/not/exist.rs"));

    let by_module = GetDependenciesParams {
        file_path: None,
        module_id: Some("no_such_module".to_string()),
        direction: Direction::Outgoing,
        max_depth: None,
        max_fanout: None,
        resume_token: None,
    };
    assert!(error_text(&handle(&conn, by_module).unwrap()).contains("no_such_module"));
}

#[test]
fn every_bad_anchor_combination_is_its_own_tool_level_error() {
    let conn = Arc::new(Mutex::new(import_chain()));
    let base = || GetDependenciesParams {
        file_path: None,
        module_id: None,
        direction: Direction::Outgoing,
        max_depth: None,
        max_fanout: None,
        resume_token: None,
    };

    let neither = handle(&conn, base()).unwrap();
    assert!(error_text(&neither).contains("file_path"));

    let both = GetDependenciesParams {
        file_path: Some("a.rs".to_string()),
        module_id: Some("a.rs".to_string()),
        ..base()
    };
    assert!(error_text(&handle(&conn, both).unwrap()).contains("not both"));

    let token_and_anchor = GetDependenciesParams {
        file_path: Some("a.rs".to_string()),
        resume_token: Some("whatever".to_string()),
        ..base()
    };
    assert!(error_text(&handle(&conn, token_and_anchor).unwrap()).contains("resume_token"));
}

/// Truncation contract, cause one: the walk stopped at the depth limit,
/// so the caller gets the boundary to re-root on and nothing else.
#[test]
fn a_depth_cut_reports_max_depth_and_hands_back_only_the_frontier() {
    let mut conn = setup();
    for path in ["a.rs", "b.rs", "c.rs", "d.rs"] {
        upsert_node(&mut conn, file(path)).unwrap();
    }
    imports(&mut conn, "a.rs", "b.rs");
    imports(&mut conn, "b.rs", "c.rs");
    imports(&mut conn, "c.rs", "d.rs");

    let params = GetDependenciesParams { max_depth: Some(1), ..anchored_at("a.rs", Direction::Outgoing) };
    let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());

    assert_eq!(reached(&body), vec![("b.rs".to_string(), 1)]);
    assert_eq!(body["truncated"], true);
    assert_eq!(body["truncatedBy"], "maxDepth");
    assert_eq!(body["frontierNodes"], serde_json::json!(["b.rs"]), "the level to re-root the same call on");
    assert!(body["resumeToken"].is_null(), "a depth cut is re-rooted, not resumed");
}

/// Cause two: a node had more imports than the fan-out cap. Deliberately
/// no extra field - the caller re-queries that node with the single-hop
/// tools' cursor pagination, which already exists.
#[test]
fn a_fanout_cut_reports_max_fanout_and_hands_back_no_continuation_field() {
    let mut conn = setup();
    upsert_node(&mut conn, file("a.rs")).unwrap();
    for path in ["b.rs", "c.rs", "d.rs"] {
        upsert_node(&mut conn, file(path)).unwrap();
        imports(&mut conn, "a.rs", path);
    }

    let params = GetDependenciesParams { max_fanout: Some(1), ..anchored_at("a.rs", Direction::Outgoing) };
    let body = json_body(&handle(&Arc::new(Mutex::new(conn)), params).unwrap());

    assert_eq!(body["results"].as_array().unwrap().len(), 1, "one of the three imports, and a warning");
    assert_eq!(body["truncated"], true);
    assert_eq!(body["truncatedBy"], "maxFanout");
    assert_eq!(
        body["frontierNodes"].as_array().unwrap().len(),
        0,
        "a fanout cut is paginated, not re-rooted"
    );
    assert!(body["resumeToken"].is_null());
}

/// Cause three, and the only one with state to carry: the internal
/// budget - not a caller-facing limit - stops the walk mid-way, and the
/// token it hands back continues it exactly where it left off.
///
/// `DEFAULT_EXPLORATION_BUDGET` rows (5000) of `DependencyNode` JSON is
/// nowhere near `pagination::MAX_RESPONSE_BYTES` (20,000 bytes), so a
/// walk wide enough to hit the exploration budget always earns its own,
/// stricter `responseSize` cut before `explorationBudget` is ever visible
/// on the wire - see `bound_walk`'s doc comment. That row-count-scale
/// case is exercised directly at the `traversal` layer instead
/// (`graph::traversal::tests::exploration_budget_caps_visited_rows_...`,
/// `..._a_resume_chain_covers_the_whole_walk_exactly_once`); what this
/// tool-level test proves is the same "no continuation is dropped or
/// double-counted" property one layer up, at the response-size scale
/// (`bound_walk`'s `prior_visited`/`prior_walked` accumulation) that
/// callers actually see.
#[test]
fn a_response_size_cut_is_continued_by_its_token_and_the_chain_covers_everything_once() {
    // Comfortably past what one response can return, comfortably short
    // of the exploration budget - so nothing but the byte cap can be
    // what's cutting each call in this chain.
    let wide = 600;
    let mut conn = setup();
    let mut diff = Diff { upsert_nodes: vec![file("a.rs")], ..Default::default() };
    for i in 0..wide {
        let path = format!("dep{i:05}.rs");
        diff.upsert_edges.push(EdgeRecord::new(
            format!("e{i:05}"),
            "a.rs",
            &path,
            "IMPORTS",
            "tree-sitter",
            true,
        ));
        diff.upsert_nodes.push(file(&path));
    }
    write::apply_diff(&mut conn, &diff).unwrap();
    let conn = Arc::new(Mutex::new(conn));

    let params =
        GetDependenciesParams { max_fanout: Some(10_000), ..anchored_at("a.rs", Direction::Outgoing) };
    let first = json_body(&handle(&conn, params).unwrap());

    let first_len = first["results"].as_array().unwrap().len();
    assert!(
        first_len > 0 && first_len < wide,
        "one response must not hold all {wide} dependencies: {first_len}"
    );
    assert_eq!(first["truncated"], true);
    assert_eq!(first["truncatedBy"], "responseSize");
    assert_eq!(first["frontierNodes"].as_array().unwrap().len(), 0, "a size cut is resumed, not re-rooted");

    let mut all: Vec<String> =
        first["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()).collect();
    let mut token = first["resumeToken"].as_str().map(str::to_string);
    let mut calls = 1;

    while let Some(t) = token {
        let resumed = GetDependenciesParams {
            file_path: None,
            module_id: None,
            // Ignored on a continuation: the token carries the walk's shape.
            direction: Direction::Incoming,
            max_depth: None,
            max_fanout: None,
            resume_token: Some(t),
        };
        let body = json_body(&handle(&conn, resumed).unwrap());
        calls += 1;
        all.extend(body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap().to_string()));
        token = body["resumeToken"].as_str().map(str::to_string);
        assert!(calls < 50, "the chain must converge, not re-explore itself forever: {calls} calls so far");
    }

    assert!(
        calls > 2,
        "a page far smaller than {wide} deps must take more than one resume: only {calls} calls"
    );

    let mut deduped = all.clone();
    deduped.sort();
    deduped.dedup();
    assert_eq!(deduped.len(), all.len(), "no dependency may be returned twice across the chain");
    assert_eq!(deduped.len(), wide, "the whole chain's union must be every dependency, exactly once");
}

/// Reproduces the shape of the two real `get_dependencies` failures in
/// g-mesh-bench's v0.4.0 outlier findings (a shared module's `Incoming`
/// fan-in producing a 115,863-character response the MCP client's
/// transport rejected outright) as a synthetic fixture: a single file
/// many other files import, none of it anywhere near the exploration
/// budget or a caller-set `max_fanout`, but still too much JSON for one
/// response.
#[test]
fn a_wide_fan_in_too_big_for_one_response_truncates_with_a_resume_token_instead_of_erroring() {
    let wide = 400;
    let mut conn = setup();
    let core = "packages/core/src/index.ts";
    let mut diff = Diff { upsert_nodes: vec![file(core)], ..Default::default() };
    for i in 0..wide {
        let path = format!("packages/consumer{i:05}/src/index.ts");
        diff.upsert_edges.push(EdgeRecord::new(
            format!("e{i:05}"),
            &path,
            core,
            "IMPORTS",
            "tree-sitter",
            true,
        ));
        diff.upsert_nodes.push(file(&path));
    }
    write::apply_diff(&mut conn, &diff).unwrap();
    let conn = Arc::new(Mutex::new(conn));

    let params = GetDependenciesParams { max_fanout: Some(10_000), ..anchored_at(core, Direction::Incoming) };
    let body = json_body(&handle(&conn, params).unwrap());

    let results = body["results"].as_array().unwrap();
    assert!(!results.is_empty(), "at least one row must always come back, even under an oversized level");
    assert!(results.len() < wide, "the full {wide}-wide fan-in must not fit in one response");
    assert_eq!(body["truncated"], true);
    assert_eq!(body["truncatedBy"], "responseSize");
    let raw_len = serde_json::to_vec(results).unwrap().len();
    assert!(
        raw_len <= pagination::MAX_RESPONSE_BYTES,
        "the truncated page itself must respect the budget: {raw_len}"
    );

    let token = body["resumeToken"].as_str().expect("a size cut must carry a resume token").to_string();
    let resumed = GetDependenciesParams {
        file_path: None,
        module_id: None,
        direction: Direction::Outgoing,
        max_depth: None,
        max_fanout: None,
        resume_token: Some(token),
    };
    let second = json_body(&handle(&conn, resumed).unwrap());
    assert!(
        !second["results"].as_array().unwrap().is_empty(),
        "resuming must make forward progress on what the first call dropped"
    );
}

/// Problem 2's fix: omitting `max_depth` must stop at this tool's own,
/// stricter default - not fall through to the walk engine's generic one
/// (`traversal::DEFAULT_MAX_DEPTH`, 5). A caller that passes `max_depth`
/// explicitly must still get exactly that depth, unaffected.
#[test]
fn omitting_max_depth_uses_this_tools_own_default_not_the_walk_engines() {
    let mut conn = setup();
    let chain = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs"];
    for path in chain {
        upsert_node(&mut conn, file(path)).unwrap();
    }
    for pair in chain.windows(2) {
        imports(&mut conn, pair[0], pair[1]);
    }
    let conn = Arc::new(Mutex::new(conn));

    let defaulted = json_body(&handle(&conn, anchored_at("a.rs", Direction::Outgoing)).unwrap());
    assert_eq!(
        reached(&defaulted),
        vec![("b.rs".to_string(), 1), ("c.rs".to_string(), 2)],
        "omitting max_depth must stop at DEFAULT_MAX_DEPTH (2), not the walk engine's default (5)"
    );
    assert_eq!(defaulted["truncated"], true);
    assert_eq!(defaulted["truncatedBy"], "maxDepth");
    assert_eq!(defaulted["frontierNodes"], serde_json::json!(["c.rs"]));

    let explicit = GetDependenciesParams { max_depth: Some(4), ..anchored_at("a.rs", Direction::Outgoing) };
    let body = json_body(&handle(&conn, explicit).unwrap());
    assert_eq!(
        reached(&body),
        vec![
            ("b.rs".to_string(), 1),
            ("c.rs".to_string(), 2),
            ("d.rs".to_string(), 3),
            ("e.rs".to_string(), 4)
        ],
        "an explicit max_depth must be honored exactly, unaffected by this tool's own default"
    );
    assert_eq!(body["truncated"], false);
}

/// The caller asked about a package or a directory, which is what the
/// prompt they are answering names. A bare refusal sends them hunting with
/// Glob for the entry point - a round trip, and the recorded trace for
/// `ex-deps-package-math-incoming` is exactly that hunt.
#[test]
fn a_directory_prefix_is_told_which_indexed_files_sit_under_it() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/angle.ts")).unwrap();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

    let message = no_file_message(&conn, "packages/math").unwrap();

    assert!(message.contains("packages/math/src/index.ts"), "{message}");
    // Entry point first: it is what a package specifier resolves to, and
    // what the caller is going to ask about next.
    let idx = message.find("packages/math/src/index.ts").unwrap();
    let other = message.find("packages/math/src/angle.ts").unwrap();
    assert!(idx < other, "the entry point must lead: {message}");
}

/// A workspace package name is not a path at all, so the only handle is
/// its last segment matching a directory - offered as a suggestion, since
/// a directory of that name does not establish the package lives there.
#[test]
fn a_package_name_is_offered_the_directory_that_shares_its_last_segment() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

    let message = no_file_message(&conn, "@excalidraw/math").unwrap();

    assert!(message.contains("packages/math/src/index.ts"), "{message}");
    assert!(message.contains("If 'math' is"), "must read as a suggestion: {message}");
}

/// A path matching nothing keeps the short answer. The explanation is only
/// worth its length where there is something to explain.
#[test]
fn a_path_under_which_nothing_is_indexed_keeps_the_terse_answer() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();

    let message = no_file_message(&conn, "packages/nowhere").unwrap();

    assert_eq!(message, "g-mesh: no file 'packages/nowhere' found in the index");
}

/// Callers put a path in `module_id` - the field reads as "the module's
/// name" and is documented as the alternative to `file_path`. Every
/// recorded run of the benchmark task that asks about a package did it,
/// and paid a refusal plus a blind Glob for the label.
#[test]
fn a_path_passed_as_a_module_id_is_answered_rather_than_refused() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
    upsert_node(&mut conn, file("packages/excalidraw/viewport.ts")).unwrap();
    imports(&mut conn, "packages/excalidraw/viewport.ts", "packages/math/src/index.ts");

    let result = from_module(
        &conn,
        "packages/math/src/index.ts",
        &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
    )
    .unwrap();

    let body = json_body(&result);
    assert_eq!(body["results"][0]["filePath"], "packages/excalidraw/viewport.ts");
}

/// GM-259, the measured case. `ex-deps-package-math-incoming` was the only
/// registry task where the g-mesh arm made zero native calls: two of five
/// repetitions grepped the specifier exactly as the grep-only baseline
/// did, and two more spent a `Glob` turn finding the path this resolves.
#[test]
fn a_package_specifier_with_one_entry_point_is_answered_not_refused() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
    upsert_node(&mut conn, file("packages/math/src/point.ts")).unwrap();
    upsert_node(&mut conn, file("packages/excalidraw/viewport.ts")).unwrap();
    imports(&mut conn, "packages/excalidraw/viewport.ts", "packages/math/src/index.ts");

    let result = from_file(
        &conn,
        "@excalidraw/math",
        &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
    )
    .unwrap();

    let body = json_body(&result);
    assert_eq!(body["results"][0]["filePath"], "packages/excalidraw/viewport.ts");
    assert_eq!(
        body["resolvedFrom"]["requested"], "@excalidraw/math",
        "the substitution has to be visible - the tool answered a question adjacent to the one asked",
    );
    assert_eq!(body["resolvedFrom"]["filePath"], "packages/math/src/index.ts");
}

/// The directory-prefix form, which is the stronger of the two inferences:
/// the caller named a real path, it just is not a file.
#[test]
fn a_directory_with_one_entry_point_is_answered_too() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/index.ts")).unwrap();
    upsert_node(&mut conn, file("packages/math/point.ts")).unwrap();
    upsert_node(&mut conn, file("app/viewport.ts")).unwrap();
    imports(&mut conn, "app/viewport.ts", "packages/math/index.ts");

    let body = json_body(
        &from_file(
            &conn,
            "packages/math",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert_eq!(body["results"][0]["filePath"], "app/viewport.ts");
    assert_eq!(body["resolvedFrom"]["filePath"], "packages/math/index.ts");
}

/// GM-273's acceptance case at the tool level: a fake manifest declaring
/// `entry_points = ["mod.rs"]` - Rust's own convention, not TypeScript's
/// `"index"` - must resolve a directory lookup to `mod.rs` the same way
/// `a_directory_with_one_entry_point_is_answered_too` resolves one to
/// `index.ts`. Calls `super::from_file` directly (not the `ts_entry_points`
/// shadow above) precisely because this is the one test that must NOT get
/// the bundled-TS default.
#[test]
fn a_directory_with_one_declared_rust_entry_point_is_answered_too() {
    let mut conn = setup();
    upsert_node(&mut conn, file("crates/math/mod.rs")).unwrap();
    upsert_node(&mut conn, file("crates/math/point.rs")).unwrap();
    // Shorter than "crates/math/mod.rs" - if entry-point rank did not
    // decide the order, `LENGTH(filePath)` would put this one first
    // instead, and the substitution below would not happen at all.
    upsert_node(&mut conn, file("crates/math/x.rs")).unwrap();
    upsert_node(&mut conn, file("app/main.rs")).unwrap();
    imports(&mut conn, "app/main.rs", "crates/math/mod.rs");

    let body = json_body(
        &super::from_file(
            &conn,
            &["mod.rs".to_string()],
            "crates/math",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert_eq!(body["results"][0]["filePath"], "app/main.rs");
    assert_eq!(
        body["resolvedFrom"]["filePath"], "crates/math/mod.rs",
        "mod.rs must be the file the walk actually started from: {body}"
    );
}

/// A directory declaring both of a Rust crate root's two conventional
/// entry points (`mod.rs` and `lib.rs`) is exactly the "more than one
/// entry point" case `entry_point_for`'s doc comment calls out by name -
/// still refused, not guessed at, the same rule
/// `two_entry_points_still_refuse_and_list_the_candidates` proves for TS.
#[test]
fn two_declared_rust_entry_points_in_one_directory_still_refuse() {
    let mut conn = setup();
    upsert_node(&mut conn, file("crates/math/mod.rs")).unwrap();
    upsert_node(&mut conn, file("crates/math/lib.rs")).unwrap();

    let message = error_text(
        &super::from_file(
            &conn,
            &["mod.rs".to_string(), "lib.rs".to_string()],
            "crates/math",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert!(message.contains("crates/math/mod.rs"), "the candidates are still named: {message}");
    assert!(message.contains("crates/math/lib.rs"), "both of them: {message}");
}

/// Two entry points is the case where answering would be worse than
/// refusing: the walk would succeed and describe the wrong file. The old
/// error, which lists the candidates, is the right outcome.
#[test]
fn two_entry_points_still_refuse_and_list_the_candidates() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/index.ts")).unwrap();
    upsert_node(&mut conn, file("packages/math/sub/index.ts")).unwrap();

    let message = error_text(
        &from_file(
            &conn,
            "packages/math",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert!(message.contains("packages/math/index.ts"), "the candidates are still named: {message}");
    assert!(message.contains("packages/math/sub/index.ts"), "both of them: {message}");
}

/// No entry point at all - a directory of ordinary modules. Picking the
/// shortest path would be a guess with nothing behind it.
#[test]
fn a_directory_without_an_entry_point_is_not_guessed_at() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/point.ts")).unwrap();
    upsert_node(&mut conn, file("packages/math/vector.ts")).unwrap();

    let message = error_text(
        &from_file(
            &conn,
            "packages/math",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert!(message.contains("no file 'packages/math' found"), "{message}");
}

/// An ordinary, exact anchor must stay exactly as it was - including
/// paying no bytes for a field about a substitution that did not happen.
#[test]
fn an_exact_file_anchor_reports_no_substitution() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
    upsert_node(&mut conn, file("app/viewport.ts")).unwrap();
    imports(&mut conn, "app/viewport.ts", "packages/math/src/index.ts");

    let result = from_file(
        &conn,
        "packages/math/src/index.ts",
        &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
    )
    .unwrap();
    let raw = json_body(&result).to_string();

    assert_eq!(json_body(&result)["results"][0]["filePath"], "app/viewport.ts");
    assert!(!raw.contains("resolvedFrom"), "no substitution, no field: {raw}");
}

// -----------------------------------------------------------------
// Containers (GM-267): `File -IMPORTS-> container` edges, walked and
// anchored on. Built directly at the storage layer - `graph::imports`'s
// own tests (`graph::imports::tests`) cover the *linking* of a
// container-scoped placeholder onto one of these edges; this module
// tests the walk and the anchor resolution once the edge exists, exactly
// the split the file-import tests above already follow (`imports` builds
// a resolved edge directly rather than going through the linker).
// -----------------------------------------------------------------

fn container_member(id: &str, language: &str, key: &str) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", id, id, format!("{key}/{id}.x"), language);
    node.container = Some(key.to_string());
    node
}

/// Materializes container `key` (in `language`) with one member - the
/// minimal fixture `graph::containers::attach` needs - and returns the
/// container's own node id.
fn materialize_container(conn: &mut Connection, language: &str, key: &str) -> String {
    upsert_node(conn, container_member(&format!("member:{language}:{key}"), language, key)).unwrap();
    crate::graph::containers::container_id(language, key)
}

/// `from` (a File) imports the container at `container_node_id`,
/// directly - the walk-time shape `graph::imports` produces after
/// linking a container-scoped placeholder, built without the linker for
/// the same reason [`imports`] builds a file-to-file edge directly.
fn imports_container(conn: &mut Connection, from: &str, container_node_id: &str) {
    upsert_edge(
        conn,
        EdgeRecord::new(
            format!("e_{from}_{container_node_id}"),
            from,
            container_node_id,
            "IMPORTS",
            "tree-sitter",
            true,
        ),
    )
    .unwrap();
}

/// Acceptance: "Outgoing from a file lists containers as well as files."
/// Decision 5's row shape, exercised end to end: a container row's
/// `qualifiedName` carries its key (`ensure_container` writes the key as
/// both `name` and `qualifiedName`), and its `filePath` is `null` rather
/// than a fabricated path - the same shape an unresolved import
/// placeholder's row already has (`an_unresolved_import_is_reported_
/// without_a_file_path_of_its_own` above), at zero extra bytes: no new
/// field, because `DependencyNode::from` already branches on `kind !=
/// MODULE_KIND` for `file_path` and `kind == FILE_KIND` for
/// `qualified_name`, and a container node's stored `kind` is `"Module"`
/// (`graph::containers::ensure_container`) - the same branch a
/// placeholder already took.
#[test]
fn outgoing_from_a_file_lists_a_container_alongside_files() {
    let mut conn = setup();
    upsert_node(&mut conn, file("main.go")).unwrap();
    upsert_node(&mut conn, file("other.go")).unwrap();
    imports(&mut conn, "main.go", "other.go");
    let container_id = materialize_container(&mut conn, "go", "github.com/x/pkg");
    imports_container(&mut conn, "main.go", &container_id);

    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("main.go", Direction::Outgoing)).unwrap());
    let rows = body["results"].as_array().unwrap();

    let container_row =
        rows.iter().find(|r| r["id"] == container_id).expect("the container must be a result row");
    assert_eq!(container_row["kind"], "Module");
    assert_eq!(
        container_row["qualifiedName"], "github.com/x/pkg",
        "decision 5: a container row names its key, not a path"
    );
    assert!(
        container_row["filePath"].is_null(),
        "decision 5: no fabricated filePath for a node with none: {container_row}"
    );

    let files: Vec<&str> =
        rows.iter().filter(|r| r["kind"] == "File").map(|r| r["filePath"].as_str().unwrap()).collect();
    assert_eq!(files, vec!["other.go"], "an ordinary file dependency is unaffected");
}

/// Acceptance: "Incoming get_dependencies on a container returns
/// importing files" - anchoring directly on an exact container key
/// (decision 4), which is not a substitution and so carries no
/// `resolvedFrom` (unlike the miss-path/`entry_point_for` case exercised
/// by `a_package_specifier_with_one_entry_point_is_answered_not_refused`
/// above).
#[test]
fn incoming_on_a_container_key_returns_the_importing_files() {
    let mut conn = setup();
    upsert_node(&mut conn, file("a.go")).unwrap();
    upsert_node(&mut conn, file("b.go")).unwrap();
    let container_id = materialize_container(&mut conn, "go", "github.com/x/pkg");
    imports_container(&mut conn, "a.go", &container_id);
    imports_container(&mut conn, "b.go", &container_id);

    let result = from_file(
        &conn,
        "github.com/x/pkg",
        &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
    )
    .unwrap();
    let body = json_body(&result);

    let mut files: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
    files.sort_unstable();
    assert_eq!(files, vec!["a.go", "b.go"]);
    assert!(
        !body.to_string().contains("resolvedFrom"),
        "an exact container key is a direct anchor, not a substitution (decision 4): {body}"
    );
}

/// Decision 4's refusal case: a key that names a container in more than
/// one language is a real ambiguity (`containers.key` is only unique
/// *within* a language), refused with the candidates named rather than
/// guessed at - the same stance `two_entry_points_still_refuse_and_list_
/// the_candidates` already takes for two file candidates.
#[test]
fn a_container_key_ambiguous_across_languages_is_refused_with_the_languages_named() {
    let mut conn = setup();
    materialize_container(&mut conn, "go", "shared");
    materialize_container(&mut conn, "rust", "shared");

    let message = error_text(
        &from_file(
            &conn,
            "shared",
            &WalkShape { direction: Direction::Incoming, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert!(message.contains("go"), "{message}");
    assert!(message.contains("rust"), "{message}");
}

/// Decision 6: two containers with the storage-level `filePath = ''` in
/// common must not collapse into a single result row - a walk-level
/// version of `graph::imports::tests::two_containers_imported_by_the_
/// same_file_both_link_independently`, at the layer (`ReachedNode`/
/// `DependencyNode`) where a `filePath`-keyed dedup would actually bite
/// if one existed. `traversal::traverse` dedups by node id
/// (`seen_nodes: HashSet<String>` keyed on `node.id`), never by
/// `filePath`, so this passed without any code change - it is coverage
/// for that fact, not a fix.
#[test]
fn two_containers_imported_by_the_same_file_are_two_separate_rows() {
    let mut conn = setup();
    upsert_node(&mut conn, file("main.go")).unwrap();
    let a = materialize_container(&mut conn, "go", "pkg/a");
    let b = materialize_container(&mut conn, "go", "pkg/b");
    imports_container(&mut conn, "main.go", &a);
    imports_container(&mut conn, "main.go", &b);

    let body =
        json_body(&handle(&Arc::new(Mutex::new(conn)), anchored_at("main.go", Direction::Outgoing)).unwrap());
    let ids: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["id"].as_str().unwrap()).collect();

    assert_eq!(ids.len(), 2, "two distinct containers must not collapse into one row: {ids:?}");
    assert!(ids.contains(&a.as_str()), "{ids:?}");
    assert!(ids.contains(&b.as_str()), "{ids:?}");
}

// -----------------------------------------------------------------
// GM-356: `Incoming` anchored on a file of a module-graph language.
//
// Every fixture below is the shape a real plugin emits, read off the
// three indexed repositories the diagnosis used: declarations carrying
// `container`, the `File -IMPORTS-> container` edge, and - for Python
// and Rust - the `Module` declarations that make "which container does
// this file define" a question with a wrong answer available.
// -----------------------------------------------------------------

/// A `File` node with a real extent. The default [`file`] helper leaves
/// every position at 0, which would make *every* member look like it
/// spans the whole file - exactly the distinction
/// `containers::defining_containers` turns on - so a fixture about that
/// distinction has to state the extent it means.
fn source_file(path: &str, language: &str, end_line: i64) -> NodeRecord {
    let mut node = NodeRecord::new(path, "File", path, path, path, language);
    node.end_line = end_line;
    node
}

/// An ordinary declaration inside `file_path`, belonging to container
/// `key` - a Go func, a Rust item, a Python class. Spans a few lines
/// somewhere inside the file, never all of it.
fn member_in(id: &str, language: &str, key: &str, parent: Option<&str>, file_path: &str) -> NodeRecord {
    let mut node = NodeRecord::new(id, "Function", id, id, file_path, language);
    node.container = Some(key.to_string());
    node.container_parent = parent.map(str::to_string);
    node.start_line = 5;
    node.end_line = 7;
    node
}

/// The `Module` declaration a Python file gets for *itself*: it spans the
/// whole file, and its container is the package the file sits in - the
/// parent, not the module the file defines. Counting it would offer
/// `requests` as a candidate for every file in `requests/`.
fn whole_file_module(id: &str, language: &str, package: &str, file_path: &str, end_line: i64) -> NodeRecord {
    let mut node = NodeRecord::new(id, MODULE_KIND, id, id, file_path, language);
    node.native_kind = Some("module".to_string());
    node.container = Some(package.to_string());
    node.end_line = end_line;
    node
}

/// The `Module` declaration a Rust `mod sinks { .. }` gets: nested inside
/// the file, so it evidences the container it is declared in rather than
/// standing for the file.
fn nested_module(
    id: &str,
    language: &str,
    declared_in: &str,
    file_path: &str,
    (start_line, end_line): (i64, i64),
) -> NodeRecord {
    let mut node = NodeRecord::new(id, MODULE_KIND, id, id, file_path, language);
    node.native_kind = Some("module".to_string());
    node.container = Some(declared_in.to_string());
    node.start_line = start_line;
    node.end_line = end_line;
    node
}

fn incoming(max_depth: u32) -> WalkShape {
    WalkShape { direction: Direction::Incoming, max_depth: Some(max_depth), max_fanout: Some(50) }
}

/// The defect itself, on the Python shape that was measured:
/// `get_dependencies("src/requests/adapters.py", Incoming)` returned
/// `results: []` with `truncated: false` while `sessions.py` held a
/// `from .adapters import HTTPAdapter`. The importers arrive at the
/// container, so the walk has to start there - and say that it did.
#[test]
fn incoming_from_a_python_file_walks_the_module_that_file_defines() {
    let mut conn = setup();
    upsert_node(&mut conn, source_file("src/requests/adapters.py", "python", 748)).unwrap();
    upsert_node(&mut conn, source_file("src/requests/sessions.py", "python", 800)).unwrap();
    upsert_node(
        &mut conn,
        member_in("HTTPAdapter", "python", "requests.adapters", Some("requests"), "src/requests/adapters.py"),
    )
    .unwrap();
    // The file's own module node, a member of the *package*.
    upsert_node(
        &mut conn,
        whole_file_module("adapters", "python", "requests", "src/requests/adapters.py", 748),
    )
    .unwrap();
    upsert_node(&mut conn, member_in("Session", "python", "requests", None, "src/requests/sessions.py"))
        .unwrap();
    let adapters = crate::graph::containers::container_id("python", "requests.adapters");
    imports_container(&mut conn, "src/requests/sessions.py", &adapters);

    let body = json_body(&from_file(&conn, "src/requests/adapters.py", &incoming(1)).unwrap());

    let importers: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
    assert_eq!(
        importers,
        vec!["src/requests/sessions.py"],
        "the importer of requests.adapters is the answer to 'what imports adapters.py': {body}"
    );
    assert_eq!(body["resolvedFrom"]["requested"], "src/requests/adapters.py");
    assert_eq!(
        body["resolvedFrom"]["qualifiedName"], "requests.adapters",
        "the substitution names the anchor that would have worked: {body}"
    );
    assert!(
        body["resolvedFrom"]["filePath"].is_null(),
        "a container substitution landed on a container, not a file: {body}"
    );
}

/// Go's shape: one package per directory, no nesting, and the file that
/// GMB-163 measured returning zero - `render/render.go` against three
/// real importers.
#[test]
fn incoming_from_a_go_file_walks_the_package_that_file_defines() {
    let mut conn = setup();
    const PKG: &str = "github.com/gin-gonic/gin/render";
    upsert_node(&mut conn, source_file("render/render.go", "go", 60)).unwrap();
    upsert_node(&mut conn, source_file("context.go", "go", 900)).unwrap();
    upsert_node(&mut conn, member_in("Render", "go", PKG, None, "render/render.go")).unwrap();
    upsert_node(&mut conn, member_in("Context", "go", "github.com/gin-gonic/gin", None, "context.go"))
        .unwrap();
    let render = crate::graph::containers::container_id("go", PKG);
    imports_container(&mut conn, "context.go", &render);

    let body = json_body(&from_file(&conn, "render/render.go", &incoming(1)).unwrap());

    let importers: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
    assert_eq!(importers, vec!["context.go"], "{body}");
    assert_eq!(body["resolvedFrom"]["qualifiedName"], PKG, "{body}");
}

/// Rust's shape, and the half of the rule Go and Python never exercise: a
/// `mod sinks { .. }` written inside `sink.rs` puts members of
/// `grep_searcher::sink::sinks` in that file too. The file still defines
/// `grep_searcher::sink`; the nested module is something it *contains*.
/// Anchoring on the descendant would answer about a module nobody
/// imports.
#[test]
fn a_module_nested_inside_a_rust_file_is_not_the_module_that_file_defines() {
    let mut conn = setup();
    const SINK: &str = "grep_searcher::sink";
    const SINKS: &str = "grep_searcher::sink::sinks";
    upsert_node(&mut conn, source_file("crates/searcher/src/sink.rs", "rust", 663)).unwrap();
    upsert_node(&mut conn, source_file("crates/printer/src/standard.rs", "rust", 400)).unwrap();
    upsert_node(
        &mut conn,
        member_in("Sink", "rust", SINK, Some("grep_searcher"), "crates/searcher/src/sink.rs"),
    )
    .unwrap();
    upsert_node(&mut conn, nested_module("sinks", "rust", SINK, "crates/searcher/src/sink.rs", (516, 662)))
        .unwrap();
    upsert_node(&mut conn, member_in("UTF8", "rust", SINKS, Some(SINK), "crates/searcher/src/sink.rs"))
        .unwrap();
    upsert_node(
        &mut conn,
        member_in("Standard", "rust", "grep_printer", None, "crates/printer/src/standard.rs"),
    )
    .unwrap();
    let sink = crate::graph::containers::container_id("rust", SINK);
    imports_container(&mut conn, "crates/printer/src/standard.rs", &sink);

    let body = json_body(&from_file(&conn, "crates/searcher/src/sink.rs", &incoming(1)).unwrap());

    let importers: Vec<&str> =
        body["results"].as_array().unwrap().iter().map(|r| r["filePath"].as_str().unwrap()).collect();
    assert_eq!(importers, vec!["crates/printer/src/standard.rs"], "{body}");
    assert_eq!(
        body["resolvedFrom"]["qualifiedName"], SINK,
        "the outermost container the file declares, not the one declared inside it: {body}"
    );
}

/// The TypeScript control, and the reason the guard is "this file has no
/// importers of its own" rather than a list of languages: a TS import
/// arrives at a file, so the literal anchor is already the right one and
/// nothing about this call may change.
#[test]
fn a_typescript_file_keeps_its_literal_incoming_anchor() {
    let mut conn = setup();
    upsert_node(&mut conn, file("packages/math/src/index.ts")).unwrap();
    upsert_node(&mut conn, file("app/viewport.ts")).unwrap();
    imports(&mut conn, "app/viewport.ts", "packages/math/src/index.ts");

    let result = from_file(&conn, "packages/math/src/index.ts", &incoming(1)).unwrap();
    let body = json_body(&result);

    assert_eq!(body["results"][0]["filePath"], "app/viewport.ts");
    assert_eq!(body["results"][0]["kind"], "File", "a file, not a container: {body}");
    assert!(!body.to_string().contains("resolvedFrom"), "no substitution: {body}");
}

/// The other TypeScript control: an empty answer stays an empty answer.
/// A file nothing imports, in a language with no containers at all, has
/// nothing to substitute - and the zero it returns is the true one.
#[test]
fn a_typescript_file_nothing_imports_still_answers_a_plain_empty_walk() {
    let mut conn = setup();
    upsert_node(&mut conn, file("app/main.ts")).unwrap();
    upsert_node(&mut conn, file("app/util.ts")).unwrap();
    imports(&mut conn, "app/main.ts", "app/util.ts");

    let body = json_body(&from_file(&conn, "app/main.ts", &incoming(1)).unwrap());

    assert!(body["results"].as_array().unwrap().is_empty(), "{body}");
    assert_eq!(body["truncated"], false);
    assert!(!body.to_string().contains("resolvedFrom"), "nothing was substituted: {body}");
}

/// `Outgoing` is deliberately outside the substitution: those edges leave
/// the file node, so the literal anchor answers the question actually
/// asked - "what does *this file* import" - and running the walk from the
/// container would silently widen it to every file in the module.
#[test]
fn outgoing_from_a_module_graph_file_is_left_alone() {
    let mut conn = setup();
    upsert_node(&mut conn, source_file("render/render.go", "go", 60)).unwrap();
    upsert_node(
        &mut conn,
        member_in("Render", "go", "github.com/gin-gonic/gin/render", None, "render/render.go"),
    )
    .unwrap();
    let http = materialize_container(&mut conn, "go", "net/http");
    imports_container(&mut conn, "render/render.go", &http);

    let body = json_body(
        &from_file(
            &conn,
            "render/render.go",
            &WalkShape { direction: Direction::Outgoing, max_depth: Some(1), max_fanout: Some(50) },
        )
        .unwrap(),
    );

    assert_eq!(body["results"][0]["qualifiedName"], "net/http", "{body}");
    assert!(!body.to_string().contains("resolvedFrom"), "no substitution on Outgoing: {body}");
}

/// A file that declares nothing the index carries a container for - a
/// `doc.go`, a `setup.py`, a Rust integration-test binary. There is no
/// module to name, so the literal answer stands rather than being dressed
/// up as something better. The residual half of the defect, recorded
/// rather than papered over.
#[test]
fn a_file_that_defines_no_container_keeps_its_literal_answer() {
    let mut conn = setup();
    upsert_node(&mut conn, source_file("src/requests/certs.py", "python", 18)).unwrap();
    // Only the file's own module node, which is a member of the package.
    upsert_node(&mut conn, whole_file_module("certs", "python", "requests", "src/requests/certs.py", 18))
        .unwrap();

    let body = json_body(&from_file(&conn, "src/requests/certs.py", &incoming(1)).unwrap());

    assert!(body["results"].as_array().unwrap().is_empty(), "{body}");
    assert!(
        !body.to_string().contains("resolvedFrom"),
        "the package is not what this file defines, so it is not substituted: {body}"
    );
}

/// Two sibling modules declared in one file, neither inside the other:
/// no single anchor means "this file", so both are named and the caller
/// picks. Guessing here would answer about one of them while looking
/// exactly like answering about the file.
#[test]
fn a_file_defining_two_sibling_modules_is_refused_with_both_named() {
    let mut conn = setup();
    upsert_node(&mut conn, source_file("crates/x/src/pair.rs", "rust", 200)).unwrap();
    upsert_node(
        &mut conn,
        member_in("a_item", "rust", "x::pair::a", Some("x::pair"), "crates/x/src/pair.rs"),
    )
    .unwrap();
    upsert_node(
        &mut conn,
        member_in("b_item", "rust", "x::pair::b", Some("x::pair"), "crates/x/src/pair.rs"),
    )
    .unwrap();

    let message = error_text(&from_file(&conn, "crates/x/src/pair.rs", &incoming(1)).unwrap());

    assert!(message.contains("x::pair::a"), "{message}");
    assert!(message.contains("x::pair::b"), "{message}");
}
