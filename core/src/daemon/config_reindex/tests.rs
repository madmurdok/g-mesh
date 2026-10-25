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
    let selected = select_affected(conn, "ts", files, imports).unwrap();
    assert_eq!(
        indexed_file_count(conn, "ts").unwrap(),
        TS_FILES.len(),
        "the language's indexed File nodes, and only its own"
    );
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
// select_affected: keyed lookup of exact targets vs the import scan (GM-655)
// -------------------------------------------------------------------------

/// `(import scans, file lists)` this thread has run so far.
fn read_counts() -> (usize, usize) {
    (reads::IMPORT_SCANS.with(std::cell::Cell::get), reads::FILE_LISTS.with(std::cell::Cell::get))
}

/// What [`scan_imports`] alone selects for the delta: the reference every
/// keyed answer must equal.
fn scanned(conn: &Connection, files: &[PathScope], imports: &[ImportSelector]) -> Vec<String> {
    let mut selected: BTreeSet<String> = indexed_files_of(conn, "ts")
        .unwrap()
        .into_iter()
        .filter(|path| files.iter().any(|scope| scope.contains(path)))
        .collect();
    let selectors: Vec<&ImportSelector> = imports.iter().collect();
    scan_imports(conn, "ts", &selectors, &mut selected).unwrap();
    selected.into_iter().collect()
}

fn scoped_target(importers: PathScope, scope_kind: TargetScopeKind, key: &str) -> ImportSelector {
    ImportSelector { importers, by: ImportMatch::Target { scope_kind, matcher: exact(key) } }
}

/// `(what the case pins, file scopes, selectors, the expected selection)`
/// for every delta made of `Target/Exact` selectors (and file scopes).
#[allow(clippy::type_complexity)]
fn exact_cases() -> Vec<(&'static str, Vec<PathScope>, Vec<ImportSelector>, Vec<String>)> {
    use TargetScopeKind::{Container, File};
    vec![
        (
            "file linked (no placeholder row); py's importer of the same file left out",
            vec![],
            vec![by_target(File, exact("lib/e.ts"))],
            paths(&["app/a.ts", "lib/f.ts"]),
        ),
        ("file placeholder", vec![], vec![by_target(File, exact("lib/f.ts"))], paths(&["pkg/h.ts"])),
        ("a container key is not a file target", vec![], vec![by_target(File, exact("pkg.subtle"))], vec![]),
        ("container linked", vec![], vec![by_target(Container, exact("pkg.subtle"))], paths(&["appx/d.ts"])),
        (
            "container placeholder",
            vec![],
            vec![by_target(Container, exact("pkg.sub.mod"))],
            paths(&["pkg/g.ts"]),
        ),
        ("container exact is not a prefix", vec![], vec![by_target(Container, exact("pkg.sub"))], vec![]),
        (
            "a file path is not a container target",
            vec![],
            vec![by_target(Container, exact("lib/e.ts"))],
            vec![],
        ),
        (
            "importer scope on a linked file",
            vec![],
            vec![scoped_target(scope("", &["app"]), File, "lib/e.ts")],
            paths(&["lib/f.ts"]),
        ),
        (
            "importer scope on a container placeholder, inside",
            vec![],
            vec![scoped_target(scope("pkg", &[]), Container, "pkg.sub.mod")],
            paths(&["pkg/g.ts"]),
        ),
        (
            "importer scope on a container placeholder, outside",
            vec![],
            vec![scoped_target(scope("app", &[]), Container, "pkg.sub.mod")],
            vec![],
        ),
        (
            "several exact selectors",
            vec![],
            vec![
                by_target(File, exact("lib/e.ts")),
                by_target(File, exact("lib/f.ts")),
                by_target(Container, exact("pkg.subtle")),
            ],
            paths(&["app/a.ts", "appx/d.ts", "lib/f.ts", "pkg/h.ts"]),
        ),
        (
            "union with a file scope",
            vec![scope("lib", &[])],
            vec![by_target(File, exact("lib/e.ts"))],
            paths(&["app/a.ts", "lib/e.ts", "lib/f.ts"]),
        ),
    ]
}

/// A delta of only `Target/Exact` selectors is answered by the keyed lookup:
/// no import scan runs, and the answer is exactly the scan's.
///
/// Controls: in `select_affected`, send every selector to `scanned` (a scan
/// runs); drop either UNION half of `indexed_importers`' File or Container
/// arm, or the `selector.importers.contains` filter (a case differs from the
/// scan).
#[test]
fn an_exact_target_delta_runs_no_scan_and_selects_what_the_scan_does() {
    let conn = seeded();
    for (case, files, imports, expected) in exact_cases() {
        let before = read_counts();
        let selected: Vec<String> =
            select_affected(&conn, "ts", &files, &imports).unwrap().into_iter().collect();
        let after = read_counts();
        assert_eq!(after.0, before.0, "{case}: an exact-only delta ran an import scan");
        assert_eq!(
            selected,
            scanned(&conn, &files, &imports),
            "{case}: the keyed answer differs from the scan"
        );
        assert_eq!(selected, expected, "{case}");
    }
}

/// Any other selector (`Specifier`, `Target` with `Under`, `StartsWith` or
/// `NonRelative`) is answered by the scan, once per call however many there
/// are, mixed with exact ones or not; the union is still the scan's.
///
/// Control: in `select_affected`, drop the `if !scanned.is_empty()` guard
/// (the exact-only delta scans once).
#[test]
fn non_exact_selectors_share_one_scan_and_exact_only_runs_none() {
    use TargetScopeKind::{Container, File};
    let conn = seeded();
    let mixed = vec![
        by_specifier(exact("@lib/e")),
        by_specifier(Matcher::StartsWith("../".to_string())),
        by_target(Container, under("pkg.sub", ".")),
        by_target(File, Matcher::NonRelative),
        by_target(Container, exact("pkg.subtle")),
        by_target(File, exact("lib/f.ts")),
    ];
    for (case, imports, scans) in [
        ("one specifier selector", vec![by_specifier(exact("@lib/e"))], 1),
        ("one container Under selector", vec![by_target(Container, under("pkg.sub", "."))], 1),
        ("mixed exact and non-exact", mixed, 1),
        (
            "exact only",
            vec![by_target(File, exact("lib/e.ts")), by_target(Container, exact("pkg.subtle"))],
            0,
        ),
    ] {
        let before = read_counts();
        let selected: Vec<String> =
            select_affected(&conn, "ts", &[], &imports).unwrap().into_iter().collect();
        let after = read_counts();
        assert_eq!(after.0 - before.0, scans, "{case}: import scans");
        assert_eq!(selected, scanned(&conn, &[], &imports), "{case}");
    }
}

/// The language's file list is read only for a delta with file scopes: an
/// imports-only delta lists no files, whatever it selects.
///
/// Control: in `select_affected`, drop the `if !files.is_empty()` guard.
#[test]
fn an_imports_only_delta_never_lists_files() {
    let conn = seeded();
    for imports in [
        vec![],
        vec![by_target(TargetScopeKind::File, exact("nowhere.ts"))],
        vec![by_target(TargetScopeKind::File, exact("lib/e.ts"))],
        vec![by_specifier(exact("@lib/e"))],
    ] {
        let before = read_counts();
        select_affected(&conn, "ts", &[], &imports).unwrap();
        assert_eq!(read_counts().1, before.1, "{imports:?} listed the language's files");
    }
    let before = read_counts();
    select_affected(&conn, "ts", &[scope("lib", &[])], &[]).unwrap();
    assert_eq!(read_counts().1 - before.1, 1, "a file scope lists the files once");
}

/// `select` counts the language's files only once something is selected: a
/// delta that selects nothing, or only its own trigger, reads no file list at
/// all. The count still includes the trigger while the selection leaves it
/// out (3 of 8 is too many; the same 3 less the trigger is 2 of 8, not 2 of 7).
///
/// Control: in `select`, move `indexed_file_count` above the `is_empty`
/// return (`Nothing` reads one list).
#[test]
fn select_counts_the_files_only_after_a_non_empty_selection() {
    let conn = seeded();
    let waiting_at = |key: &str| vec![by_target(TargetScopeKind::File, exact(key))];

    for (case, imports, trigger) in [
        ("selects nothing", waiting_at("nowhere.ts"), None),
        ("selects only its trigger", waiting_at("lib/f.ts"), Some("pkg/h.ts")),
    ] {
        let before = read_counts();
        let selection = super::select(&conn, "ts", &[], &imports, trigger).unwrap();
        assert!(matches!(selection, Selection::Nothing), "{case}: {selection:?}");
        assert_eq!(read_counts(), before, "{case}: no scan and no file list");
    }

    let before = read_counts();
    let selection = super::select(&conn, "ts", &[], &waiting_at("lib/f.ts"), None).unwrap();
    assert!(matches!(&selection, Selection::Files(files) if files.iter().eq(["pkg/h.ts"])), "{selection:?}");
    assert_eq!(read_counts(), (before.0, before.1 + 1), "one count, no scan");

    let app = [scope("app", &[])];
    let selection = super::select(&conn, "ts", &app, &[], None).unwrap();
    assert!(matches!(selection, Selection::TooMany(_)), "3 of 8 files: {selection:?}");
    let selection = super::select(&conn, "ts", &app, &[], Some("app/a.ts")).unwrap();
    assert!(
        matches!(&selection, Selection::Files(files) if files.iter().eq(["app/b.ts", "app/sub/c.ts"])),
        "2 of 8 files: {selection:?}"
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
        generated: false,
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
    set_walk_of(
        project,
        facts,
        [wire_import(1, 0, "lib"), wire_import(2, 0, "lib"), wire_import(3, 0, "./f0")],
    );
}

/// The walk of [`set_walk`] after a resolution config change moved `f2`'s
/// `lib` import onto `f3`, a file that existed already, as a `paths` swap
/// does: every file row is unchanged, and the edge's id, which includes the
/// target, is new.
fn set_moved_walk(project: &Path, facts: Option<&str>) {
    set_walk_of(
        project,
        facts,
        [wire_import(1, 0, "lib"), wire_import(2, 3, "lib"), wire_import(3, 0, "./f0")],
    );
}

fn set_walk_of(project: &Path, facts: Option<&str>, imports: [WireEdge; 3]) {
    let mut lines: Vec<String> =
        (0..FILES).map(|i| serde_json::to_string(&wire_file(&src(i))).unwrap()).collect();
    for edge in imports {
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

/// `f2`'s import as (importer, target, specifier), at v1 (onto `f0`) and
/// after [`set_moved_walk`] (onto `f3`).
fn f2_imports() -> [(String, String, Option<String>); 2] {
    let edge = |to: usize| (format!("file:{}", src(2)), format!("file:{}", src(to)), Some("lib".to_string()));
    [edge(0), edge(3)]
}

/// GM-546: a whole-language reindex after a resolution change moves an
/// unchanged importer's edge onto its new, already existing target: `f2`'s
/// file row is the same in both walks, so the swap keeps its live edges
/// except the ones it drops. Without the capability, and with it but the
/// answer above the threshold.
///
/// Control: revert `plan_attached` to the pre-GM-546 predicates (both
/// `dropped` uses back to the target-only rule) -> `f2` still imports `f0`.
#[test]
fn a_whole_language_reindex_moves_an_unchanged_importers_edge() {
    // Each arm's imports, compared once both ran, so a control fails both.
    let mut arms = Vec::new();
    let mut expected = Vec::new();
    for capability in [false, true] {
        let fixture = fixture(capability, false, Some("facts-1"));
        let [v1_edge, moved_edge] = f2_imports();
        let mut moved = alpha_imports(&fixture.conn);
        assert!(moved.remove(&v1_edge), "the walk stored f2's v1 import: {moved:?}");
        moved.insert(moved_edge);
        expected.push((capability, moved));
        set_moved_walk(fixture.project.path(), Some("facts-2"));
        if capability {
            // f1, f2, f8, f9: 4 of 10, above the threshold.
            fixture.answer(&affected(&[&src(8), &src(9)], IMPORTS_OF_LIB, "facts-2"));
        }

        let seen = fixture.save_go_mod();

        assert!(seen.whole_reindex, "capability {capability}: the language is reindexed whole");
        assert!(re_extracted(&seen).is_empty(), "capability {capability}: {:?}", seen.requests);
        arms.push((capability, alpha_imports(&fixture.conn)));
    }
    assert_eq!(arms, expected, "f2 imports f3 instead of f0, the other imports as they were");
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
/// Control (GM-546): revert `plan_attached` to the pre-GM-546 predicates
/// (the v2 edge stays, the v1 edge is not taken).
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
    assert!(fixture.pending().is_empty(), "the swap clears the mark");
    // GM-546: f2's file row is unchanged, yet its v2-only edge goes and its
    // v1 edge is back: the edges are exactly the v1 walk's.
    assert_eq!(alpha_imports(&fixture.conn), v1, "no v2-only edge remains and the v1 edges are restored");
    assert_eq!(fixture.facts().as_deref(), Some("facts-1"));
}

// -------------------------------------------------------------------------
// GM-507: re-extracts a source edit owed and a killed daemon did not finish
// -------------------------------------------------------------------------

fn owed(conn: &IndexStore) -> Vec<(String, String, Vec<String>)> {
    conn.with(schema::owed_reextracts).unwrap()
}

/// A daemon killed inside a source edit's re-extract loop leaves its rows
/// (`reextract_owed_files`, written before the loop). The next start, with a
/// new plugin process, re-extracts exactly those files,
/// clears the rows and reindexes nothing; rows of a language no plugin
/// serves are dropped.
///
/// Control: return before the loop in `resume_owed_reextracts` when the
/// language has a manifest (nothing is re-extracted; the rows stay).
#[test]
fn owed_re_extracts_left_by_a_killed_daemon_are_finished_at_the_next_start() {
    let fixture = fixture(true, false, Some("facts-1"));
    fixture
        .conn
        .with(|conn| {
            schema::owe_reextracts(conn, "alpha", &src(0), &[src(4), src(7)])?;
            schema::owe_reextracts(conn, "gone-language", "x.gone", &["y.gone".to_string()])
        })
        .unwrap();
    // The old process died with the daemon.
    fixture.supervisor.sleep_now("the daemon was killed");
    let requests = test_plugin::requests(&fixture.dir).len();
    let notifications = test_plugin::notifications(&fixture.dir).len();

    resume_owed_reextracts(&fixture.registry, &fixture.conn);

    let asked = test_plugin::requests(&fixture.dir)[requests..].to_vec();
    assert_eq!(
        asked,
        vec![format!("fileChanged {}", src(4)), format!("fileChanged {}", src(7))],
        "one fileChanged each, nothing else"
    );
    assert!(
        !test_plugin::notifications(&fixture.dir)[notifications..]
            .iter()
            .any(|line| line.starts_with("workspaceChanged")),
        "no whole reindex"
    );
    assert!(owed(&fixture.conn).is_empty(), "every row is settled or dropped");
    assert_eq!(
        indexed_files(&fixture.conn).len(),
        FILES - 2,
        "the fake's empty answers retired the two re-extracted files"
    );
}
