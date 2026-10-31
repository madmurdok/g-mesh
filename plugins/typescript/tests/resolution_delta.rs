//! What a TypeScript config save changes for resolution: the plugin's
//! `resolution_facts`/`resolution_delta` over a project before and after an
//! edit, the importers that delta selects, and the `specifier` every
//! `IMPORTS` edge carries for core to match it against.
//!
//! Design: `docs/architecture/gm-509-selective-config-reindex.md`,
//! sections 3.3 and 3.6. Selection is evaluated here with the wire's own
//! `PathScope::contains` and `Matcher::matches`, the predicates core applies.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use g_mesh_plugin_sdk::wire::{
    EdgeKind, ImportMatch, ImportSelector, Matcher, PathScope, ResolutionDelta, TargetScopeKind,
};
use g_mesh_plugin_sdk::{Extractor, RelPath};
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project::facts::TsFacts;
use g_mesh_plugin_typescript::project::resolve::{
    addresses_resolving_to, candidates_shadowed_by, fallbacks_shadowed_by, resolve,
};
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

// -------------------------------------------------------------------------
// source_changed: a created file offers the addresses waiting on it
// -------------------------------------------------------------------------

/// The delta naming, everywhere, the importers waiting at each of `addresses`.
fn waiting_at(addresses: &[&str]) -> ResolutionDelta {
    ResolutionDelta::Affected {
        files: Vec::new(),
        imports: addresses
            .iter()
            .map(|address| ImportSelector {
                importers: PathScope { under: String::new(), not_under: Vec::new() },
                by: ImportMatch::Target {
                    scope_kind: TargetScopeKind::File,
                    matcher: Matcher::Exact((*address).to_string()),
                },
            })
            .collect(),
    }
}

fn source_changed(project: &mut TsProject, path: &str, source: Option<&str>) -> Option<ResolutionDelta> {
    TypeScriptExtractor.source_changed(project, &RelPath::new(path), source)
}

/// After a fresh load (a plugin restart, the file created while nothing
/// watched), a file's first `fileChanged` offers every address it answers,
/// whatever the importers' text: a plain `./missing`, a split literal the
/// extractor folds to it, or no importer at all.
///
/// Control: `TsProject::source_changed` returns `None` on a first sighting.
#[test]
fn a_first_sighting_after_a_load_offers_every_address_whatever_the_importers() {
    for (case, importer) in [
        ("plain", Some("import { m } from \"./missing\";\n")),
        ("split literal", Some("export const m = import(`./${'mis'}sing`);\n")),
        ("no importer", None),
    ] {
        let mut files = vec![("src/missing.ts", "export const m = 1;\n")];
        if let Some(importer) = importer {
            files.push(("src/a.ts", importer));
        }
        let fixture = Fixture::new(&files);
        let mut project = fixture.load();
        assert_eq!(
            source_changed(&mut project, "src/missing.ts", Some("export const m = 1;\n")),
            Some(waiting_at(&["src/missing", "src/missing.js"])),
            "{case}"
        );
    }
}

/// Only the first `fileChanged` of a path per model offers; a later edit
/// offers nothing. Deletion (no source, or presence gone) forgets the path,
/// so a re-creation offers again.
///
/// Controls: drop the `announced.insert` early return (the second call
/// offers); drop `announced.remove` in the `None` branch (re-creation after a
/// delete offers nothing).
#[test]
fn a_path_is_offered_once_per_model_until_it_is_deleted() {
    let fixture = Fixture::new(&[("src/a.ts", "import { m } from \"./missing\";\n")]);
    let mut project = fixture.load();
    let offer = Some(waiting_at(&["src/missing", "src/missing.js"]));
    let text = Some("export const m = 1;\n");

    assert_eq!(source_changed(&mut project, "src/missing.ts", text), offer, "first sighting");
    assert_eq!(source_changed(&mut project, "src/missing.ts", text), None, "an edit offers nothing");

    assert_eq!(source_changed(&mut project, "src/missing.ts", None), None, "a deletion offers nothing");
    assert_eq!(source_changed(&mut project, "src/missing.ts", text), offer, "re-created after a delete");

    TypeScriptExtractor.file_presence_changed(&mut project, &RelPath::new("src/missing.ts"), false);
    assert_eq!(source_changed(&mut project, "src/missing.ts", text), offer, "re-created after presence gone");
}

/// `src/missing/index.ts` answers `./missing` too, and a file no address can
/// name besides its own path offers nothing.
///
/// Control: drop the `strip_suffix("/index")` insert in
/// `addresses_resolving_to` (`src/missing` is not offered).
#[test]
fn a_directory_index_offers_its_directory() {
    let fixture = Fixture::new(&[("src/a.ts", "import { m } from \"./missing\";\n")]);
    let mut project = fixture.load();
    assert_eq!(
        source_changed(&mut project, "src/missing/index.ts", Some("export const m = 1;\n")),
        Some(waiting_at(&["src/missing", "src/missing/index", "src/missing/index.js"]))
    );
}

/// The addresses a file answers are only those whose resolver candidates
/// include it, sorted: `./m.js` names `src/m.ts` (TS's ESM rule) but never
/// `src/m.js.ts`, though stripping `.ts` from that path yields `src/m.js`.
///
/// Control: drop the `candidate_paths` filter in `addresses_resolving_to`'s
/// `retain` (`src/m.js` is offered for `src/m.js.ts`).
#[test]
fn addresses_resolving_to_names_only_bases_whose_candidates_include_the_file() {
    assert_eq!(addresses_resolving_to("src/m.ts"), vec!["src/m", "src/m.js"]);
    assert_eq!(
        addresses_resolving_to("src/m/index.tsx"),
        vec!["src/m", "src/m/index", "src/m/index.js", "src/m/index.jsx"]
    );
    assert_eq!(addresses_resolving_to("src/m.js"), vec!["src/m"]);
    assert!(addresses_resolving_to("src/m.d.ts").contains(&"src/m".to_string()));
    assert_eq!(addresses_resolving_to("src/m.js.ts"), vec!["src/m.js.js"]);
    assert!(addresses_resolving_to("README.md").is_empty());
}

/// A root index file also answers the root's own address `.` (`..` in
/// `src/a.ts`, `.` in `main.ts`), whichever index extension it carries; a
/// directory index never does.
///
/// Control: drop the `stem == "index"` insert in `addresses_resolving_to`.
#[test]
fn a_root_index_offers_the_root_address() {
    assert_eq!(addresses_resolving_to("index.ts"), vec![".", "index", "index.js"]);
    for path in ["index.tsx", "index.d.ts", "index.js", "index.mts"] {
        assert!(addresses_resolving_to(path).contains(&".".to_string()), "{path}");
    }
    assert!(!addresses_resolving_to("src/index.ts").contains(&".".to_string()));
    assert!(!addresses_resolving_to("main.ts").contains(&".".to_string()));
}

/// A created root `index.ts` offers `.`, where an unresolved `import '..'`
/// waits, and names the root `index.tsx` it shadows there.
///
/// Control: the `stem == "index"` revert above (`.` is not offered).
#[test]
fn a_first_sighting_of_a_root_index_offers_the_root_address() {
    let fixture = Fixture::new(&[
        ("src/a.ts", "import { m } from \"..\";\n"),
        ("index.tsx", "export const m = 1;\n"),
        ("index.ts", "export const m = 2;\n"),
    ]);
    let mut project = fixture.load();
    assert_eq!(
        source_changed(&mut project, "index.ts", Some("export const m = 2;\n")),
        Some(waiting_at(&[".", "index", "index.js", "index.tsx"]))
    );
    assert_eq!(candidates_shadowed_by("index.ts", &existing(&["index.ts", "index.tsx"])), vec!["index.tsx"]);
}

// -------------------------------------------------------------------------
// source_changed: a created file offers the candidate it shadows
// -------------------------------------------------------------------------

fn existing(paths: &[&str]) -> BTreeSet<RelPath> {
    paths.iter().map(RelPath::new).collect()
}

/// Per address of the created file, only the candidate that answered it
/// before: the first existing one after the file in the resolver's order.
/// `src/m.tsx` comes before `src/m/index.ts` for `./m`, so with both present
/// only `src/m.tsx` is named; with `src/m.tsx` absent, `src/m/index.ts` is.
///
/// Control: collect every existing candidate after the file (`.filter`
/// instead of `.find` in `candidates_shadowed_by`): `src/m/index.ts` is
/// named beside `src/m.tsx`.
#[test]
fn only_the_previous_winner_of_an_address_is_named() {
    assert_eq!(
        candidates_shadowed_by("src/m.ts", &existing(&["src/m.ts", "src/m.tsx", "src/m/index.ts"])),
        vec!["src/m.tsx"]
    );
    assert_eq!(
        candidates_shadowed_by("src/m.ts", &existing(&["src/m.ts", "src/m/index.ts"])),
        vec!["src/m/index.ts"]
    );
    assert_eq!(candidates_shadowed_by("src/m.ts", &existing(&["src/m.ts", "src/m.js"])), vec!["src/m.js"]);
    assert!(candidates_shadowed_by("src/m.ts", &existing(&["src/m.ts", "src/other.ts"])).is_empty());
}

/// The created file is in the existence set it is asked against (the
/// plugin applies presence first) and is never named as its own previous
/// winner, nor is a candidate before it: `src/m.tsx` still wins `./m` over a
/// created `src/m/index.ts`, so nothing moved.
///
/// Controls: start the search at the file (`candidates[at..]`): `src/m.ts`
/// names itself; search the whole list (`candidates.iter()`): `src/m.tsx`
/// is named for `src/m/index.ts`.
#[test]
fn neither_the_file_itself_nor_an_earlier_candidate_is_named() {
    assert!(candidates_shadowed_by("src/m.ts", &existing(&["src/m.ts"])).is_empty());
    assert!(candidates_shadowed_by("src/m/index.ts", &existing(&["src/m/index.ts", "src/m.tsx"])).is_empty());
}

/// A directory index shadows the later index candidates of each of its
/// addresses (`./m`, `./m/index`, `./m/index.js`).
#[test]
fn a_directory_index_names_the_index_it_shadows() {
    assert_eq!(
        candidates_shadowed_by("src/m/index.ts", &existing(&["src/m/index.ts", "src/m/index.tsx"])),
        vec!["src/m/index.tsx"]
    );
}

/// A JS-extension address is walked in its substitution order: `./m.mjs`
/// tries `src/m.mts`, `src/m.d.mts`, `src/m.mjs`, so a created `src/m.mts`
/// shadows `src/m.d.mts` there, a file `./m` never tries, while `./m` moves
/// off `src/m/index.ts`. Both are named, sorted.
///
/// Control: skip the addresses carrying a JS extension in
/// `candidates_shadowed_by` (only `src/m/index.ts` is named).
#[test]
fn a_js_extension_address_names_its_substitution_winner() {
    assert_eq!(
        candidates_shadowed_by("src/m.mts", &existing(&["src/m.mts", "src/m.d.mts", "src/m/index.ts"])),
        vec!["src/m.d.mts", "src/m/index.ts"]
    );
}

/// A created file's first `fileChanged` offers its addresses plus the path
/// of the candidate it shadows, so core selects the importers linked there:
/// `src/m.ts` beside `src/m/index.ts` offers `src/m/index.ts`, `src/m.ts`
/// beside `src/m.tsx` offers `src/m.tsx`.
///
/// Control: drop the `keys.extend(resolve::candidates_shadowed_by(..))`
/// line in `TsProject::source_changed` (only the addresses are offered).
#[test]
fn a_first_sighting_offers_the_candidate_it_shadows() {
    for (lower, offered) in [
        ("src/m/index.ts", ["src/m", "src/m.js", "src/m/index.ts"]),
        ("src/m.tsx", ["src/m", "src/m.js", "src/m.tsx"]),
    ] {
        let fixture = Fixture::new(&[
            (lower, "export const m = 1;\n"),
            ("src/m.ts", "export const m = 2;\n"),
            ("src/a.ts", "import { m } from \"./m\";\n"),
        ]);
        let mut project = fixture.load();
        assert_eq!(
            source_changed(&mut project, "src/m.ts", Some("export const m = 2;\n")),
            Some(waiting_at(&offered)),
            "{lower}"
        );
    }
}

/// Two candidates created in one batch (the existence set already holds
/// both): the higher one names the lower new one, the lower new one names
/// the old winner, so between them the importers linked to the old winner
/// are selected whichever is announced first.
///
/// Control: the `keys.extend` revert above (neither offer names
/// `src/m/index.ts`).
#[test]
fn a_batch_of_two_candidates_offers_the_old_winner() {
    let fixture = Fixture::new(&[
        ("src/m/index.ts", "export const m = 1;\n"),
        ("src/m.ts", "export const m = 2;\n"),
        ("src/m.tsx", "export const m = 3;\n"),
    ]);
    let mut project = fixture.load();
    assert_eq!(
        source_changed(&mut project, "src/m.ts", Some("export const m = 2;\n")),
        Some(waiting_at(&["src/m", "src/m.js", "src/m.tsx"]))
    );
    assert_eq!(
        source_changed(&mut project, "src/m.tsx", Some("export const m = 3;\n")),
        Some(waiting_at(&["src/m", "src/m.js", "src/m.jsx", "src/m/index.ts"]))
    );
}

// -------------------------------------------------------------------------
// source_changed: a created file offers the alias, package or `#` fallback
// it shadows
// -------------------------------------------------------------------------

/// The importer every fallback fixture adds, and the file it resolves from.
const MAIN: &str = "src/main.ts";

/// A root tsconfig whose `paths` are `paths` (a JSON object), with `baseUrl`
/// at the root.
fn alias_tsconfig(paths: &str) -> String {
    format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{paths}}}}}"#)
}

/// The file `specifier`, written in [`MAIN`], names in `project`.
fn resolves_to(project: &TsProject, specifier: &str) -> Option<String> {
    resolve(project, specifier, &RelPath::new(MAIN)).map(|path| path.as_str().to_string())
}

/// `files` plus [`MAIN`] importing `specifier`: loaded without `created`,
/// `specifier` names `before`; with every path of `created` written (one
/// batch: the model holds them all), it names `after`. Returns that model,
/// so a test asks it what the creation shadows.
fn created_over(
    files: &[(&str, &str)],
    specifier: &str,
    before: &str,
    created: &[&str],
    after: &str,
) -> (Fixture, TsProject) {
    let importer = format!("import {{ v }} from \"{specifier}\";\n");
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push((MAIN, &importer));
    let fixture = Fixture::new(&all);
    assert_eq!(resolves_to(&fixture.load(), specifier).as_deref(), Some(before), "{specifier} before");
    for path in created {
        fixture.write(path, b"export const v = 2;\n");
    }
    let project = fixture.load();
    assert_eq!(resolves_to(&project, specifier).as_deref(), Some(after), "{specifier} after");
    (fixture, project)
}

/// The exact targets `delta` offers, sorted.
fn offered(delta: Option<ResolutionDelta>) -> BTreeSet<String> {
    let Some(ResolutionDelta::Affected { imports, .. }) = delta else {
        return BTreeSet::new();
    };
    imports
        .into_iter()
        .filter_map(|selector| match selector.by {
            ImportMatch::Target { matcher: Matcher::Exact(target), .. } => Some(target),
            _ => None,
        })
        .collect()
}

const SRC_THEN_GENERATED: &str = r#"{"@/*":["src/*","generated/*"]}"#;

/// `@/*` tries `src/*` before `generated/*`: `@/x` linked to
/// `generated/x.ts` moves to a created `src/x.ts`, which names
/// `generated/x.ts`; a created directory index `src/x/index.ts` (address
/// `src/x`) does the same.
///
/// Control: skip the tsconfig loop of `fallbacks_shadowed_by`.
#[test]
fn a_created_file_of_an_earlier_alias_target_names_the_later_one() {
    let tsconfig = alias_tsconfig(SRC_THEN_GENERATED);
    for created in ["src/x.ts", "src/x/index.ts"] {
        let files = [("tsconfig.json", tsconfig.as_str()), ("generated/x.ts", "export const v = 1;\n")];
        let (_fixture, project) = created_over(&files, "@/x", "generated/x.ts", &[created], created);
        assert_eq!(fallbacks_shadowed_by(&project, created), vec!["generated/x.ts"], "{created}");
    }
}

/// The reverse: a created `generated/x.ts` comes after the existing
/// `src/x.ts`, which keeps `@/x`; nothing is named.
///
/// Control: search the whole candidate list in `displaced_by`, not only
/// after the file (`src/x.ts` is named).
#[test]
fn a_created_file_of_a_later_alias_target_names_nothing() {
    let tsconfig = alias_tsconfig(SRC_THEN_GENERATED);
    let files = [("tsconfig.json", tsconfig.as_str()), ("src/x.ts", "export const v = 1;\n")];
    let (_fixture, project) = created_over(&files, "@/x", "src/x.ts", &["generated/x.ts"], "src/x.ts");
    assert!(fallbacks_shadowed_by(&project, "generated/x.ts").is_empty());
}

const THREE_BASES: &str = r#"{"@/*":["a/*","b/*","c/*"]}"#;

/// With `b/x.ts` and `c/x.ts` existing, `@/x` was on `b/x.ts`; a created
/// `a/x.ts` names only that one, not `c/x.ts`, which no importer reached
/// through `@/x`.
///
/// Control: name the last existing candidate after the file in
/// `displaced_by` (`c/x.ts` is named).
#[test]
fn only_the_previous_winner_among_alias_targets_is_named() {
    let tsconfig = alias_tsconfig(THREE_BASES);
    let files = [
        ("tsconfig.json", tsconfig.as_str()),
        ("b/x.ts", "export const v = 1;\n"),
        ("c/x.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) = created_over(&files, "@/x", "b/x.ts", &["a/x.ts"], "a/x.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "a/x.ts"), vec!["b/x.ts"]);
}

/// `a/x.ts` and `b/x.ts` created in one batch over an existing `c/x.ts`:
/// the model holds both before either is announced, and whichever is
/// announced first, the offers of the two together name `c/x.ts`, where
/// `@/x` was linked.
///
/// Control: let `displaced_by` answer `None` when an existing candidate
/// other than the file comes before it (`b/x.ts`'s event sees the new
/// `a/x.ts` and names nothing).
#[test]
fn a_batch_of_alias_targets_names_the_old_winner_in_either_order() {
    let tsconfig = alias_tsconfig(THREE_BASES);
    for order in [["a/x.ts", "b/x.ts"], ["b/x.ts", "a/x.ts"]] {
        let files = [("tsconfig.json", tsconfig.as_str()), ("c/x.ts", "export const v = 1;\n")];
        let (_fixture, mut project) = created_over(&files, "@/x", "c/x.ts", &["a/x.ts", "b/x.ts"], "a/x.ts");
        let mut keys = BTreeSet::new();
        for path in order {
            keys.extend(offered(source_changed(&mut project, path, Some("export const v = 2;\n"))));
        }
        assert!(keys.contains("c/x.ts"), "{order:?}: {keys:?}");
    }
}

/// An exact alias `"cfg": ["src/cfg", "gen/cfg"]`: a created `src/cfg.ts`
/// names `gen/cfg.ts`.
///
/// Control: the tsconfig-loop revert above.
#[test]
fn a_created_file_of_an_exact_alias_names_its_later_target() {
    let tsconfig = alias_tsconfig(r#"{"cfg":["src/cfg","gen/cfg"]}"#);
    let files = [("tsconfig.json", tsconfig.as_str()), ("gen/cfg.ts", "export const v = 1;\n")];
    let (_fixture, project) = created_over(&files, "cfg", "gen/cfg.ts", &["src/cfg.ts"], "src/cfg.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "src/cfg.ts"), vec!["gen/cfg.ts"]);
}

/// A created file's first `fileChanged` offers the alias target it shadows
/// beside its own addresses.
///
/// Control: drop the `keys.extend(resolve::fallbacks_shadowed_by(..))` line
/// in `TsProject::source_changed`.
#[test]
fn a_first_sighting_offers_the_alias_target_it_shadows() {
    let tsconfig = alias_tsconfig(SRC_THEN_GENERATED);
    let files = [("tsconfig.json", tsconfig.as_str()), ("generated/x.ts", "export const v = 1;\n")];
    let (_fixture, mut project) = created_over(&files, "@/x", "generated/x.ts", &["src/x.ts"], "src/x.ts");
    assert_eq!(
        source_changed(&mut project, "src/x.ts", Some("export const v = 2;\n")),
        Some(waiting_at(&["generated/x.ts", "src/x", "src/x.js"]))
    );
}

const WORKSPACE_ROOT: (&str, &str) = ("package.json", r#"{"name":"root","workspaces":["packages/*"]}"#);

/// A package without entry fields tries `src/index` before `index` for its
/// root: `@s/p` linked to `packages/p/index.ts` moves to a created
/// `packages/p/src/index.ts`, which names it.
///
/// Control: skip the workspace loop of `fallbacks_shadowed_by`.
#[test]
fn a_created_package_source_index_names_the_root_index() {
    let files = [
        WORKSPACE_ROOT,
        ("packages/p/package.json", r#"{"name":"@s/p"}"#),
        ("packages/p/index.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) = created_over(
        &files,
        "@s/p",
        "packages/p/index.ts",
        &["packages/p/src/index.ts"],
        "packages/p/src/index.ts",
    );
    assert_eq!(fallbacks_shadowed_by(&project, "packages/p/src/index.ts"), vec!["packages/p/index.ts"]);
}

/// A package subpath tries the path under the package before the one under
/// `src/`: `@s/p/u` linked to `packages/p/src/u.ts` moves to a created
/// `packages/p/u.ts`, which names it.
///
/// Control: the workspace-loop revert above.
#[test]
fn a_created_package_subpath_file_names_the_src_one() {
    let files = [
        WORKSPACE_ROOT,
        ("packages/p/package.json", r#"{"name":"@s/p"}"#),
        ("packages/p/src/u.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) =
        created_over(&files, "@s/p/u", "packages/p/src/u.ts", &["packages/p/u.ts"], "packages/p/u.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "packages/p/u.ts"), vec!["packages/p/src/u.ts"]);
}

/// An `exports` wildcard listing `./gen/*.ts` before `./lib/*.ts`, as an
/// array or as a condition object (`source` ranks before `default`):
/// `@s/p/u` linked to `packages/p/lib/u.ts` moves to a created
/// `packages/p/gen/u.ts`, which names it. Only the file's own path matches
/// the `.ts` pattern; no address of it or source convention yields `./u`.
///
/// Controls: the workspace-loop revert above; leave the file's own path out
/// of the address set in `fallbacks_shadowed_by` (nothing is named).
#[test]
fn a_created_file_of_an_earlier_exports_target_names_the_later_one() {
    for exports in [r#"["./gen/*.ts","./lib/*.ts"]"#, r#"{"source":"./gen/*.ts","default":"./lib/*.ts"}"#] {
        let manifest = format!(r#"{{"name":"@s/p","exports":{{"./*":{exports}}}}}"#);
        let files = [
            WORKSPACE_ROOT,
            ("packages/p/package.json", manifest.as_str()),
            ("packages/p/lib/u.ts", "export const v = 1;\n"),
        ];
        let (_fixture, project) = created_over(
            &files,
            "@s/p/u",
            "packages/p/lib/u.ts",
            &["packages/p/gen/u.ts"],
            "packages/p/gen/u.ts",
        );
        assert_eq!(
            fallbacks_shadowed_by(&project, "packages/p/gen/u.ts"),
            vec!["packages/p/lib/u.ts"],
            "{exports}"
        );
    }
}

/// A workspace package is tried before tsconfig `paths`: `@s/p`, whose
/// package has no existing entry, was answered by the alias `@s/*` ->
/// `gen/*`; a created `packages/p/index.ts` takes it and names `gen/p.ts`.
///
/// Control: drop the alias chain from the workspace loop of
/// `fallbacks_shadowed_by` (only the package entries are walked).
#[test]
fn a_created_package_entry_names_the_alias_target_it_takes_over() {
    let tsconfig = alias_tsconfig(r#"{"@s/*":["gen/*"]}"#);
    let files = [
        WORKSPACE_ROOT,
        ("tsconfig.json", tsconfig.as_str()),
        ("packages/p/package.json", r#"{"name":"@s/p"}"#),
        // The model reads a package.json only in a directory holding a file.
        ("packages/p/other.ts", "export const o = 1;\n"),
        ("gen/p.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) =
        created_over(&files, "@s/p", "gen/p.ts", &["packages/p/index.ts"], "packages/p/index.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "packages/p/index.ts"), vec!["gen/p.ts"]);
}

/// A package entry may leave the package directory: `"main":
/// "../../shared/x.ts"` comes before the `index` convention, so a created
/// `shared/x.ts` takes `@s/p` from `packages/p/index.ts` and names it,
/// though it is outside the package. Another package, which cannot name it,
/// adds nothing.
///
/// Control: let `package_may_name` ignore `..` entries (the package is
/// skipped and nothing is named).
#[test]
fn a_created_file_named_by_an_entry_outside_its_package_names_the_old_entry() {
    let files = [
        WORKSPACE_ROOT,
        ("packages/p/package.json", r#"{"name":"@s/p","main":"../../shared/x.ts"}"#),
        ("packages/p/index.ts", "export const v = 1;\n"),
        ("packages/q/package.json", r#"{"name":"@s/q"}"#),
        ("packages/q/index.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) =
        created_over(&files, "@s/p", "packages/p/index.ts", &["shared/x.ts"], "shared/x.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "shared/x.ts"), vec!["packages/p/index.ts"]);
}

/// An `imports` key listing `./src/x.ts` before `./gen/x.ts`: `#x` linked to
/// `gen/x.ts` moves to a created `src/x.ts`, which names it.
///
/// Control: skip the `imports` loop of `fallbacks_shadowed_by`.
#[test]
fn a_created_file_of_an_earlier_imports_target_names_the_later_one() {
    let files = [
        ("package.json", r##"{"name":"root","imports":{"#x":["./src/x.ts","./gen/x.ts"]}}"##),
        ("gen/x.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) = created_over(&files, "#x", "gen/x.ts", &["src/x.ts"], "src/x.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "src/x.ts"), vec!["gen/x.ts"]);
}

/// `files` plus `importer` importing `specifier`, which names `before`; a
/// root `index.ts` is then created and `specifier` names `after`. Returns
/// that model, so a test asks it what the creation shadows.
fn root_index_created_over(
    files: &[(&str, &str)],
    importer: &str,
    specifier: &str,
    before: &str,
    after: &str,
) -> (Fixture, TsProject) {
    let resolves = |project: &TsProject| {
        resolve(project, specifier, &RelPath::new(importer)).map(|path| path.as_str().to_string())
    };
    let source = format!("import {{ v }} from \"{specifier}\";\n");
    let mut all: Vec<(&str, &str)> = files.to_vec();
    all.push((importer, &source));
    let fixture = Fixture::new(&all);
    assert_eq!(resolves(&fixture.load()).as_deref(), Some(before), "{specifier} before");
    fixture.write("index.ts", b"export const v = 2;\n");
    let project = fixture.load();
    assert_eq!(resolves(&project).as_deref(), Some(after), "{specifier} after");
    (fixture, project)
}

/// A tsconfig `paths` target `.` listed before a fallback, and a package
/// whose `main` is `../..` before its `index` convention: the import linked
/// to the fallback moves to a created root `index.ts`, which names it.
///
/// Controls: `expand_paths_candidates` or `package_entry_targets`' legacy
/// fields resolve with `paths::inside` (the import stays on the fallback);
/// `specifiers_targeting` inverts with `paths::inside` (nothing is named).
#[test]
fn a_created_root_index_names_the_fallback_of_a_paths_or_main_target_naming_the_root() {
    let tsconfig = alias_tsconfig(r#"{"~":[".","fallback/p"]}"#);
    let files = [("tsconfig.json", tsconfig.as_str()), ("fallback/p.ts", "export const v = 1;\n")];
    let (_fixture, project) = root_index_created_over(&files, MAIN, "~", "fallback/p.ts", "index.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "index.ts"), vec!["fallback/p.ts"]);

    let files = [
        WORKSPACE_ROOT,
        ("packages/c/package.json", r#"{"name":"@s/c","main":"../.."}"#),
        ("packages/c/index.ts", "export const v = 1;\n"),
    ];
    let (_fixture, project) =
        root_index_created_over(&files, MAIN, "@s/c", "packages/c/index.ts", "index.ts");
    assert_eq!(fallbacks_shadowed_by(&project, "index.ts"), vec!["packages/c/index.ts"]);
}

/// An `imports` target spelled as a module name and naming the root (`.`
/// from the root package, `..\` from `sub`, `..\..` from `a/b`) listed
/// before a fallback: the import moves to a created root `index.ts`, which
/// names the fallback.
///
/// Controls: `imports_targets` resolves with `paths::inside`, or tests the
/// slash-normalised target (the import stays on the fallback);
/// `imports_specifiers_targeting` inverts with `paths::inside` (nothing is
/// named).
#[test]
fn a_created_root_index_names_the_fallback_of_an_imports_module_name_target() {
    for (manifest, json, importer, fallback) in [
        (
            "package.json",
            r##"{"name":"root","imports":{"#r":[".","./fallback/i.ts"]}}"##,
            MAIN,
            "fallback/i.ts",
        ),
        (
            "sub/package.json",
            r##"{"name":"sub","imports":{"#r":["..\\","./fc.ts"]}}"##,
            "sub/main.ts",
            "sub/fc.ts",
        ),
        (
            "a/b/package.json",
            r##"{"name":"ab","imports":{"#r":["..\\..","./ff.ts"]}}"##,
            "a/b/main.ts",
            "a/b/ff.ts",
        ),
    ] {
        let files = [(manifest, json), (fallback, "export const v = 1;\n")];
        let (_fixture, project) = root_index_created_over(&files, importer, "#r", fallback, "index.ts");
        assert_eq!(fallbacks_shadowed_by(&project, "index.ts"), vec![fallback], "{manifest}");
    }
}

/// An `imports` target spelled relatively and naming the root (`./` from
/// the root package, `../` from `sub`) names nothing, so the import stays on
/// its fallback when a root `index.ts` is created, and that is not named.
///
/// Controls: `paths::inside` keeps the root (`./` moves); the module-name
/// test ignores a leading `../` (`../` moves).
#[test]
fn a_created_root_index_leaves_a_relative_imports_target_naming_the_root_alone() {
    for (manifest, json, importer, fallback) in [
        (
            "package.json",
            r##"{"name":"root","imports":{"#r":["./","./fallback/s.ts"]}}"##,
            MAIN,
            "fallback/s.ts",
        ),
        (
            "sub/package.json",
            r##"{"name":"sub","imports":{"#r":["../","./fd.ts"]}}"##,
            "sub/main.ts",
            "sub/fd.ts",
        ),
    ] {
        let files = [(manifest, json), (fallback, "export const v = 1;\n")];
        let (_fixture, project) = root_index_created_over(&files, importer, "#r", fallback, fallback);
        assert!(!fallbacks_shadowed_by(&project, "index.ts").contains(&fallback.to_string()), "{manifest}");
    }
}
