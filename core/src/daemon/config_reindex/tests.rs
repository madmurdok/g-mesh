//! `select_affected` against seeded rows, and `run` end to end
//! through the registry and the fake fixture plugin, whose
//! `resolutionChanged` answer each test scripts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;

use super::*;
use crate::daemon::manifest::discover;
use crate::daemon::test_plugin;
use crate::protocol::types::{
    EdgeKind, Matcher, NodeKind, Position, Range, SourceTier, Visibility, WireEdge, WireNode,
};
use crate::storage::write::{apply_diff, Diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};

// -------------------------------------------------------------------------
// select_affected, on a seeded in-memory index
// -------------------------------------------------------------------------

fn scope(under: &str, not_under: &[&str]) -> PathScope {
    PathScope { under: under.to_string(), not_under: not_under.iter().map(|dir| dir.to_string()).collect() }
}

fn everywhere() -> PathScope {
    scope("", &[])
}

fn by_specifier(matcher: Matcher) -> ImportSelector {
    ImportSelector { importers: everywhere(), by: ImportMatch::Specifier(matcher) }
}

fn by_target(scope_kind: TargetScopeKind, matcher: Matcher) -> ImportSelector {
    ImportSelector { importers: everywhere(), by: ImportMatch::Target { scope_kind, matcher } }
}

fn under(prefix: &str, separator: &str) -> Matcher {
    Matcher::Under { prefix: prefix.to_string(), separator: separator.to_string() }
}

fn exact(s: &str) -> Matcher {
    Matcher::Exact(s.to_string())
}

fn file(path: &str, language: &str) -> NodeRecord {
    NodeRecord::new(format!("file:{path}"), "File", path, path, path, language)
}

fn import(from: &str, to_id: &str, specifier: &str) -> EdgeRecord {
    let mut edge = EdgeRecord::new(
        format!("imp:{from}->{to_id}"),
        format!("file:{from}"),
        to_id,
        "IMPORTS",
        "tree-sitter",
        true,
    );
    edge.specifier = Some(specifier.to_string());
    edge
}

/// An unlinked placeholder in `importer`, waiting on `(scope_kind, scope)`.
fn placeholder(importer: &str, scope_kind: &str, scope: &str) -> NodeRecord {
    let mut node = NodeRecord::new(format!("mod:{importer}:{scope}"), "Module", scope, scope, importer, "ts");
    node.native_kind = Some("resolved_module".to_string());
    node.target = Some(PlaceholderTargetRecord {
        scope_kind: scope_kind.to_string(),
        scope: scope.to_string(),
        key_kind: "name".to_string(),
        key: "*".to_string(),
        from_container: None,
        key_path: None,
    });
    node
}

const TS_FILES: [&str; 8] =
    ["app/a.ts", "app/b.ts", "app/sub/c.ts", "appx/d.ts", "lib/e.ts", "lib/f.ts", "pkg/g.ts", "pkg/h.ts"];

/// `ts` has the eight [`TS_FILES`]; `py` has `app/z.py`. Imports of `ts`:
///
/// - `app/a.ts` -> File `lib/e.ts`, written `@lib/e` (linked);
/// - `lib/f.ts` -> File `lib/e.ts`, written `./e` (linked, relative);
/// - `pkg/g.ts` -> placeholder `container pkg.sub.mod`, written `pkg.sub.mod`;
/// - `pkg/h.ts` -> placeholder `file lib/f.ts`, written `../lib/f`;
/// - `appx/d.ts` -> container `pkg.subtle`, written `pkg.subtle` (linked).
///
/// And `app/z.py` (`py`) -> File `lib/e.ts`, written `@lib/e`.
fn seeded() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    schema::apply(&conn).unwrap();
    let mut nodes: Vec<NodeRecord> = TS_FILES.iter().map(|path| file(path, "ts")).collect();
    nodes.push(file("app/z.py", "py"));
    nodes.push(placeholder("pkg/g.ts", "container", "pkg.sub.mod"));
    nodes.push(placeholder("pkg/h.ts", "file", "lib/f.ts"));
    nodes.push(NodeRecord::new("ctr:pkg.subtle", "Module", "pkg.subtle", "pkg.subtle", "pkg/subtle", "ts"));
    apply_diff(
        &mut conn,
        &Diff {
            upsert_nodes: nodes,
            upsert_edges: vec![
                import("app/a.ts", "file:lib/e.ts", "@lib/e"),
                import("lib/f.ts", "file:lib/e.ts", "./e"),
                import("pkg/g.ts", "mod:pkg/g.ts:pkg.sub.mod", "pkg.sub.mod"),
                import("pkg/h.ts", "mod:pkg/h.ts:lib/f.ts", "../lib/f"),
                import("appx/d.ts", "ctr:pkg.subtle", "pkg.subtle"),
                import("app/z.py", "file:lib/e.ts", "@lib/e"),
            ],
            ..Default::default()
        },
    )
    .unwrap();
    conn.execute(
        "INSERT INTO containers (nodeId, language, key, parentKey, memberCount)
         VALUES ('ctr:pkg.subtle', 'ts', 'pkg.subtle', NULL, 1)",
        [],
    )
    .unwrap();
    conn
}

fn select(conn: &Connection, files: &[PathScope], imports: &[ImportSelector]) -> Vec<String> {
    let (selected, indexed) = select_affected(conn, "ts", files, imports).unwrap();
    assert_eq!(indexed, TS_FILES.len(), "the language's indexed File nodes, and only its own");
    selected.into_iter().collect()
}

fn paths(paths: &[&str]) -> Vec<String> {
    paths.iter().map(|path| path.to_string()).collect()
}

/// A file scope selects the language's files under its directory on a `/`
/// boundary, minus `notUnder`; never another language's file.
#[test]
fn a_file_scope_selects_the_languages_files_inside_it() {
    let conn = seeded();
    assert_eq!(select(&conn, &[scope("app", &["app/sub"])], &[]), paths(&["app/a.ts", "app/b.ts"]));
    assert_eq!(select(&conn, &[scope("app", &[])], &[]), paths(&["app/a.ts", "app/b.ts", "app/sub/c.ts"]));
    assert_eq!(select(&conn, &[everywhere()], &[]).len(), TS_FILES.len());
    assert!(select(&conn, &[], &[]).is_empty(), "an empty delta selects nothing");
}

/// A specifier matcher reads `edges.specifier`, whether the edge is linked
/// or not, and only the language's own imports.
#[test]
fn a_specifier_selector_matches_the_stored_import_text() {
    let conn = seeded();
    assert_eq!(select(&conn, &[], &[by_specifier(exact("@lib/e"))]), paths(&["app/a.ts"]));
    assert_eq!(
        select(&conn, &[], &[by_specifier(Matcher::NonRelative)]),
        paths(&["app/a.ts", "appx/d.ts", "pkg/g.ts"])
    );
    assert_eq!(
        select(&conn, &[], &[by_specifier(Matcher::StartsWith("../".to_string()))]),
        paths(&["pkg/h.ts"])
    );
}

/// A file target matches a linked edge's `File` path or a file-scoped
/// placeholder's scope; a container-scoped placeholder is not a file target.
#[test]
fn a_file_target_selector_matches_linked_files_and_file_placeholders() {
    let conn = seeded();
    assert_eq!(
        select(&conn, &[], &[by_target(TargetScopeKind::File, exact("lib/e.ts"))]),
        paths(&["app/a.ts", "lib/f.ts"])
    );
    assert_eq!(
        select(&conn, &[], &[by_target(TargetScopeKind::File, exact("lib/f.ts"))]),
        paths(&["pkg/h.ts"])
    );
    assert!(
        select(&conn, &[], &[by_target(TargetScopeKind::File, Matcher::StartsWith("pkg".to_string()))])
            .is_empty(),
        "neither a container key nor a container placeholder is a file target"
    );
}

/// A container target matches a linked container's key or a
/// container-scoped placeholder's scope, `Under` on its separator boundary.
///
/// Control: `Matcher::Under` without the separator boundary (`appx/d.ts`,
/// importing `pkg.subtle`, joins `pkg.sub`'s selection).
#[test]
fn a_container_target_selector_matches_on_the_key_boundary() {
    let conn = seeded();
    assert_eq!(
        select(&conn, &[], &[by_target(TargetScopeKind::Container, under("pkg.sub", "."))]),
        paths(&["pkg/g.ts"])
    );
    assert_eq!(
        select(&conn, &[], &[by_target(TargetScopeKind::Container, exact("pkg.subtle"))]),
        paths(&["appx/d.ts"])
    );
    assert!(
        select(&conn, &[], &[by_target(TargetScopeKind::Container, exact("lib/e.ts"))]).is_empty(),
        "a File target is not a container target"
    );
}

/// `importers` restricts a selector to importers inside its scope.
#[test]
fn an_importers_scope_restricts_the_selector() {
    let conn = seeded();
    let selector =
        ImportSelector { importers: scope("pkg", &[]), by: ImportMatch::Specifier(Matcher::NonRelative) };
    assert_eq!(select(&conn, &[], &[selector]), paths(&["pkg/g.ts"]));
    let selector = ImportSelector {
        importers: scope("", &["app"]),
        by: ImportMatch::Target { scope_kind: TargetScopeKind::File, matcher: exact("lib/e.ts") },
    };
    assert_eq!(select(&conn, &[], &[selector]), paths(&["lib/f.ts"]));
}

/// File scopes and import selectors add up to one set.
#[test]
fn file_scopes_and_import_selectors_are_one_union() {
    let conn = seeded();
    assert_eq!(
        select(
            &conn,
            &[scope("lib", &[])],
            &[by_specifier(exact("@lib/e")), by_target(TargetScopeKind::File, exact("lib/e.ts"))]
        ),
        paths(&["app/a.ts", "lib/e.ts", "lib/f.ts"])
    );
}

// -------------------------------------------------------------------------
// run, end to end through the registry and the fake plugin
// -------------------------------------------------------------------------

const FILES: usize = 10;

fn src(i: usize) -> String {
    format!("src/f{i}.alpha-src")
}

fn wire_file(path: &str) -> WireNode {
    WireNode {
        id: format!("file:{path}"),
        kind: NodeKind::File,
        name: path.to_string(),
        qualified_name: path.to_string(),
        file_path: path.to_string(),
        range: Range { start: Position { line: 0, col: 0 }, end: Position { line: 1, col: 0 } },
        signature: None,
        visibility: Visibility::Public,
        doc_comment: None,
        language: "alpha".to_string(),
        native_kind: None,
        has_syntax_errors: false,
        declarations: None,
        container: None,
        container_parent: None,
        target: None,
        alias_paths: Vec::new(),
        untyped_calls: Vec::new(),
        qualified_path: None,
    }
}

fn wire_import(from: usize, to: usize, specifier: &str) -> WireEdge {
    WireEdge {
        id: format!("imp:{from}->{to}"),
        from_id: format!("file:{}", src(from)),
        to_id: format!("file:{}", src(to)),
        kind: EdgeKind::Imports,
        source: SourceTier::Syntactic,
        engine: "tree-sitter".to_string(),
        resolved: true,
        to_declaration: None,
        specifier: Some(specifier.to_string()),
    }
}

/// The walk: [`FILES`] files, none of them on disk; `f1` and `f2` import
/// `f0` as `lib`, `f3` imports it as `./f0`; then the trailer, when `facts`.
fn set_walk(project: &Path, facts: Option<&str>) {
    let mut lines: Vec<String> =
        (0..FILES).map(|i| serde_json::to_string(&wire_file(&src(i))).unwrap()).collect();
    for edge in [wire_import(1, 0, "lib"), wire_import(2, 0, "lib"), wire_import(3, 0, "./f0")] {
        lines.push(serde_json::to_string(&edge).unwrap());
    }
    if let Some(facts) = facts {
        lines.push(serde_json::json!({ "resolutionFacts": facts }).to_string());
    }
    test_plugin::set_bulk_stream(project, "alpha", &lines, 0);
}

struct Fixture {
    project: tempfile::TempDir,
    _plugins: tempfile::TempDir,
    dir: PathBuf,
    registry: PluginRegistry,
    conn: IndexStore,
    supervisor: Arc<PluginSupervisor>,
}

/// `alpha`, watching `go.mod`, declaring `resolution_delta` when
/// `capability` and `semantic_pass` when `semantic`, indexed once by a
/// whole-language reindex of [`set_walk`] with `facts`.
fn fixture(capability: bool, semantic: bool, facts: Option<&str>) -> Fixture {
    let project = tempfile::tempdir().expect("failed to create a project root");
    let plugins = tempfile::tempdir().expect("failed to create a plugin root");
    let dir = if semantic {
        test_plugin::install_with_workspace_semantic_pass_capable(
            plugins.path(),
            "alpha",
            &[".alpha-src"],
            &["go.mod"],
            &[],
        )
    } else {
        test_plugin::install_with_workspace(plugins.path(), "alpha", &[".alpha-src"], &["go.mod"], &[])
    };
    if capability {
        test_plugin::declare_resolution_delta(&dir);
    }
    let discovered = discover(&[plugins.path().to_path_buf()]).expect("the fixture manifest must discover");
    let state_dir = crate::storage::connection::project_dir(project.path()).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let registry = PluginRegistry::new(
        project.path(),
        state_dir,
        discovered,
        None,
        None,
        Arc::new(EmbeddingPipeline::disabled()),
    );
    let conn = {
        let conn = crate::storage::connection::open(project.path()).expect("failed to open the live index");
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::ensure_current(&conn, "test-generation").unwrap();
        IndexStore::new(conn)
    };
    set_walk(project.path(), facts);
    let supervisor = registry.get_or_spawn("alpha").expect("the fixture plugin spawns");
    workspace_reindex::run(&registry, &supervisor, &conn, "go.mod").expect("the first index succeeds");
    assert_eq!(indexed_files(&conn).len(), FILES, "the walk indexed every file");
    Fixture { project, _plugins: plugins, dir, registry, conn, supervisor }
}

/// What a trigger did, read from the fake plugin's logs.
struct Seen {
    requests: Vec<String>,
    whole_reindex: bool,
    spawns: usize,
}

impl Fixture {
    fn answer(&self, result: &str) {
        test_plugin::set_resolution_changed_answer(&self.dir, Some(result));
    }

    /// Saves `go.mod` once, through the registry's routing, and reports what
    /// the plugin saw meanwhile.
    fn save_go_mod(&self) -> Seen {
        let requests = test_plugin::requests(&self.dir).len();
        let notifications = test_plugin::notifications(&self.dir).len();
        let spawns = test_plugin::spawns(&self.dir).len();
        self.registry.workspace_file_changed(&self.conn, "alpha", "go.mod");
        Seen {
            requests: test_plugin::requests(&self.dir)[requests..].to_vec(),
            whole_reindex: test_plugin::notifications(&self.dir)[notifications..]
                .iter()
                .any(|line| line == "workspaceChanged go.mod"),
            spawns: test_plugin::spawns(&self.dir).len() - spawns,
        }
    }

    fn facts(&self) -> Option<String> {
        self.conn.with(|conn| schema::resolution_facts(conn, "alpha")).unwrap()
    }

    fn pending(&self) -> Vec<(String, String)> {
        self.conn.with(schema::pending_reindexes).unwrap()
    }
}

fn indexed_files(conn: &IndexStore) -> BTreeSet<String> {
    conn.with(|conn| -> rusqlite::Result<BTreeSet<String>> {
        conn.prepare("SELECT filePath FROM nodes WHERE kind = 'File' AND language = 'alpha'")?
            .query_map([], |row| row.get(0))?
            .collect()
    })
    .unwrap()
}

fn re_extracted(seen: &Seen) -> BTreeSet<String> {
    seen.requests.iter().filter_map(|line| line.strip_prefix("fileChanged ")).map(str::to_string).collect()
}

fn affected(files: &[&str], imports: &str, facts: &str) -> String {
    let files: Vec<serde_json::Value> =
        files.iter().map(|path| serde_json::json!({ "under": path })).collect();
    format!(
        r#"{{"delta":{{"kind":"affected","files":{},"imports":{imports}}},"facts":"{facts}"}}"#,
        serde_json::Value::Array(files)
    )
}

const IMPORTS_OF_LIB: &str = r#"[{"importers":{"under":""},"by":{"specifier":{"exact":"lib"}}}]"#;

/// A plugin without `resolution_delta` is never asked `resolutionChanged`:
/// the save notifies `workspaceChanged` and re-walks the language.
///
/// Control: drop the `resolution_delta` condition in
/// `PluginRegistry::workspace_file_changed` (always `config_reindex::run`).
#[test]
fn without_the_capability_a_save_reindexes_the_whole_language() {
    let fixture = fixture(false, false, Some("facts-1"));
    let seen = fixture.save_go_mod();
    assert!(!seen.requests.iter().any(|line| line.starts_with("resolutionChanged")), "{:?}", seen.requests);
    assert!(seen.whole_reindex, "the save notifies workspaceChanged");
    assert_eq!(seen.spawns, 1, "one bulk walk");
    assert!(re_extracted(&seen).is_empty());
}

/// With no facts stored, core does not ask: it re-walks the language, whose
/// trailer stores facts, so the next save asks.
///
/// Control: drop `set_resolution_facts` from `walk_one_language_in` (no facts
/// after the re-walk; the second save re-walks again).
#[test]
fn with_no_stored_facts_a_save_reindexes_and_the_walk_stores_them() {
    let fixture = fixture(true, false, None);
    assert_eq!(fixture.facts(), None);
    set_walk(fixture.project.path(), Some("facts-1"));

    let seen = fixture.save_go_mod();
    assert!(seen.requests.is_empty(), "nothing is asked without facts: {:?}", seen.requests);
    assert!(seen.whole_reindex);
    assert_eq!(fixture.facts().as_deref(), Some("facts-1"), "the re-walk's trailer is stored");

    fixture.answer(r#"{"delta":{"kind":"unchanged"},"facts":"facts-2"}"#);
    let seen = fixture.save_go_mod();
    assert_eq!(seen.requests, vec!["resolutionChanged go.mod"]);
    assert!(!seen.whole_reindex);
    assert_eq!(fixture.facts().as_deref(), Some("facts-2"));
}

/// `unchanged`: nothing is re-extracted or re-walked, the rows stay, and the
/// answer's facts replace the stored ones.
///
/// Control: make the `Unchanged` arm of `selective` return `Fallback`.
#[test]
fn an_unchanged_answer_re_extracts_nothing_and_stores_the_new_facts() {
    let fixture = fixture(true, false, Some("facts-1"));
    fixture.answer(r#"{"delta":{"kind":"unchanged"},"facts":"facts-2"}"#);

    let seen = fixture.save_go_mod();

    assert_eq!(seen.requests, vec!["resolutionChanged go.mod"]);
    assert!(!seen.whole_reindex, "no workspaceChanged");
    assert_eq!(seen.spawns, 0, "no bulk walk");
    assert_eq!(indexed_files(&fixture.conn).len(), FILES);
    assert_eq!(fixture.facts().as_deref(), Some("facts-2"));
}

/// `unknown`, and an answer core cannot read, both re-walk the language,
/// whose trailer then supplies the facts.
///
/// Control: treat `Unknown` like `Unchanged` in `selective`.
#[test]
fn an_unknown_or_unreadable_answer_reindexes_the_whole_language() {
    let fixture = fixture(true, false, Some("facts-1"));
    set_walk(fixture.project.path(), Some("facts-2"));
    // No scripted answer: the fake answers `unknown`.
    let seen = fixture.save_go_mod();
    assert_eq!(seen.requests, vec!["resolutionChanged go.mod"]);
    assert!(seen.whole_reindex);
    assert_eq!(seen.spawns, 1);
    assert_eq!(fixture.facts().as_deref(), Some("facts-2"));

    set_walk(fixture.project.path(), Some("facts-3"));
    fixture.answer(r#"{"delta":{"kind":"bogus"},"facts":"never stored"}"#);
    let seen = fixture.save_go_mod();
    assert_eq!(seen.requests, vec!["resolutionChanged go.mod"]);
    assert!(seen.whole_reindex, "an unreadable answer falls back");
    assert_eq!(fixture.facts().as_deref(), Some("facts-3"));
}

/// A `.gitignore` change that alters a `resolution_delta` language's indexed
/// files reindexes the whole language without asking `resolutionChanged`: no
/// resolution delta says which files the rules added or removed, and an
/// `unchanged` answer would leave the index holding the ignored files. Here
/// the walked files are not on disk, so the empty `.gitignore` drops them all.
///
/// Control: route `gitignore_changed` through `workspace_file_changed` (the
/// selective path) - the plugin is asked, answers `unchanged`, and nothing
/// is re-walked.
#[test]
fn a_gitignore_change_reindexes_a_resolution_delta_language_whole() {
    let fixture = fixture(true, false, Some("facts-1"));
    fixture.answer(r#"{"delta":{"kind":"unchanged"},"facts":"facts-2"}"#);
    std::fs::write(fixture.project.path().join(".gitignore"), "").unwrap();
    let requests = test_plugin::requests(&fixture.dir).len();
    let notifications = test_plugin::notifications(&fixture.dir).len();

    fixture.registry.gitignore_changed(&fixture.conn, &[".gitignore".to_string()], &[]);

    let asked = test_plugin::requests(&fixture.dir)[requests..].to_vec();
    assert!(!asked.iter().any(|line| line.starts_with("resolutionChanged")), "{asked:?}");
    assert!(
        test_plugin::notifications(&fixture.dir)[notifications..]
            .iter()
            .any(|line| line == "workspaceChanged .gitignore"),
        "the whole language is re-walked"
    );
}

/// `affected` selecting 3 of 10 files (30%, not more): exactly those get a
/// `fileChanged`, nothing is re-walked, the other files keep their rows, the
/// facts are replaced and no reindex is left pending. The fake answers every
/// `fileChanged` with an empty diff and the files are not on disk, so a
/// re-extracted file loses its rows: which rows are gone is which files were
/// re-extracted.
#[test]
fn an_affected_answer_re_extracts_exactly_the_selected_files() {
    let fixture = fixture(true, false, Some("facts-1"));
    fixture.answer(&affected(&[&src(9)], IMPORTS_OF_LIB, "facts-2"));

    let seen = fixture.save_go_mod();

    assert_eq!(seen.requests[0], "resolutionChanged go.mod");
    let selected: BTreeSet<String> = [src(1), src(2), src(9)].into();
    assert_eq!(re_extracted(&seen), selected);
    assert_eq!(seen.requests.len(), 1 + selected.len(), "one fileChanged each: {:?}", seen.requests);
    assert!(!seen.whole_reindex);
    assert_eq!(seen.spawns, 0);
    let kept: BTreeSet<String> = (0..FILES).map(src).filter(|path| !selected.contains(path)).collect();
    assert_eq!(indexed_files(&fixture.conn), kept);
    assert_eq!(fixture.facts().as_deref(), Some("facts-2"));
    assert!(fixture.pending().is_empty());
}

/// One more file (4 of 10, above 30%) re-walks the language instead.
///
/// Control: remove the `FALLBACK_SHARE_PERCENT` check in `selective`.
#[test]
fn an_affected_answer_above_the_threshold_reindexes_the_whole_language() {
    let fixture = fixture(true, false, Some("facts-1"));
    set_walk(fixture.project.path(), Some("facts-3"));
    fixture.answer(&affected(&[&src(8), &src(9)], IMPORTS_OF_LIB, "facts-2"));

    let seen = fixture.save_go_mod();

    assert_eq!(seen.requests, vec!["resolutionChanged go.mod"], "nothing is re-extracted file by file");
    assert!(seen.whole_reindex);
    assert_eq!(indexed_files(&fixture.conn).len(), FILES);
    assert_eq!(fixture.facts().as_deref(), Some("facts-3"), "the walk's facts, not the answer's");
}

/// For a semantic-pass plugin, the re-extracts are followed by exactly one
/// `semanticPass`, after the last of them.
///
/// Control: re-extract with a per-file semantic pass (`send_one`'s
/// `semantic_suspended` false in `PluginProcess::reextract`).
#[test]
fn an_affected_answer_sends_one_semantic_pass_after_the_re_extracts() {
    let fixture = fixture(true, true, Some("facts-1"));
    fixture.answer(&affected(&[], IMPORTS_OF_LIB, "facts-2"));

    let seen = fixture.save_go_mod();

    assert_eq!(re_extracted(&seen), [src(1), src(2)].into());
    let passes: Vec<usize> = seen
        .requests
        .iter()
        .enumerate()
        .filter(|(_, line)| line.starts_with("semanticPass"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(passes, vec![seen.requests.len() - 1], "one pass, last: {:?}", seen.requests);
    assert!(!seen.whole_reindex);
}

/// A plugin put to sleep is woken to answer `resolutionChanged`, rather than
/// the save falling back to a re-walk.
///
/// Control: use `with_exclusive_access` in `run`, falling back when it hands
/// `None`.
#[test]
fn a_sleeping_plugin_is_woken_to_answer() {
    let fixture = fixture(true, false, Some("facts-1"));
    fixture.answer(r#"{"delta":{"kind":"unchanged"},"facts":"facts-2"}"#);
    fixture.supervisor.sleep_now("test");

    let seen = fixture.save_go_mod();

    assert_eq!(seen.requests, vec!["resolutionChanged go.mod"]);
    assert!(!seen.whole_reindex);
    assert_eq!(seen.spawns, 1, "the control-plane process was spawned again, no bulk walk");
    assert_eq!(fixture.facts().as_deref(), Some("facts-2"));
}

/// Every `IMPORTS` row of `alpha` as (importer, target, specifier).
fn alpha_imports(conn: &IndexStore) -> BTreeSet<(String, String, Option<String>)> {
    conn.with(|conn| -> rusqlite::Result<BTreeSet<(String, String, Option<String>)>> {
        conn.prepare(
            "SELECT e.fromId, e.toId, e.specifier FROM edges e JOIN nodes n ON n.id = e.fromId
             WHERE e.kind = 'IMPORTS' AND n.language = 'alpha'",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
    })
    .unwrap()
}

/// A selective run under config v2 died mid-loop: it had marked the reindex
/// pending and re-extracted `f2` (its v1 import of `f0` replaced by a v2-only
/// import of `f3`), and never stored the v2 facts. The config was then
/// reverted to v1 while the daemon was down, so the plugin would answer
/// `unchanged` against the stored v1 facts. The next start must still
/// reindex the whole language, without asking, and the swap clears the mark.
///
/// Control: drop the `pending_reindex` check at the top of `selective` (the
/// resume asks `resolutionChanged`, gets `unchanged`, re-extracts nothing,
/// and the mark stays).
#[test]
fn an_interrupted_selective_reindex_is_resumed_as_a_whole_language_reindex() {
    let fixture = fixture(true, false, Some("facts-1"));
    let v1 = alpha_imports(&fixture.conn);
    let v1_edge = (format!("file:{}", src(2)), format!("file:{}", src(0)), Some("lib".to_string()));
    let v2_edge = (format!("file:{}", src(2)), format!("file:{}", src(3)), Some("v2/f3".to_string()));
    assert!(v1.contains(&v1_edge), "the walk stored f2's v1 import: {v1:?}");

    // The interrupted run: the mark, then f2 re-extracted under v2.
    fixture.conn.with(|conn| schema::mark_pending_reindex(conn, "alpha", "go.mod")).unwrap();
    {
        let mut conn = fixture.conn.lock().unwrap();
        let v1_id: String = conn
            .query_row(
                "SELECT id FROM edges WHERE fromId = ?1 AND toId = ?2 AND kind = 'IMPORTS'",
                [&v1_edge.0, &v1_edge.1],
                |row| row.get(0),
            )
            .unwrap();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_edges: vec![import(&src(2), &v2_edge.1, "v2/f3")],
                delete_edge_ids: vec![v1_id],
                ..Default::default()
            },
        )
        .unwrap();
    }
    let interrupted = alpha_imports(&fixture.conn);
    assert!(interrupted.contains(&v2_edge) && !interrupted.contains(&v1_edge), "{interrupted:?}");

    // Reverted to v1: the plugin answers unchanged, and a re-walk is v1's.
    fixture.answer(r#"{"delta":{"kind":"unchanged"},"facts":"facts-1"}"#);
    set_walk(fixture.project.path(), Some("facts-1"));
    let requests = test_plugin::requests(&fixture.dir).len();
    let notifications = test_plugin::notifications(&fixture.dir).len();

    workspace_reindex::resume_pending(&fixture.registry, &fixture.conn);

    let asked = test_plugin::requests(&fixture.dir)[requests..].to_vec();
    assert!(!asked.iter().any(|line| line.starts_with("resolutionChanged")), "nothing is asked: {asked:?}");
    assert!(
        test_plugin::notifications(&fixture.dir)[notifications..]
            .iter()
            .any(|line| line == "workspaceChanged go.mod"),
        "the language is reindexed whole"
    );
    // Edge convergence (v2-only edge gone, v1 edge back) is GM-546.
    assert!(fixture.pending().is_empty(), "the swap clears the mark");
    assert_eq!(fixture.facts().as_deref(), Some("facts-1"));
}
