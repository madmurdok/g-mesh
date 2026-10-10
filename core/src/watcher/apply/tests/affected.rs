//! GM-507: a `fileChanged` answered with `affected` re-extracts the files it
//! selects, or owes the whole-language reindex.
//! Design: `docs/architecture/gm-507-rust-module-tree-refresh.md`, sections 5
//! and 9 (behaviours 8 and 9).
//!
//! A scripted stub plugin answers every request until core closes the pipe,
//! and logs each one, so a test sees exactly which round trips an edit cost.

use super::*;
use crate::protocol::types::PathScope;

const FILES: usize = 10;

fn src(i: usize) -> String {
    format!("src/f{i}.rs")
}

fn scopes(indices: &[usize]) -> ResolutionDelta {
    ResolutionDelta::Affected {
        files: indices.iter().map(|&i| PathScope { under: src(i), not_under: Vec::new() }).collect(),
        imports: Vec::new(),
    }
}

/// `FILES` indexed `rust` files, each on disk under `root`.
fn seeded(root: &std::path::Path) -> IndexStore {
    let mut conn = setup_conn();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut nodes = Vec::new();
    for i in 0..FILES {
        std::fs::write(root.join(src(i)), "").unwrap();
        nodes.push(NodeRecord::new(format!("file:{}", src(i)), "File", src(i), src(i), src(i), "rust"));
    }
    apply_diff(&mut conn, &Diff { upsert_nodes: nodes, ..Default::default() }).unwrap();
    IndexStore::new(conn)
}

/// Answers every request until EOF: the `fileChanged` of `trigger` with
/// `trigger_affected`, any other `fileChanged` with `other_affected`, a
/// `semanticPass` with an empty diff. Returns the log, one line per request.
fn spawn_scripted(
    mut reader: std::io::PipeReader,
    mut writer: std::io::PipeWriter,
    trigger: String,
    trigger_affected: Option<ResolutionDelta>,
    other_affected: Option<ResolutionDelta>,
) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut log = Vec::new();
        let mut buf_reader = BufReader::new(&mut reader);
        while let Some(request) = read_message::<ControlEnvelope, _>(&mut buf_reader).unwrap() {
            let id = request.id.clone().expect("every request here expects an answer");
            let affected = match request.message {
                ControlMessage::FileChanged { file_path, reextract } => {
                    let affected =
                        if file_path == trigger { trigger_affected.clone() } else { other_affected.clone() };
                    log.push(format!("fileChanged {file_path}{}", if reextract { " reextract" } else { "" }));
                    affected
                }
                ControlMessage::SemanticPass { file_paths, .. } => {
                    log.push(format!("semanticPass {}", file_paths.join(",")));
                    None
                }
                other => panic!("unexpected request {other:?}"),
            };
            write_message(
                &mut writer,
                &FileChangeResponse {
                    jsonrpc: JSONRPC_VERSION.to_string(),
                    id,
                    result: FileChangeDiff { affected, ..Default::default() },
                    incomplete: false,
                    incomplete_reason: None,
                    unfinished_files: None,
                },
            )
            .unwrap();
        }
        log
    })
}

/// One `fileChanged` of `src/f0.rs` through [`apply_file_change`] against the
/// scripted stub: what it returned, what the plugin was asked, and the store.
fn edit_f0(
    trigger_affected: Option<ResolutionDelta>,
    other_affected: Option<ResolutionDelta>,
    semantic_pass_capable: bool,
) -> (FileChangeOutcome, Vec<String>, IndexStore) {
    let root = tempfile::tempdir().unwrap();
    let conn = seeded(root.path());
    let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
    let (core_reader, plugin_writer) = std::io::pipe().unwrap();
    let plugin = spawn_scripted(plugin_reader, plugin_writer, src(0), trigger_affected, other_affected);

    let mut buf_reader = BufReader::new(core_reader);
    let outcome = apply_file_change(
        &mut buf_reader,
        &mut core_writer,
        &conn,
        root.path(),
        "rust",
        src(0),
        RequestId::Number(1),
        &EmbeddingPipeline::disabled(),
        TEST_TIMEOUT,
        TEST_TIMEOUT,
        semantic_pass_capable,
        &mut on_timeout_must_not_fire,
    )
    .unwrap();
    drop(core_writer);
    let log = plugin.join().unwrap();
    (outcome, log, conn)
}

fn owed(conn: &IndexStore) -> Vec<(String, String, Vec<String>)> {
    conn.with(schema::owed_reextracts).unwrap()
}

/// Behaviour 8: a selection above `FALLBACK_SHARE_PERCENT` (4 of 10, the
/// edited file not counted) re-extracts nothing, sends no semantic pass,
/// owes nothing file by file, and returns `ReindexLanguage`.
///
/// Control: drop the `TooMany` check in `config_reindex::select` (the four
/// files are re-extracted and the outcome is `Applied`).
#[test]
fn a_selection_above_the_threshold_owes_the_whole_language_reindex() {
    let (outcome, log, conn) = edit_f0(Some(scopes(&[0, 1, 2, 3, 4])), None, true);

    assert!(matches!(outcome, FileChangeOutcome::ReindexLanguage { .. }), "{outcome:?}");
    assert_eq!(log, vec!["fileChanged src/f0.rs"], "nothing else is asked");
    assert!(owed(&conn).is_empty());
}

/// Behaviour 8, `unknown`: the plugin cannot say what moved, so the whole
/// language is owed.
///
/// Control: treat `Unknown` like `None` in `apply_file_change_in`.
#[test]
fn an_unknown_affected_answer_owes_the_whole_language_reindex() {
    let unknown = ResolutionDelta::Unknown { reason: "test".to_string() };
    let (outcome, log, _conn) = edit_f0(Some(unknown), None, true);

    assert!(matches!(outcome, FileChangeOutcome::ReindexLanguage { .. }), "{outcome:?}");
    assert_eq!(log, vec!["fileChanged src/f0.rs"]);
}

/// Behaviours 1 and 9: a selection at the threshold (3 of 10; the edited
/// file, also named, is left out) re-extracts exactly those files, flagged
/// `reextract`, then sends one semantic pass over the edited file and them,
/// and settles the owed rows. Each re-extract answers an `affected` of its
/// own naming `f5`, which starts no second round.
///
/// Controls: ignore `reextract` when reading `structural.affected` in
/// `apply_file_change_in` (`f5` is re-extracted too); leave `trigger` in the
/// selection in `config_reindex::select` (4 of 10, a fallback).
#[test]
fn a_selection_at_the_threshold_is_re_extracted_once_and_starts_no_second_round() {
    let (outcome, log, conn) = edit_f0(Some(scopes(&[0, 1, 2, 3])), Some(scopes(&[5])), true);

    assert_eq!(outcome, FileChangeOutcome::Applied { reextracted: 3 });
    assert_eq!(
        log,
        vec![
            "fileChanged src/f0.rs",
            "fileChanged src/f1.rs reextract",
            "fileChanged src/f2.rs reextract",
            "fileChanged src/f3.rs reextract",
            "semanticPass src/f0.rs,src/f1.rs,src/f2.rs,src/f3.rs",
        ]
    );
    assert!(owed(&conn).is_empty(), "the rows are settled after the pass");
}

/// An edit with no `affected` costs one round trip, as before GM-507.
///
/// Control: none of its own; it pins the common case's cost.
#[test]
fn an_edit_without_affected_costs_one_round_trip() {
    let (outcome, log, _conn) = edit_f0(None, None, false);

    assert_eq!(outcome, FileChangeOutcome::Applied { reextracted: 0 });
    assert_eq!(log, vec!["fileChanged src/f0.rs"]);
}
