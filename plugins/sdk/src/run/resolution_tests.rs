//! `resolutionChanged` and `fileChanged { reextract }` on the control
//! plane, driven through [`Session::handle`] as core sends them.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`, section 3.5.
//! The extractor here keeps its whole model in one file, `model.cfg`, and
//! names every declaration after the model's version, so an extraction says
//! which model it was made against.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use g_mesh_wire::{NodeKind, PathScope, Position, Range, ResolutionChangedResult, ResolutionDelta};

use super::*;
use crate::graph::{FileGraphBuilder, NodeSpec};
use crate::semantic::SemanticEngineFactory;

const MODEL: &str = "model.cfg";

/// The model: the trimmed text of `model.cfg`.
struct Model(String);

/// An extractor whose facts are the model's version and whose delta names
/// the two versions in its one file scope, so the answer shows it came from
/// here.
#[derive(Default)]
struct Versioned {
    presence: Mutex<Vec<(String, bool)>>,
    panic_in_delta: bool,
}

fn load(root: &Path) -> anyhow::Result<Model> {
    let text = std::fs::read_to_string(root.join(MODEL))?;
    let text = text.trim().to_string();
    anyhow::ensure!(text != "broken", "the model is broken");
    Ok(Model(text))
}

fn extract_against(model: &Model, path: &RelPath) -> FileGraph {
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", path);
    let range = Range { start: Position { line: 0, col: 0 }, end: Position { line: 0, col: 1 } };
    builder.file_node(range);
    let name = format!("built_against_{}", model.0);
    builder.add_node(NodeSpec::new(NodeKind::Function, name.clone(), name, range).public());
    builder.finish()
}

impl crate::Extractor for Versioned {
    const LANGUAGE: &'static str = "toy";
    type Project = Model;

    fn load_project(&self, root: &Path) -> anyhow::Result<Model> {
        load(root)
    }

    fn extract(&self, project: &Model, path: &RelPath, _source: &str) -> FileGraph {
        extract_against(project, path)
    }

    fn file_presence_changed(&self, _project: &mut Model, path: &RelPath, present: bool) {
        self.presence.lock().unwrap().push((path.as_str().to_string(), present));
    }

    fn resolution_facts(&self, project: &Model) -> Option<String> {
        Some(project.0.clone())
    }

    fn resolution_delta(&self, previous: &str, project: &Model) -> ResolutionDelta {
        assert!(!self.panic_in_delta, "deliberate panic for the test");
        if previous == project.0 {
            return ResolutionDelta::Unchanged;
        }
        ResolutionDelta::Affected {
            files: vec![PathScope { under: format!("{previous}->{}", project.0), not_under: Vec::new() }],
            imports: Vec::new(),
        }
    }
}

/// The same model, with the `Extractor` defaults for both resolution methods.
struct Defaults;

impl crate::Extractor for Defaults {
    const LANGUAGE: &'static str = "toy";
    type Project = Model;

    fn load_project(&self, root: &Path) -> anyhow::Result<Model> {
        load(root)
    }

    fn extract(&self, project: &Model, path: &RelPath, _source: &str) -> FileGraph {
        extract_against(project, path)
    }
}

/// A scratch project, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str, model: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("g-mesh-run-resolution-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MODEL), model).unwrap();
        std::fs::write(dir.join("a.toy"), "a\n").unwrap();
        Scratch(dir)
    }

    fn set_model(&self, model: &str) {
        std::fs::write(self.0.join(MODEL), model).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spec() -> ResolvedSpec {
    ResolvedSpec::resolve_from(&PluginSpec::new("toy", "0.0.0", &[".toy"]), None)
}

/// A session as `control_plane` builds it, its model loaded, with a semantic
/// factory that records whether it was ever called.
fn session<'a, E: Extractor>(
    extractor: &'a E,
    spec: &'a ResolvedSpec,
    root: &Path,
    engine_started: &Arc<AtomicBool>,
) -> Session<'a, E> {
    let started = Arc::clone(engine_started);
    let factory: SemanticEngineFactory = Box::new(move |_root| {
        started.store(true, Ordering::SeqCst);
        anyhow::bail!("no engine in this test")
    });
    let mut session = Session {
        extractor,
        spec,
        root: root.to_path_buf(),
        root_real: std::fs::canonicalize(root).ok(),
        project: None,
        index: SdkIndex::new(),
        engine: LazyEngine::new("toy", Some(factory)),
        project_hydrated: false,
    };
    session.load_project();
    session
}

/// Sends one frame with `id` and returns the whole response, or `None` when
/// nothing was written.
fn send<E: Extractor>(
    session: &mut Session<'_, E>,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> Option<serde_json::Value> {
    let frame =
        serde_json::json!({ "jsonrpc": JSONRPC_VERSION, "id": id, "method": method, "params": params });
    let mut out = Vec::new();
    session.handle(frame.to_string().as_bytes(), &mut out).unwrap();
    if out.is_empty() {
        return None;
    }
    let body = read_frame(&mut io::Cursor::new(out)).unwrap().expect("one framed response");
    Some(serde_json::from_slice(&body).unwrap())
}

fn resolution_changed<E: Extractor>(
    session: &mut Session<'_, E>,
    id: u64,
    previous: Option<&str>,
) -> ResolutionChangedResult {
    let params = match previous {
        Some(previous) => serde_json::json!({ "filePath": MODEL, "previousFacts": previous }),
        None => serde_json::json!({ "filePath": MODEL }),
    };
    let response = send(session, id, "resolutionChanged", params).expect("resolutionChanged is answered");
    assert_eq!(response["id"], id, "the answer carries the request's id: {response}");
    serde_json::from_value(response["result"].clone()).unwrap_or_else(|err| panic!("{err}: {response}"))
}

fn file_changed<E: Extractor>(session: &mut Session<'_, E>, reextract: Option<bool>) -> FileChangeDiff {
    let params = match reextract {
        Some(reextract) => serde_json::json!({ "filePath": "a.toy", "reextract": reextract }),
        None => serde_json::json!({ "filePath": "a.toy" }),
    };
    let response = send(session, 1, "fileChanged", params).expect("fileChanged is answered");
    serde_json::from_value(response["result"].clone()).unwrap()
}

fn upserted_names(diff: &FileChangeDiff) -> Vec<String> {
    diff.upsert_nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Function)
        .map(|node| node.name.clone())
        .collect()
}

fn unknown(result: &ResolutionChangedResult) -> bool {
    matches!(result.delta, ResolutionDelta::Unknown { .. })
}

/// the answer is the extractor's own delta and the
/// reloaded model's facts; cached extractions and hydration survive it.
///
/// Controls: answer `Unknown` in place of `delta_caught` (the delta and
/// the kept cache both fail); clear the index on every answer.
#[test]
fn a_resolution_change_answers_the_extractors_delta_and_keeps_the_cache() {
    let scratch = Scratch::new("delta", "1");
    let (spec, extractor, started) = (spec(), Versioned::default(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&extractor, &spec, &scratch.0, &started);
    assert_eq!(upserted_names(&file_changed(&mut session, None)), vec!["built_against_1"]);
    session.project_hydrated = true;

    scratch.set_model("2");
    let result = resolution_changed(&mut session, 7, Some("1"));
    assert_eq!(
        result.delta,
        ResolutionDelta::Affected {
            files: vec![PathScope { under: "1->2".to_string(), not_under: Vec::new() }],
            imports: Vec::new(),
        }
    );
    assert_eq!(result.facts.as_deref(), Some("2"), "the reloaded model's facts");
    assert!(session.index.entry(&RelPath::new("a.toy")).is_some(), "the cached extraction is kept");
    assert!(session.project_hydrated, "hydration is kept");

    let again = resolution_changed(&mut session, 8, Some("2"));
    assert_eq!(again.delta, ResolutionDelta::Unchanged, "the swapped-in model is the one compared");
    assert_eq!(again.facts.as_deref(), Some("2"));
    assert!(!started.load(Ordering::SeqCst), "no engine is started by a resolution change");
}

/// an extractor keeping the defaults answers `Unknown`
/// with no facts, and that `Unknown` clears the cache and hydration.
///
/// Control: make the `Extractor::resolution_delta` default answer
/// `Unchanged`.
#[test]
fn the_default_extractor_answers_unknown_with_no_facts_and_clears_the_cache() {
    let scratch = Scratch::new("defaults", "1");
    let (spec, started) = (spec(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&Defaults, &spec, &scratch.0, &started);
    file_changed(&mut session, None);
    session.project_hydrated = true;

    let result = resolution_changed(&mut session, 3, Some("anything"));
    assert!(unknown(&result), "{result:?}");
    assert_eq!(result.facts, None, "the default facts are none");
    assert!(session.index.entry(&RelPath::new("a.toy")).is_none(), "Unknown clears the cache");
    assert!(!session.project_hydrated, "Unknown resets hydration");
}

/// no previous facts, a panicking delta and a model that
/// fails to load all answer `Unknown`; the failed load keeps the old model.
#[test]
fn a_missing_blob_a_panicking_delta_or_a_failed_load_answers_unknown() {
    let scratch = Scratch::new("unknown", "1");
    let spec = spec();
    let started = Arc::new(AtomicBool::new(false));

    let extractor = Versioned::default();
    let mut plain = session(&extractor, &spec, &scratch.0, &started);
    let result = resolution_changed(&mut plain, 1, None);
    assert!(unknown(&result), "no previousFacts: {result:?}");

    let panicking = Versioned { panic_in_delta: true, ..Versioned::default() };
    let mut session_p = session(&panicking, &spec, &scratch.0, &started);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = resolution_changed(&mut session_p, 2, Some("0"));
    std::panic::set_hook(previous);
    assert!(unknown(&result), "a panicking delta: {result:?}");

    scratch.set_model("broken");
    let result = resolution_changed(&mut plain, 3, Some("1"));
    assert!(unknown(&result), "a failed load: {result:?}");
    assert_eq!(result.facts, None);
    assert_eq!(
        upserted_names(&file_changed(&mut plain, Some(true))),
        vec!["built_against_1"],
        "the old model stays in use after a failed load"
    );
}

/// `reextract` extracts unchanged text against the
/// reloaded model and diffs against the kept baseline; without it the
/// unchanged text is still short-circuited. Presence is applied as on any
/// `fileChanged` (present, never absent) and no engine is started.
///
/// Control: drop `!reextract &&` from the short-circuit in
/// `Session::file_changed` (the re-extract answers an empty diff).
#[test]
fn a_reextract_extracts_unchanged_text_against_the_reloaded_model() {
    let scratch = Scratch::new("reextract", "1");
    let (spec, extractor, started) = (spec(), Versioned::default(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&extractor, &spec, &scratch.0, &started);
    file_changed(&mut session, None);

    scratch.set_model("2");
    assert!(!unknown(&resolution_changed(&mut session, 2, Some("1"))));

    let plain = file_changed(&mut session, None);
    assert!(plain.upsert_nodes.is_empty() && plain.delete_node_ids.is_empty(), "{plain:?}");
    let explicit_false = file_changed(&mut session, Some(false));
    assert!(explicit_false.upsert_nodes.is_empty(), "{explicit_false:?}");

    let diff = file_changed(&mut session, Some(true));
    assert_eq!(upserted_names(&diff), vec!["built_against_2"], "extracted against the reloaded model");
    assert_eq!(diff.delete_node_ids.len(), 1, "the v1 declaration is deleted against the kept baseline");
    assert!(
        diff.upsert_nodes.iter().all(|node| node.kind != NodeKind::File),
        "the unchanged File node is not re-sent: the baseline was kept"
    );

    let again = file_changed(&mut session, Some(true));
    assert!(again.upsert_nodes.is_empty() && again.delete_node_ids.is_empty(), "{again:?}");

    let presence = extractor.presence.lock().unwrap().clone();
    assert!(!presence.is_empty() && presence.iter().all(|(path, present)| path == "a.toy" && *present));
    assert!(!started.load(Ordering::SeqCst), "a re-extract starts no semantic engine");
}

// --- GM-507: the source hook ------------------------------------------------
//
// `Extractor::source_changed` on `fileChanged`, as the SDK calls it.
// Design: `docs/architecture/gm-507-rust-module-tree-refresh.md`, section 5.

/// The `Versioned` model with a source hook that records every call, answers
/// a delta naming `b.toy` for a text containing `affects` (and for a file
/// gone), and panics on a text containing `panic`.
#[derive(Default)]
struct Hooked {
    calls: Mutex<Vec<(String, Option<String>)>>,
}

impl crate::Extractor for Hooked {
    const LANGUAGE: &'static str = "toy";
    type Project = Model;

    fn load_project(&self, root: &Path) -> anyhow::Result<Model> {
        load(root)
    }

    fn extract(&self, project: &Model, path: &RelPath, _source: &str) -> FileGraph {
        extract_against(project, path)
    }

    fn source_changed(
        &self,
        _project: &mut Model,
        path: &RelPath,
        source: Option<&str>,
    ) -> Option<ResolutionDelta> {
        self.calls.lock().unwrap().push((path.as_str().to_string(), source.map(str::to_string)));
        let text = source.unwrap_or("gone, which affects b");
        assert!(!text.contains("panic"), "deliberate panic for the test");
        text.contains("affects").then(hook_delta)
    }
}

fn hook_delta() -> ResolutionDelta {
    ResolutionDelta::Affected {
        files: vec![PathScope { under: "b.toy".to_string(), not_under: Vec::new() }],
        imports: Vec::new(),
    }
}

fn hook_calls(extractor: &Hooked) -> Vec<(String, Option<String>)> {
    extractor.calls.lock().unwrap().clone()
}

/// `fileChanged` for `a.toy`, as the raw `result` object core reads.
fn file_changed_raw<E: Extractor>(session: &mut Session<'_, E>) -> serde_json::Value {
    let response = send(session, 1, "fileChanged", serde_json::json!({ "filePath": "a.toy" }))
        .expect("fileChanged is answered");
    response["result"].clone()
}

/// GM-507 behaviour 5 (SDK half): the hook runs once for each new text of a
/// file, with that text, and once with `None` when the file is gone; a
/// `fileChanged` whose text is what the plugin last reported does not call
/// it.
///
/// Control: call `self.source_changed(path, Some(&source))` above the
/// unchanged-text short-circuit in `Session::file_changed` (the unchanged
/// save is hooked too).
#[test]
fn the_source_hook_runs_once_per_new_text_and_never_on_unchanged_text() {
    let scratch = Scratch::new("hook-once", "1");
    let (spec, extractor, started) = (spec(), Hooked::default(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&extractor, &spec, &scratch.0, &started);
    let a = |text: Option<&str>| ("a.toy".to_string(), text.map(str::to_string));

    file_changed(&mut session, None);
    assert_eq!(hook_calls(&extractor), vec![a(Some("a\n"))]);

    let unchanged = file_changed(&mut session, None);
    assert_eq!(unchanged, FileChangeDiff::default(), "unchanged text is short-circuited");
    assert_eq!(hook_calls(&extractor), vec![a(Some("a\n"))], "the unchanged save is not hooked");

    std::fs::write(scratch.0.join("a.toy"), "a2\n").unwrap();
    file_changed(&mut session, None);
    std::fs::remove_file(scratch.0.join("a.toy")).unwrap();
    file_changed(&mut session, None);
    assert_eq!(hook_calls(&extractor), vec![a(Some("a\n")), a(Some("a2\n")), a(None)]);
}

/// S5 deviation: a re-extract of unchanged text is hooked too (the hook then
/// answers for the text it already has).
///
/// Control: return before the hook when `reextract` (the second call is
/// missing).
#[test]
fn a_reextract_of_unchanged_text_is_hooked() {
    let scratch = Scratch::new("hook-reextract", "1");
    let (spec, extractor, started) = (spec(), Hooked::default(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&extractor, &spec, &scratch.0, &started);

    file_changed(&mut session, None);
    file_changed(&mut session, Some(true));

    assert_eq!(hook_calls(&extractor).len(), 2, "{:?}", hook_calls(&extractor));
}

/// GM-507 behaviours 1-4 (SDK half): the hook's delta is the answer's
/// `affected`, for a changed text and for a file gone; a hook answering
/// `None` leaves the key off the wire.
///
/// Control: drop `diff.affected = affected;` in `Session::file_changed` (the
/// changed text answers no `affected`).
#[test]
fn the_hooks_delta_reaches_the_wire_and_none_is_left_off() {
    let scratch = Scratch::new("hook-wire", "1");
    let (spec, extractor, started) = (spec(), Hooked::default(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&extractor, &spec, &scratch.0, &started);

    let plain = file_changed_raw(&mut session);
    assert!(plain.get("affected").is_none(), "no delta, no key: {plain}");
    assert!(plain["upsertNodes"].as_array().is_some_and(|nodes| !nodes.is_empty()), "{plain}");

    std::fs::write(scratch.0.join("a.toy"), "affects\n").unwrap();
    let changed = file_changed_raw(&mut session);
    let diff: FileChangeDiff = serde_json::from_value(changed.clone()).unwrap();
    assert_eq!(diff.affected, Some(hook_delta()), "{changed}");
    assert_eq!(
        upserted_names(&diff),
        Vec::<String>::new(),
        "the same graph: only the File node's text moved"
    );

    std::fs::remove_file(scratch.0.join("a.toy")).unwrap();
    let gone = file_changed_raw(&mut session);
    let diff: FileChangeDiff = serde_json::from_value(gone.clone()).unwrap();
    assert_eq!(diff.affected, Some(hook_delta()), "a deletion carries the hook's delta: {gone}");
    assert!(!diff.delete_node_ids.is_empty(), "and still deletes the file's nodes: {gone}");
}

/// The hook's panic costs its delta only: the file is still extracted and
/// answered, and the next `fileChanged` is hooked as usual.
///
/// Control: call `extractor.source_changed` without `catch_unwind` in
/// `source_changed_caught` (the panic escapes `Session::handle`).
#[test]
fn a_panicking_source_hook_costs_only_its_delta() {
    let scratch = Scratch::new("hook-panic", "1");
    let (spec, extractor, started) = (spec(), Hooked::default(), Arc::new(AtomicBool::new(false)));
    let mut session = session(&extractor, &spec, &scratch.0, &started);
    std::fs::write(scratch.0.join("a.toy"), "panic\n").unwrap();

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panicked = file_changed(&mut session, None);
    std::panic::set_hook(previous);

    assert_eq!(panicked.affected, None, "{panicked:?}");
    assert_eq!(upserted_names(&panicked), vec!["built_against_1"], "the file is still extracted");

    std::fs::write(scratch.0.join("a.toy"), "affects\n").unwrap();
    let next = file_changed(&mut session, None);
    assert_eq!(next.affected, Some(hook_delta()), "the session goes on: {next:?}");
    assert_eq!(hook_calls(&extractor).len(), 2);
}
