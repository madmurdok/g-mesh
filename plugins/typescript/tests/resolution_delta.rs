//! What a TypeScript config save changes for resolution: the plugin's
//! `resolution_facts`/`resolution_delta` over a project before and after an
//! edit, the importers that delta selects, and the `specifier` every
//! `IMPORTS` edge carries for core to match it against.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`,
//! sections 3.3 and 3.6. Selection is evaluated here with the wire's own
//! `PathScope::contains` and `Matcher::matches`, the predicates core applies.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use g_mesh_plugin_sdk::wire::{EdgeKind, ImportMatch, Matcher, PathScope, ResolutionDelta};
use g_mesh_plugin_sdk::{Extractor, RelPath};
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project::facts::TsFacts;
use g_mesh_plugin_typescript::project::TsProject;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("g-mesh-ts-resolution-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { root };
        for (path, contents) in files {
            fixture.write(path, contents.as_bytes());
        }
        fixture
    }

    fn write(&self, path: &str, contents: &[u8]) {
        let full = self.root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    fn remove(&self, path: &str) {
        fs::remove_file(self.root.join(path)).unwrap();
    }

    fn load(&self) -> TsProject {
        TypeScriptExtractor.load_project(&self.root).expect("load never fails on a project tree")
    }

    /// The facts of the project as it is on disk now.
    fn facts(&self) -> String {
        TypeScriptExtractor.resolution_facts(&self.load()).expect("the TS plugin always has facts")
    }

    /// The delta from `previous` to the project as it is on disk now.
    fn delta(&self, previous: &str) -> ResolutionDelta {
        TypeScriptExtractor.resolution_delta(previous, &self.load())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Whether `delta` selects the importer `importer` of `specifier`, by its
/// specifier selectors and file scopes.
fn selects(delta: &ResolutionDelta, importer: &str, specifier: &str) -> bool {
    match delta {
        ResolutionDelta::Unchanged | ResolutionDelta::Unknown { .. } => false,
        ResolutionDelta::Affected { files, imports } => {
            files.iter().any(|scope| scope.contains(importer)) || imports.iter().any(|selector| {
                selector.importers.contains(importer)
                    && matches!(&selector.by, ImportMatch::Specifier(matcher) if matcher.matches(specifier))
            })
        }
    }
}

/// The `(importer, specifier)` pairs of `importers` that `delta` selects.
fn selected<'a>(delta: &ResolutionDelta, importers: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    importers.iter().copied().filter(|(importer, specifier)| selects(delta, importer, specifier)).collect()
}

fn specifier_matchers(delta: &ResolutionDelta) -> Vec<(PathScope, Matcher)> {
    let ResolutionDelta::Affected { imports, .. } = delta else { return Vec::new() };
    imports
        .iter()
        .filter_map(|selector| match &selector.by {
            ImportMatch::Specifier(matcher) => Some((selector.importers.clone(), matcher.clone())),
            ImportMatch::Target { .. } => None,
        })
        .collect()
}

const WORKSPACE: &[(&str, &str)] = &[
    ("package.json", r#"{"name":"root","version":"1.0.0","workspaces":["packages/*"]}"#),
    (
        "packages/math/package.json",
        r#"{"name":"@acme/math","version":"1.0.0","main":"src/index.ts","dependencies":{"left-pad":"1.0.0"}}"#,
    ),
    ("packages/math/src/index.ts", "export const add = 1;\n"),
    ("packages/strings/package.json", r#"{"name":"@acme/strings","version":"1.0.0","main":"src/index.ts"}"#),
    ("packages/strings/src/index.ts", "export const pad = 1;\n"),
    ("src/main.ts", "import { add } from \"@acme/math\";\n"),
];

/// Importers of the workspace fixture, as `(path, specifier)`.
const WORKSPACE_IMPORTERS: &[(&str, &str)] = &[
    ("src/main.ts", "@acme/math"),
    ("src/deep.ts", "@acme/math/sub"),
    ("src/strings.ts", "@acme/strings"),
    ("src/mathematics.ts", "@acme/mathematics"),
    ("packages/math/src/rel.ts", "./index"),
    ("src/util.ts", "@acme/util"),
];

/// Behaviour 1: a `version` or `dependencies` edit, of the root or of a
/// member, answers `Unchanged`.
///
/// Control: project the whole manifest (every field) in `package_facts`.
#[test]
fn a_version_or_dependencies_edit_is_unchanged() {
    let fixture = Fixture::new(WORKSPACE);
    let edits: [(&str, &str); 4] = [
        ("package.json", r#"{"name":"root","version":"2.0.0","workspaces":["packages/*"]}"#),
        (
            "packages/math/package.json",
            r#"{"name":"@acme/math","version":"1.0.1","main":"src/index.ts","dependencies":{"left-pad":"1.0.0"}}"#,
        ),
        (
            "packages/math/package.json",
            r#"{"name":"@acme/math","version":"1.0.0","main":"src/index.ts","dependencies":{"left-pad":"2.0.0","x":"1"}}"#,
        ),
        (
            "packages/strings/package.json",
            r#"{"name":"@acme/strings","version":"9.9.9","main":"src/index.ts","devDependencies":{"y":"1"}}"#,
        ),
    ];
    for (path, edited) in edits {
        let previous = fixture.facts();
        fixture.write(path, edited.as_bytes());
        assert_eq!(fixture.delta(&previous), ResolutionDelta::Unchanged, "{path}: {edited}");
    }
}

/// Behaviour 4: a member package's entry change selects importers anywhere
/// whose specifier names that package (or is under it), and no other.
///
/// Control: drop `main` from `ENTRY_FIELDS`.
#[test]
fn a_members_entry_edit_selects_the_importers_of_that_package() {
    let fixture = Fixture::new(WORKSPACE);
    let previous = fixture.facts();
    fixture.write(
        "packages/math/package.json",
        br#"{"name":"@acme/math","version":"1.0.0","main":"src/other.ts","dependencies":{"left-pad":"1.0.0"}}"#,
    );
    let delta = fixture.delta(&previous);
    assert_eq!(
        selected(&delta, WORKSPACE_IMPORTERS),
        vec![("src/main.ts", "@acme/math"), ("src/deep.ts", "@acme/math/sub")],
        "{delta:?}"
    );
}

/// Behaviour 4: adding or removing a workspace member selects the importers
/// of that member's name, anywhere, and no others.
///
/// Control: compare only the packages present on both sides in `delta`'s
/// package loop (`intersection` for `union`).
#[test]
fn adding_or_removing_a_workspace_member_selects_its_importers() {
    let fixture = Fixture::new(WORKSPACE);
    let previous = fixture.facts();
    fixture.write("packages/util/package.json", br#"{"name":"@acme/util","main":"src/index.ts"}"#);
    fixture.write("packages/util/src/index.ts", b"export const u = 1;\n");
    let added = fixture.delta(&previous);
    assert_eq!(selected(&added, WORKSPACE_IMPORTERS), vec![("src/util.ts", "@acme/util")], "{added:?}");

    let previous = fixture.facts();
    fixture.remove("packages/strings/package.json");
    let removed = fixture.delta(&previous);
    assert_eq!(
        selected(&removed, WORKSPACE_IMPORTERS),
        vec![("src/strings.ts", "@acme/strings")],
        "{removed:?}"
    );
}

const ROOT_TSCONFIG: &str =
    r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/app/*"],"@lib":["src/lib/index.ts"]}}}"#;
const EDITED_ROOT_TSCONFIG: &str =
    r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/app2/*"],"@lib":["src/lib/index.ts"]}}}"#;

fn paths_fixture() -> Fixture {
    Fixture::new(&[
        ("package.json", r#"{"name":"root"}"#),
        ("tsconfig.json", ROOT_TSCONFIG),
        ("deep/tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@deep/*":["x/*"]}}}"#),
        ("src/app/x.ts", "export const x = 1;\n"),
        ("src/app2/x.ts", "export const x = 2;\n"),
        // The model reads configs only in directories that hold a source file.
        ("deep/f.ts", "export {};\n"),
    ])
}

/// Importers of [`paths_fixture`], as `(path, specifier)`.
const PATHS_IMPORTERS: &[(&str, &str)] = &[
    ("src/a.ts", "@app/x"),
    ("src/b.ts", "@lib"),
    ("src/c.ts", "./app/x"),
    ("src/d.ts", "react"),
    ("src/e.ts", "@application"),
    ("deep/f.ts", "@app/x"),
    ("deep/inner/g.ts", "@app/x"),
    ("src/h.ts", "#internal"),
];

/// Behaviour 2: a `paths` edit at the root selects the importers under the
/// root, outside the deeper config's directory, whose specifier matches a
/// pattern of the edited config; relative, bare and look-alike specifiers
/// are not selected.
///
/// Controls: return `PathScope { under: dir, not_under: [] }` from
/// `shadowed_scope` (the shadowed `deep/` importers are selected); answer
/// `NonRelative` for every pattern in `pattern_matchers` (`react` and
/// `@application` are selected).
#[test]
fn a_paths_edit_selects_only_the_matching_unshadowed_importers() {
    let fixture = paths_fixture();
    let previous = fixture.facts();
    fixture.write("tsconfig.json", EDITED_ROOT_TSCONFIG.as_bytes());
    let delta = fixture.delta(&previous);
    assert_eq!(
        selected(&delta, PATHS_IMPORTERS),
        vec![("src/a.ts", "@app/x"), ("src/b.ts", "@lib")],
        "{delta:?}"
    );
    let ResolutionDelta::Affected { files, .. } = &delta else { unreachable!() };
    assert!(files.is_empty(), "a paths edit names importers, not file scopes: {files:?}");
    for (scope, _) in specifier_matchers(&delta) {
        assert_eq!(scope, PathScope { under: String::new(), not_under: vec!["deep".to_string()] });
    }
}

/// Behaviour 3: removing a deeper config hands its directory back to the
/// root's patterns, so the importers there matching either config's patterns
/// are selected, and nothing outside it.
///
/// Control: drop the two `inherited` calls in `delta`.
#[test]
fn removing_a_deeper_config_selects_its_importers_by_the_inherited_patterns() {
    let fixture = paths_fixture();
    let previous = fixture.facts();
    fixture.remove("deep/tsconfig.json");
    let delta = fixture.delta(&previous);
    let mut importers = PATHS_IMPORTERS.to_vec();
    importers.push(("deep/k.ts", "@deep/k"));
    assert_eq!(
        selected(&delta, &importers),
        vec![("deep/f.ts", "@app/x"), ("deep/inner/g.ts", "@app/x"), ("deep/k.ts", "@deep/k")],
        "{delta:?}"
    );
}

/// Behaviour 6: a config that cannot be read from disk (here: not UTF-8)
/// answers `Unknown`, as does a previous blob that does not decode.
///
/// Control: drop the `UNREADABLE_NOTE` check in `facts::resolution_delta`.
#[test]
fn an_unreadable_config_or_blob_answers_unknown() {
    for config in ["tsconfig.json", "package.json"] {
        let fixture = paths_fixture();
        let previous = fixture.facts();
        fixture.write(config, b"{\"compilerOptions\": \xff\xfe}");
        let delta = fixture.delta(&previous);
        assert!(matches!(delta, ResolutionDelta::Unknown { .. }), "{config}: {delta:?}");
    }

    let fixture = paths_fixture();
    for blob in ["", "not json", r#"{"format":999}"#] {
        let delta = fixture.delta(blob);
        assert!(matches!(delta, ResolutionDelta::Unknown { .. }), "{blob:?}: {delta:?}");
    }
}

/// Behaviour 6's counterpart: a config that reads but does not parse is a
/// config with no `paths`, so it answers an ordinary delta naming the old
/// patterns, not `Unknown`.
#[test]
fn a_malformed_config_answers_an_ordinary_delta() {
    let fixture = paths_fixture();
    let previous = fixture.facts();
    fixture.write("tsconfig.json", b"{ \"compilerOptions\": ");
    let delta = fixture.delta(&previous);
    assert!(matches!(delta, ResolutionDelta::Affected { .. }), "{delta:?}");
    assert_eq!(
        selected(&delta, PATHS_IMPORTERS),
        vec![("src/a.ts", "@app/x"), ("src/b.ts", "@lib")],
        "{delta:?}"
    );
}

/// An `imports` map edit at a directory selects that directory's `#`
/// importers only.
#[test]
fn an_imports_map_edit_selects_the_hash_importers_under_it() {
    let fixture = paths_fixture();
    let previous = fixture.facts();
    fixture.write("package.json", br##"{"name":"root","imports":{"#internal":"./src/app/x.ts"}}"##);
    let delta = fixture.delta(&previous);
    assert_eq!(selected(&delta, PATHS_IMPORTERS), vec![("src/h.ts", "#internal")], "{delta:?}");
}

fn import_specifiers(fixture: &Fixture, project: &TsProject, path: &str) -> Vec<Option<String>> {
    let source = fs::read_to_string(fixture.root.join(path)).unwrap();
    TypeScriptExtractor
        .extract(project, &RelPath::new(path), &source)
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Imports)
        .map(|edge| edge.specifier.clone())
        .collect()
}

/// Behaviour 10: every `IMPORTS` edge carries the specifier as written; one
/// edge shared by a relative and an alias specifier of the same file keeps
/// the non-relative one, whichever comes first; no other edge carries one.
///
/// Controls: pass `EdgeKind::Imports` through `add_edge` without a
/// specifier in `record_specifier`; keep the first specifier always in
/// `add_import_edge`.
#[test]
fn imports_edges_carry_their_specifier() {
    let fixture = paths_fixture();
    fixture.write("src/one.ts", b"import { x } from \"@app/x\";\nimport React from \"react\";\n");
    fixture.write("src/two.ts", b"import { x } from \"./app/x\";\nimport { x as y } from \"@app/x\";\n");
    fixture.write("src/three.ts", b"import { x as y } from \"@app/x\";\nimport { x } from \"./app/x\";\n");
    let project = fixture.load();

    let mut one = import_specifiers(&fixture, &project, "src/one.ts");
    one.sort();
    assert_eq!(one, vec![Some("@app/x".to_string()), Some("react".to_string())]);
    for shared in ["src/two.ts", "src/three.ts"] {
        assert_eq!(
            import_specifiers(&fixture, &project, shared),
            vec![Some("@app/x".to_string())],
            "{shared}: one edge to src/app/x.ts, keeping the alias"
        );
    }

    let source = fs::read_to_string(fixture.root.join("src/one.ts")).unwrap();
    let graph = TypeScriptExtractor.extract(&project, &RelPath::new("src/one.ts"), &source);
    assert!(
        graph.edges.iter().filter(|edge| edge.kind != EdgeKind::Imports).all(|edge| edge.specifier.is_none()),
        "{:?}",
        graph.edges
    );
}

/// The bulk walk ends with the facts of the model it walked, as its last
/// line, and the stream before it is the walk's nodes and edges.
///
/// Control: drop the trailer write in `run::bulk_index`.
#[test]
fn the_bulk_walk_ends_with_the_resolution_facts() {
    let fixture = paths_fixture();
    fixture.write("src/a.ts", b"import { x } from \"@app/x\";\n");
    let output = Command::new(env!("CARGO_BIN_EXE_g-mesh-plugin-typescript"))
        .arg("--bulk-index")
        .arg(&fixture.root)
        .output()
        .expect("the plugin binary runs");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> =
        stdout.lines().map(|line| serde_json::from_str(line).expect("every line is JSON")).collect();
    let (last, rest) = lines.split_last().expect("the walk wrote lines");
    assert!(rest.iter().all(|line| line.get("resolutionFacts").is_none()), "the trailer is written once");
    let blob =
        last["resolutionFacts"].as_str().unwrap_or_else(|| panic!("the last line is the trailer: {last}"));
    assert_eq!(TsFacts::decode(blob), Some(TsFacts::of(&fixture.load())), "the facts of the walked model");
    assert!(
        rest.iter().any(|line| line.get("kind").and_then(|kind| kind.as_str()) == Some("IMPORTS")
            && line.get("specifier").and_then(|s| s.as_str()) == Some("@app/x")),
        "the walk's IMPORTS edge carries its specifier"
    );
}
