//! The project model and resolver: `TsProject::load` on real temporary trees,
//! `resolve`, the presence hook, and the pure pieces under them (candidate
//! paths, package specifiers and entries, `exports`/`imports` maps, workspace
//! globs, pnpm files, tsconfig `extends`/`paths`/`baseUrl`, JSONC).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use g_mesh_plugin_sdk::{walk_project, Extractor, RelPath};
use g_mesh_plugin_typescript::extractor::grammar::EXTENSIONS as GRAMMAR_EXTENSIONS;
use g_mesh_plugin_typescript::extractor::TypeScriptExtractor;
use g_mesh_plugin_typescript::project::exports::{
    collect_condition_targets, condition_rank, exports_targets, imports_targets, key_targets, match_wildcard,
    PackageImports,
};
use g_mesh_plugin_typescript::project::jsonc::{parse_json, parse_jsonc, strip_jsonc, Json};
use g_mesh_plugin_typescript::project::paths::inside;
use g_mesh_plugin_typescript::project::resolve::{candidate_paths, resolve_relative, EXTENSIONS};
use g_mesh_plugin_typescript::project::tsconfig::{
    expand_paths_candidates, extends_candidates, own_resolve_dir, EffectiveConfig, PathsEntry,
};
use g_mesh_plugin_typescript::project::workspace::{
    expand_pattern, package_entry_targets, parse_bare_specifier, pnpm_workspace_patterns, scalar_value,
    workspace_packages, workspace_patterns, DirectoryTree, WorkspacePackage,
};
use g_mesh_plugin_typescript::project::{TsProject, EXCLUDE_DIRS};

// --- fixtures -----------------------------------------------------------------

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

/// A project tree in its own temporary directory, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("g-mesh-ts-project-model-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let fixture = Self { root };
        for (path, contents) in files {
            fixture.write(path, contents);
        }
        fixture
    }

    fn write(&self, path: &str, contents: &str) {
        let full = self.root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, contents).unwrap();
    }

    fn load(&self) -> TsProject {
        TsProject::load(&self.root).expect("load never fails on a project tree")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

const SRC: &str = "export {};\n";

fn rel(path: &str) -> RelPath {
    RelPath::new(path)
}

fn resolve(project: &TsProject, specifier: &str, from: &str) -> Option<String> {
    project.resolve(specifier, &rel(from)).map(|path| path.as_str().to_string())
}

fn some(path: &str) -> Option<String> {
    Some(path.to_string())
}

fn json(text: &str) -> Json {
    parse_json(text).expect("valid JSON")
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn keys(value: &Json) -> Vec<&str> {
    value.as_object().expect("an object").iter().map(|(key, _)| key.as_str()).collect()
}

fn has_note(project: &TsProject, needles: &[&str]) -> bool {
    project.notes.iter().any(|note| needles.iter().all(|needle| note.contains(needle)))
}

// --- candidate paths ------------------------------------------------------------

#[test]
fn a_js_extension_offers_the_ts_sources_before_the_js_file_itself() {
    assert_eq!(candidate_paths("src/x.js"), strings(&["src/x.ts", "src/x.tsx", "src/x.d.ts", "src/x.js"]));
    assert_eq!(candidate_paths("src/x.jsx"), strings(&["src/x.tsx", "src/x.jsx"]));
    assert_eq!(candidate_paths("m.mjs"), strings(&["m.mts", "m.d.mts", "m.mjs"]));
    assert_eq!(candidate_paths("c.cjs"), strings(&["c.cts", "c.d.cts", "c.cjs"]));
}

#[test]
fn an_extensionless_base_offers_every_extension_then_every_index() {
    let mut expected: Vec<String> = EXTENSIONS.iter().map(|ext| format!("lib/util{ext}")).collect();
    expected.extend(EXTENSIONS.iter().map(|ext| format!("lib/util/index{ext}")));
    assert_eq!(candidate_paths("lib/util"), expected);
    assert_eq!(
        EXTENSIONS,
        [".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
        "TypeScript's own extensions come before the JS ones"
    );
}

#[test]
fn a_base_already_naming_a_source_file_is_its_only_candidate() {
    for base in ["a.ts", "a.tsx", "a.mts", "a.cts", "types.d.ts", "dir/a.ts"] {
        assert_eq!(candidate_paths(base), vec![base.to_string()], "{base}");
    }
}

// --- relative resolution ------------------------------------------------------

#[test]
fn a_specifier_that_already_names_a_source_file_resolves_to_itself() {
    let fx =
        Fixture::new(&[("a.ts", SRC), ("b.ts", SRC), ("c.tsx", SRC), ("d.mts", SRC), ("types.d.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./b.ts", "a.ts"), some("b.ts"));
    assert_eq!(resolve(&project, "./c.tsx", "a.ts"), some("c.tsx"));
    assert_eq!(resolve(&project, "./d.mts", "a.ts"), some("d.mts"));
    assert_eq!(resolve(&project, "./types.d.ts", "a.ts"), some("types.d.ts"));
}

#[test]
fn an_extensionless_specifier_picks_up_the_source_extension() {
    let fx =
        Fixture::new(&[("src/a.ts", SRC), ("src/util.ts", SRC), ("src/m.mts", SRC), ("src/legacy.cjs", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./util", "src/a.ts"), some("src/util.ts"));
    assert_eq!(resolve(&project, "./m", "src/a.ts"), some("src/m.mts"));
    assert_eq!(resolve(&project, "./legacy", "src/a.ts"), some("src/legacy.cjs"));
}

#[test]
fn typescripts_own_extensions_win_over_the_js_ones_for_the_same_stem() {
    let fx = Fixture::new(&[
        ("a.ts", SRC),
        ("x.ts", SRC),
        ("x.js", SRC),
        ("y.tsx", SRC),
        ("y.js", SRC),
        ("z.d.ts", SRC),
        ("z.js", SRC),
        ("p.ts", SRC),
        ("p.tsx", SRC),
        ("w.js", SRC),
        ("w.jsx", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./x", "a.ts"), some("x.ts"));
    assert_eq!(resolve(&project, "./y", "a.ts"), some("y.tsx"));
    assert_eq!(resolve(&project, "./z", "a.ts"), some("z.d.ts"));
    assert_eq!(resolve(&project, "./p", "a.ts"), some("p.ts"));
    assert_eq!(resolve(&project, "./w", "a.ts"), some("w.js"));
}

#[test]
fn an_esm_js_specifier_resolves_to_the_ts_source_it_is_compiled_from() {
    let fx = Fixture::new(&[
        ("a.ts", SRC),
        ("only-ts.ts", SRC),
        ("only-tsx.tsx", SRC),
        ("only-dts.d.ts", SRC),
        ("both.ts", SRC),
        ("both.js", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./only-ts.js", "a.ts"), some("only-ts.ts"));
    assert_eq!(resolve(&project, "./only-tsx.js", "a.ts"), some("only-tsx.tsx"));
    assert_eq!(resolve(&project, "./only-dts.js", "a.ts"), some("only-dts.d.ts"));
    assert_eq!(resolve(&project, "./both.js", "a.ts"), some("both.ts"));
}

#[test]
fn a_real_js_file_next_to_ts_sources_still_resolves_to_itself() {
    let fx = Fixture::new(&[("a.ts", SRC), ("other.ts", SRC), ("real.js", SRC), ("real.mjs", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./real.js", "a.ts"), some("real.js"));
    assert_eq!(resolve(&project, "./real.mjs", "a.ts"), some("real.mjs"));
}

#[test]
fn the_other_emitted_extension_pairs_substitute_the_same_way() {
    let fx = Fixture::new(&[
        ("a.ts", SRC),
        ("m.mts", SRC),
        ("dm.d.mts", SRC),
        ("c.cts", SRC),
        ("dc.d.cts", SRC),
        ("j.tsx", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./m.mjs", "a.ts"), some("m.mts"));
    assert_eq!(resolve(&project, "./dm.mjs", "a.ts"), some("dm.d.mts"));
    assert_eq!(resolve(&project, "./c.cjs", "a.ts"), some("c.cts"));
    assert_eq!(resolve(&project, "./dc.cjs", "a.ts"), some("dc.d.cts"));
    assert_eq!(resolve(&project, "./j.jsx", "a.ts"), some("j.tsx"));
}

#[test]
fn a_directory_specifier_resolves_to_its_index_file_in_extension_order() {
    let fx = Fixture::new(&[
        ("a.ts", SRC),
        ("lib/index.ts", SRC),
        ("both/index.tsx", SRC),
        ("both/index.js", SRC),
        ("jsdir/index.js", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./lib", "a.ts"), some("lib/index.ts"));
    assert_eq!(resolve(&project, "./lib/", "a.ts"), some("lib/index.ts"));
    assert_eq!(resolve(&project, "./both", "a.ts"), some("both/index.tsx"));
    assert_eq!(resolve(&project, "./jsdir", "a.ts"), some("jsdir/index.js"));
}

#[test]
fn a_file_wins_over_a_same_named_directorys_index() {
    let fx = Fixture::new(&[("a.ts", SRC), ("lib.ts", SRC), ("lib/index.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./lib", "a.ts"), some("lib.ts"));
}

#[test]
fn dotdot_segments_are_resolved_against_the_importing_files_directory() {
    let fx = Fixture::new(&[("src/a/b.ts", SRC), ("src/a/sib.ts", SRC), ("src/c.ts", SRC), ("root.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "../c", "src/a/b.ts"), some("src/c.ts"));
    assert_eq!(resolve(&project, "../../root", "src/a/b.ts"), some("root.ts"));
    assert_eq!(resolve(&project, "./../a/sib", "src/a/b.ts"), some("src/a/sib.ts"));
}

#[test]
fn an_importer_at_the_project_root_resolves_against_the_root() {
    let fx = Fixture::new(&[("main.ts", SRC), ("util.ts", SRC), ("src/x.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./util", "main.ts"), some("util.ts"));
    assert_eq!(resolve(&project, "./src/x", "main.ts"), some("src/x.ts"));
}

#[test]
fn relative_resolution_never_claims_a_bare_or_private_specifier() {
    let fx = Fixture::new(&[("a.ts", SRC), ("lib.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve_relative(&project, "lib", &rel("a.ts")), None);
    assert_eq!(resolve_relative(&project, "#lib", &rel("a.ts")), None);
    assert_eq!(resolve(&project, "lib", "a.ts"), None);
    assert_eq!(
        resolve_relative(&project, "./lib", &rel("a.ts")).as_ref().map(RelPath::as_str),
        Some("lib.ts")
    );
}

#[test]
fn a_dangling_relative_import_resolves_to_nothing() {
    let fx = Fixture::new(&[("a.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./missing", "a.ts"), None);
    assert_eq!(resolve(&project, "./missing.js", "a.ts"), None);
    assert_eq!(resolve(&project, "../missing", "a.ts"), None);
}

#[test]
fn a_specifier_climbing_out_of_the_project_root_resolves_to_nothing() {
    let fx = Fixture::new(&[("src/a.ts", SRC), ("x.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "../x", "src/a.ts"), some("x.ts"));
    assert_eq!(resolve(&project, "../../x", "src/a.ts"), None);
    assert_eq!(resolve(&project, "../../../etc/passwd", "src/a.ts"), None);
    assert_eq!(resolve(&project, "../src/a", "x.ts"), None);
}

#[test]
fn a_specifier_naming_the_root_itself_stays_unresolved() {
    let fx = Fixture::new(&[
        ("index.ts", SRC),
        ("main.ts", SRC),
        ("src/index.ts", SRC),
        ("src/a.ts", SRC),
        ("src/deep/b.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, ".", "main.ts"), None);
    assert_eq!(resolve(&project, "./", "main.ts"), None);
    assert_eq!(resolve(&project, "..", "src/a.ts"), None);
    // The same forms naming a directory below the root do resolve.
    assert_eq!(resolve(&project, ".", "src/a.ts"), some("src/index.ts"));
    assert_eq!(resolve(&project, "..", "src/deep/b.ts"), some("src/index.ts"));
}

#[test]
fn an_absolute_importer_path_resolves_nothing() {
    let fx = Fixture::new(&[("abs/a.ts", SRC), ("abs/b.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./b", "abs/a.ts"), some("abs/b.ts"));
    assert_eq!(resolve(&project, "./b", "/abs/a.ts"), None);
}

#[test]
fn a_target_this_plugin_does_not_parse_is_not_claimed_as_resolved() {
    let fx = Fixture::new(&[("a.ts", SRC), ("styles.css", "a{}"), ("data.json", "{}"), ("readme.md", "# x")]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./styles.css", "a.ts"), None);
    assert_eq!(resolve(&project, "./data.json", "a.ts"), None);
    assert_eq!(resolve(&project, "./styles", "a.ts"), None);
    assert_eq!(resolve(&project, "./readme.md", "a.ts"), None);
}

// --- the existence set ----------------------------------------------------------

#[test]
fn a_gitignored_file_never_resolves() {
    let fx = Fixture::new(&[
        (".gitignore", "ignored/\ngen.ts\n"),
        ("a.ts", SRC),
        ("ignored/x.ts", SRC),
        ("gen.ts", SRC),
    ]);
    let project = fx.load();
    assert!(!project.existence.contains(&rel("ignored/x.ts")));
    assert_eq!(resolve(&project, "./ignored/x", "a.ts"), None);
    assert_eq!(resolve(&project, "./gen", "a.ts"), None);
    assert_eq!(resolve(&project, "./gen.js", "a.ts"), None);
}

#[test]
fn a_file_under_a_hard_excluded_directory_never_resolves() {
    let fx = Fixture::new(&[("a.ts", SRC), ("dist/x.ts", SRC), ("node_modules/pkg/index.ts", SRC)]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./dist/x", "a.ts"), None);
    assert_eq!(resolve(&project, "./node_modules/pkg/index", "a.ts"), None);
    assert_eq!(resolve(&project, "./node_modules/pkg", "a.ts"), None);
}

#[test]
fn a_gitignored_build_entry_does_not_shadow_the_source_it_was_built_from() {
    let fx = Fixture::new(&[
        (".gitignore", "build/\n"),
        ("package.json", r#"{"name":"root","workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/p/package.json", r#"{"name":"p","main":"./build/index.js","types":"./build/index.d.ts"}"#),
        ("packages/p/build/index.js", SRC),
        ("packages/p/build/index.d.ts", SRC),
        ("packages/p/src/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "p", "app.ts"), some("packages/p/src/index.ts"));
}

#[test]
fn a_dist_entry_does_not_shadow_the_source_either() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/p/package.json", r#"{"name":"p","main":"./dist/index.js","exports":"./dist/index.js"}"#),
        ("packages/p/dist/index.js", SRC),
        ("packages/p/src/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "p", "app.ts"), some("packages/p/src/index.ts"));
}

#[test]
fn a_declared_entry_wins_when_the_build_output_is_really_there() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/p/package.json", r#"{"name":"p","main":"./lib/main.js"}"#),
        ("packages/p/lib/main.js", SRC),
        ("packages/p/src/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "p", "app.ts"), some("packages/p/lib/main.js"));
}

// --- resolution order -----------------------------------------------------------

#[test]
fn a_private_specifier_never_falls_through_to_tsconfig_paths() {
    let fx = Fixture::new(&[
        (
            "tsconfig.json",
            r##"{"compilerOptions":{"paths":{"#cfg":["./src/cfg.ts"],"#lib/*":["./src/*"]}}}"##,
        ),
        ("src/a.ts", SRC),
        ("src/cfg.ts", SRC),
        ("src/x.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "#cfg", "src/a.ts"), None);
    assert_eq!(resolve(&project, "#lib/x", "src/a.ts"), None);
}

#[test]
fn a_relative_specifier_is_never_aliased() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"./util":["./src/util.ts"],"./*":["./src/*"]}}}"#),
        ("a.ts", SRC),
        ("src/util.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "./util", "a.ts"), None);
}

#[test]
fn the_workspace_answer_wins_over_an_alias_for_the_same_specifier() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"@acme/math":["./alias/math.ts"],"@acme/*":["./alias/*"]}}}"#,
        ),
        ("app.ts", SRC),
        ("alias/math.ts", SRC),
        ("alias/other.ts", SRC),
        ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
        ("packages/math/src/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@acme/math", "app.ts"), some("packages/math/src/index.ts"));
    // Not a workspace package: the alias map answers.
    assert_eq!(resolve(&project, "@acme/other", "app.ts"), some("alias/other.ts"));
}

#[test]
fn a_package_outside_the_workspace_stays_unresolved() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("node_modules/react/package.json", r#"{"name":"react","main":"index.js"}"#),
        ("node_modules/react/index.js", SRC),
        ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
        ("packages/math/src/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "react", "app.ts"), None);
    assert_eq!(resolve(&project, "lodash/fp", "app.ts"), None);
    assert_eq!(resolve(&project, "node:fs", "app.ts"), None);
    assert_eq!(resolve(&project, "@acme/other", "app.ts"), None);
    assert_eq!(resolve(&project, "@acme/math", "app.ts"), some("packages/math/src/index.ts"));
}

#[test]
fn a_project_with_no_workspace_manifest_has_no_packages_and_resolves_no_bare_name() {
    let fx = Fixture::new(&[
        ("app.ts", SRC),
        ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
        ("packages/math/src/index.ts", SRC),
    ]);
    let project = fx.load();
    assert!(project.packages.is_empty());
    assert_eq!(resolve(&project, "@acme/math", "app.ts"), None);
}

#[test]
fn a_workspace_package_whose_entry_is_missing_is_not_invented() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/p/package.json", r#"{"name":"p","main":"./dist/index.js"}"#),
        ("packages/p/lib/other.ts", SRC),
    ]);
    let project = fx.load();
    assert!(project.packages.contains_key("p"));
    assert_eq!(resolve(&project, "p", "app.ts"), None);
}

// --- bare specifiers ------------------------------------------------------------

#[test]
fn a_package_specifier_splits_into_its_name_and_the_subpath_it_addresses() {
    let split = |name: &str, subpath: &str| Some((name.to_string(), subpath.to_string()));
    assert_eq!(parse_bare_specifier("lodash"), split("lodash", "."));
    assert_eq!(parse_bare_specifier("lodash/fp"), split("lodash", "./fp"));
    assert_eq!(parse_bare_specifier("@scope/pkg"), split("@scope/pkg", "."));
    assert_eq!(parse_bare_specifier("@scope/pkg/a/b"), split("@scope/pkg", "./a/b"));
}

#[test]
fn what_is_not_a_package_specifier_at_all_is_refused() {
    for specifier in [
        "",
        "./x",
        "../x",
        ".",
        "/abs/x",
        "#private",
        "node:fs",
        "https://example.com/x.js",
        "data:text/javascript,x",
        "@scope",
        "@scope/",
        "@scope//pkg",
        "a//b",
        "pkg/",
    ] {
        assert_eq!(parse_bare_specifier(specifier), None, "{specifier:?}");
    }
}

// --- package entries ------------------------------------------------------------

fn package(dir: &str, manifest: &str) -> WorkspacePackage {
    WorkspacePackage { dir: dir.to_string(), manifest: json(manifest) }
}

#[test]
fn the_declared_entry_fields_are_offered_in_a_fixed_order_after_exports() {
    // Fields written in reverse: the order comes from the code, not the file.
    let pkg = package(
        "packages/p",
        r#"{"typings":"ty.d.ts","types":"t.d.ts","module":"mod.js","main":"m.js","source":"s.ts","exports":"./e.js"}"#,
    );
    assert_eq!(
        package_entry_targets(&pkg, "."),
        strings(&[
            "packages/p/e.js",
            "packages/p/s.ts",
            "packages/p/m.js",
            "packages/p/mod.js",
            "packages/p/t.d.ts",
            "packages/p/ty.d.ts",
            "packages/p/src/index",
            "packages/p/index",
        ])
    );
}

#[test]
fn a_subpath_falls_back_to_the_same_path_inside_the_package_and_its_src() {
    let plain = package("packages/p", r#"{"name":"p"}"#);
    assert_eq!(
        package_entry_targets(&plain, "./utils"),
        strings(&["packages/p/utils", "packages/p/src/utils"])
    );
    let mapped = package("packages/p", r#"{"exports":{"./utils":"./lib/utils.js"}}"#);
    assert_eq!(
        package_entry_targets(&mapped, "./utils"),
        strings(&["packages/p/lib/utils.js", "packages/p/utils", "packages/p/src/utils"])
    );
}

#[test]
fn an_entry_pointing_outside_the_project_is_not_offered() {
    let conventions = strings(&["packages/p/src/index", "packages/p/index"]);
    for manifest in [
        r#"{"main":"../../../etc/passwd"}"#,
        r#"{"main":"/etc/passwd"}"#,
        r#"{"main":"C:/evil.js"}"#,
        r#"{"main":"c:\\evil.js"}"#,
        r#"{"exports":"../../../outside.js"}"#,
    ] {
        assert_eq!(package_entry_targets(&package("packages/p", manifest), "."), conventions, "{manifest}");
    }
}

#[test]
fn a_repeated_entry_is_offered_once() {
    let pkg =
        package("packages/p", r#"{"main":"src/index","module":"./src/index","source":"./src/../src/index"}"#);
    assert_eq!(package_entry_targets(&pkg, "."), strings(&["packages/p/src/index", "packages/p/index"]));
}

#[test]
fn paths_inside_refuses_absolute_drive_lettered_and_escaping_targets() {
    assert_eq!(inside("pkg", "./a.ts"), some("pkg/a.ts"));
    assert_eq!(inside("pkg", "../shared/a.ts"), some("shared/a.ts"));
    assert_eq!(inside("pkg", "../../a.ts"), None);
    assert_eq!(inside("pkg", "/a.ts"), None);
    assert_eq!(inside("pkg", "C:/a.ts"), None);
    assert_eq!(inside("pkg", "z:a.ts"), None);
    assert_eq!(inside("pkg", ""), None);
    assert_eq!(inside("", "."), None);
    assert_eq!(inside("", "./a.ts"), some("a.ts"));
}

#[test]
fn an_exports_map_decides_the_entry_for_the_package_root_and_its_subpaths() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        (
            "packages/p/package.json",
            r#"{"name":"p","exports":{".":"./src/main.ts","./feature":"./src/feat/impl.ts","./deep/*":"./src/deep/*.ts"}}"#,
        ),
        ("packages/p/src/index.ts", SRC),
        ("packages/p/src/main.ts", SRC),
        ("packages/p/src/feat/impl.ts", SRC),
        ("packages/p/src/feature.ts", SRC),
        ("packages/p/src/deep/a/b.ts", SRC),
        ("packages/p/src/helpers.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "p", "app.ts"), some("packages/p/src/main.ts"));
    assert_eq!(resolve(&project, "p/feature", "app.ts"), some("packages/p/src/feat/impl.ts"));
    assert_eq!(resolve(&project, "p/deep/a/b", "app.ts"), some("packages/p/src/deep/a/b.ts"));
    // No key for it: the source tree answers.
    assert_eq!(resolve(&project, "p/helpers", "app.ts"), some("packages/p/src/helpers.ts"));
}

/// A wildcard key binds its suffix as well as its prefix: a subpath that
/// starts right but ends wrong is not that key's, even when the capture it
/// would give names a real file.
#[test]
fn a_wildcard_exports_key_does_not_match_a_subpath_with_the_wrong_suffix() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/p/package.json", r#"{"name":"p","exports":{"./x/*.js":"./lib/*.ts"}}"#),
        ("packages/p/lib/a.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "p/x/a.js", "app.ts"), some("packages/p/lib/a.ts"));
    assert_eq!(resolve(&project, "p/x/a.ts", "app.ts"), None);
}

#[test]
fn an_exports_map_with_both_an_import_and_a_require_target_picks_import() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/p/package.json", r#"{"name":"p","exports":{".":{"require":"./r.ts","import":"./i.ts"}}}"#),
        ("packages/p/r.ts", SRC),
        ("packages/p/i.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "p", "app.ts"), some("packages/p/i.ts"));
}

// --- exports maps ---------------------------------------------------------------

#[test]
fn a_string_or_array_exports_value_is_the_package_roots_entry() {
    let string = json(r#""./index.js""#);
    assert_eq!(exports_targets(Some(&string), "."), strings(&["./index.js"]));
    assert_eq!(exports_targets(Some(&string), "./x"), Vec::<String>::new());
    let array = json(r#"["./a.js", "./b.js"]"#);
    assert_eq!(exports_targets(Some(&array), "."), strings(&["./a.js", "./b.js"]));
    assert_eq!(exports_targets(Some(&array), "./a"), Vec::<String>::new());
    assert_eq!(exports_targets(None, "."), Vec::<String>::new());
}

#[test]
fn a_condition_map_without_subpath_keys_describes_the_package_root() {
    let conditions = json(r#"{"require":"./r.js","import":"./i.js"}"#);
    assert_eq!(exports_targets(Some(&conditions), "."), strings(&["./i.js", "./r.js"]));
    assert_eq!(exports_targets(Some(&conditions), "./r"), Vec::<String>::new());
}

#[test]
fn an_exact_exports_key_wins_outright_over_wildcards() {
    let map = json(r#"{"./*":"./src/*.ts","./utils":"./lib/utils.js"}"#);
    assert_eq!(exports_targets(Some(&map), "./utils"), strings(&["./lib/utils.js"]));
    assert_eq!(exports_targets(Some(&map), "./other"), strings(&["./src/other.ts"]));
}

#[test]
fn every_matching_wildcard_exports_key_contributes_in_declaration_order() {
    let map = json(r#"{"./*":"./a/*.js","./feat/*":"./b/*.js","./nope/*":"./c/*.js"}"#);
    assert_eq!(exports_targets(Some(&map), "./feat/x"), strings(&["./a/feat/x.js", "./b/x.js"]));
    assert_eq!(exports_targets(Some(&map), "."), Vec::<String>::new());
}

#[test]
fn a_null_exports_target_contributes_nothing() {
    let map = json(r#"{".":"./i.js","./internal/*":null}"#);
    assert_eq!(exports_targets(Some(&map), "./internal/x"), Vec::<String>::new());
    assert_eq!(exports_targets(Some(&map), "."), strings(&["./i.js"]));
}

#[test]
fn conditions_rank_source_and_esm_first_and_types_after_the_runtime_ones() {
    let mut out = Vec::new();
    collect_condition_targets(
        &json(r#"{"types":"t","default":"d","require":"r","import":"i","source":"s","module":"m"}"#),
        &mut out,
    );
    assert_eq!(out, strings(&["s", "i", "m", "r", "d", "t"]));
    assert_eq!(condition_rank("source"), 0);
    assert!(condition_rank("default") < condition_rank("types"));
    assert!(condition_rank("types") < condition_rank("typings"));
}

#[test]
fn unknown_conditions_rank_after_the_known_ones_in_declaration_order() {
    let mut out = Vec::new();
    collect_condition_targets(
        &json(r#"{"zeta":"z","types":"t","alpha":"a","import":"i","mid":"m"}"#),
        &mut out,
    );
    assert_eq!(out, strings(&["i", "t", "z", "a", "m"]));
}

#[test]
fn nested_condition_objects_are_ranked_at_every_level() {
    let mut out = Vec::new();
    collect_condition_targets(
        &json(r#"{"require":"r.js","import":{"types":"i.d.ts","default":"i.js"},"other":[null,"o.js",1]}"#),
        &mut out,
    );
    assert_eq!(out, strings(&["i.js", "i.d.ts", "r.js", "o.js"]));
}

#[test]
fn a_wildcard_capture_replaces_every_star_in_the_target() {
    assert_eq!(key_targets(&json(r#""./src/*/*.ts""#), Some("x")), strings(&["./src/x/x.ts"]));
    assert_eq!(
        key_targets(&json(r#"{"require":"./cjs/*.js","import":"./esm/*.js"}"#), Some("a/b")),
        strings(&["./esm/a/b.js", "./cjs/a/b.js"])
    );
    assert_eq!(key_targets(&json(r#""./src/*.ts""#), None), strings(&["./src/*.ts"]));
}

#[test]
fn match_wildcard_captures_between_the_prefix_and_the_suffix() {
    assert_eq!(match_wildcard("./*.js", "./a/b.js"), Some("a/b"));
    assert_eq!(match_wildcard("#lib/*", "#lib/x"), Some("x"));
    assert_eq!(match_wildcard("*", ""), Some(""));
    assert_eq!(match_wildcard("./feat/*", "./other/x"), None);
    assert_eq!(match_wildcard("./a*a", "./a"), None, "prefix and suffix may not overlap");
    assert_eq!(match_wildcard("./exact", "./exact"), None, "a pattern without `*` captures nothing");
    assert_eq!(match_wildcard("./*/x*", "./a/x*"), Some("a"), "text after the first `*` is literal");
}

// --- imports maps -----------------------------------------------------------------

fn imports(dir: &str, map: &str) -> PackageImports {
    PackageImports { dir: dir.to_string(), imports: json(map).as_object().unwrap().to_vec() }
}

#[test]
fn an_exact_imports_key_resolves_to_its_declared_target() {
    let config = imports("packages/math", r##"{"#a":"./one.ts","#b":"./two.ts"}"##);
    assert_eq!(imports_targets(&config, "#a"), strings(&["packages/math/one.ts"]));
    assert_eq!(imports_targets(&config, "#b"), strings(&["packages/math/two.ts"]));
    assert_eq!(imports_targets(&config, "#a/x"), Vec::<String>::new());
}

#[test]
fn a_wildcard_imports_key_substitutes_the_captured_part() {
    let config = imports("", r##"{"#lib/*":"./src/lib/*.ts"}"##);
    assert_eq!(imports_targets(&config, "#lib/a/b"), strings(&["src/lib/a/b.ts"]));
    assert_eq!(imports_targets(&config, "#other"), Vec::<String>::new());
}

#[test]
fn a_condition_imports_value_is_ranked_like_exports() {
    let config = imports("", r##"{"#x":{"require":"./r.ts","types":"./t.d.ts","import":"./i.ts"}}"##);
    assert_eq!(imports_targets(&config, "#x"), strings(&["i.ts", "r.ts", "t.d.ts"]));
}

#[test]
fn all_matching_imports_keys_contribute_in_declaration_order_without_repeats() {
    let config = imports("", r##"{"#a/*":"./x/*.ts","#a/b":"./y.ts","#a/b*":"./x/b*.ts"}"##);
    assert_eq!(imports_targets(&config, "#a/b"), strings(&["x/b.ts", "y.ts"]));
}

#[test]
fn an_imports_target_escaping_the_project_is_refused() {
    let config = imports("packages/math", r##"{"#leak":"../../../etc/passwd","#abs":"/etc/passwd"}"##);
    assert_eq!(imports_targets(&config, "#leak"), Vec::<String>::new());
    assert_eq!(imports_targets(&config, "#abs"), Vec::<String>::new());
}

#[test]
fn a_private_specifier_resolves_through_the_importers_own_imports_map() {
    let fx = Fixture::new(&[
        (
            "package.json",
            r##"{"name":"app","imports":{"#utils":"./src/utils.ts","#lib/*":"./src/lib/*.js"}}"##,
        ),
        ("src/a.ts", SRC),
        ("src/utils.ts", SRC),
        ("src/lib/x.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "#utils", "src/a.ts"), some("src/utils.ts"));
    assert_eq!(resolve(&project, "#lib/x", "src/a.ts"), some("src/lib/x.ts"));
    assert_eq!(project.imports_for("src").map(|config| config.dir.as_str()), Some(""));
}

#[test]
fn an_unmatched_private_specifier_stays_unresolved() {
    let fx = Fixture::new(&[
        ("package.json", r##"{"imports":{"#utils":"./src/utils.ts"}}"##),
        ("src/a.ts", SRC),
        ("src/utils.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "#nope", "src/a.ts"), None);
    assert_eq!(resolve(&project, "#", "src/a.ts"), None);
}

#[test]
fn a_private_specifier_with_no_enclosing_package_json_stays_unresolved() {
    let fx = Fixture::new(&[("src/a.ts", SRC), ("src/utils.ts", SRC)]);
    let project = fx.load();
    assert!(project.imports_for("src").is_none());
    assert_eq!(resolve(&project, "#utils", "src/a.ts"), None);
}

#[test]
fn the_nearest_package_json_wins_for_private_specifiers() {
    let fx = Fixture::new(&[
        ("package.json", r##"{"imports":{"#x":"./root-x.ts","#only-root":"./root-x.ts"}}"##),
        ("root-x.ts", SRC),
        ("b.ts", SRC),
        ("packages/sub/package.json", r##"{"name":"sub","imports":{"#x":"./sub-x.ts"}}"##),
        ("packages/sub/sub-x.ts", SRC),
        ("packages/sub/src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "#x", "packages/sub/src/a.ts"), some("packages/sub/sub-x.ts"));
    assert_eq!(resolve(&project, "#x", "b.ts"), some("root-x.ts"));
    // The sub-package's map shadows the root's whole, not key by key.
    assert_eq!(resolve(&project, "#only-root", "packages/sub/src/a.ts"), None);
}

#[test]
fn a_package_json_without_imports_shadows_its_parents_map() {
    let fx = Fixture::new(&[
        ("package.json", r##"{"imports":{"#x":"./root-x.ts"}}"##),
        ("root-x.ts", SRC),
        ("packages/sub/package.json", r#"{"name":"sub"}"#),
        ("packages/sub/src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert!(project.imports_for("packages/sub/src").is_none());
    assert_eq!(resolve(&project, "#x", "packages/sub/src/a.ts"), None);
}

#[test]
fn a_malformed_package_json_shadows_its_parents_map_and_is_noted() {
    let fx = Fixture::new(&[
        ("package.json", r##"{"imports":{"#x":"./root-x.ts"}}"##),
        ("root-x.ts", SRC),
        ("packages/sub/package.json", "{ not json"),
        ("packages/sub/src/a.ts", SRC),
        ("packages/arr/package.json", "[1, 2]"),
        ("packages/arr/a.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "#x", "packages/sub/src/a.ts"), None);
    assert_eq!(resolve(&project, "#x", "packages/arr/a.ts"), None);
    assert!(has_note(&project, &["packages/sub/package.json"]), "{:?}", project.notes);
    assert!(has_note(&project, &["packages/arr/package.json"]), "{:?}", project.notes);
}

// --- pnpm-workspace.yaml --------------------------------------------------------

#[test]
fn a_block_packages_list_is_read_with_quotes_and_comments() {
    let text = "# the workspace\npackages:\n  # apps first\n  - 'packages/*'\n  - \"apps/*\"\n\n  - tools/x # a comment\n  - 'a # b'\n  -\n";
    assert_eq!(pnpm_workspace_patterns(text), strings(&["packages/*", "apps/*", "tools/x", "a # b"]));
}

#[test]
fn a_flow_packages_list_is_read_as_well() {
    assert_eq!(
        pnpm_workspace_patterns("packages: [packages/*, 'apps/*', \"tools/**\"]\n"),
        strings(&["packages/*", "apps/*", "tools/**"])
    );
}

#[test]
fn pnpm_keys_other_than_packages_are_ignored() {
    let text = "catalog:\n  - nope\npackages:\n  - a/*\nonlyBuiltDependencies:\n  - esbuild\n";
    assert_eq!(pnpm_workspace_patterns(text), strings(&["a/*"]));
}

#[test]
fn crlf_line_ends_and_tab_indents_are_read() {
    assert_eq!(pnpm_workspace_patterns("packages:\r\n\t- a/*\r\n  - b/*\r\n"), strings(&["a/*", "b/*"]));
}

#[test]
fn a_yaml_scalar_is_unquoted_or_cut_at_a_spaced_hash() {
    assert_eq!(scalar_value("'a'"), "a");
    assert_eq!(scalar_value("\"a\""), "a");
    assert_eq!(scalar_value("a # comment"), "a");
    assert_eq!(scalar_value("a#b"), "a#b");
    assert_eq!(scalar_value("'a # b'"), "a # b");
    assert_eq!(scalar_value("'"), "'");
}

#[test]
fn pnpm_workspace_globs_are_expanded_to_the_packages_they_name() {
    let fx = Fixture::new(&[
        ("pnpm-workspace.yaml", "packages:\n  - 'packages/*'\n  - apps/* # apps\n"),
        ("app.ts", SRC),
        ("packages/a/package.json", r#"{"name":"a"}"#),
        ("packages/a/src/index.ts", SRC),
        ("apps/web/package.json", r#"{"name":"web"}"#),
        ("apps/web/index.ts", SRC),
        ("other/x/package.json", r#"{"name":"x"}"#),
        ("other/x/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(project.packages.keys().map(String::as_str).collect::<Vec<_>>(), vec!["a", "web"]);
    assert_eq!(resolve(&project, "a", "app.ts"), some("packages/a/src/index.ts"));
    assert_eq!(resolve(&project, "web", "app.ts"), some("apps/web/index.ts"));
    assert_eq!(resolve(&project, "x", "app.ts"), None);
}

#[test]
fn a_pnpm_workspace_yml_is_read_when_there_is_no_yaml() {
    let fx = Fixture::new(&[
        ("pnpm-workspace.yml", "packages: [packages/*]\n"),
        ("app.ts", SRC),
        ("packages/a/package.json", r#"{"name":"a"}"#),
        ("packages/a/index.ts", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "a", "app.ts"), some("packages/a/index.ts"));
}

#[test]
fn a_pnpm_workspace_yaml_wins_over_a_yml() {
    let fx = Fixture::new(&[
        ("pnpm-workspace.yaml", "packages: [a/*]\n"),
        ("pnpm-workspace.yml", "packages: [b/*]\n"),
        ("a/one/package.json", r#"{"name":"one"}"#),
        ("a/one/index.ts", SRC),
        ("b/two/package.json", r#"{"name":"two"}"#),
        ("b/two/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(project.packages.keys().map(String::as_str).collect::<Vec<_>>(), vec!["one"]);
}

// --- workspace patterns and packages ----------------------------------------------

#[test]
fn root_workspaces_are_read_as_an_array_or_as_yarns_object_form() {
    let array = json(r#"{"workspaces":["packages/*"," apps/* ","",5]}"#);
    assert_eq!(workspace_patterns(None, Some(&array)), strings(&["packages/*", "apps/*"]));
    let object = json(r#"{"workspaces":{"packages":["x/*"],"nohoist":["**/y"]}}"#);
    assert_eq!(workspace_patterns(None, Some(&object)), strings(&["x/*"]));
    assert_eq!(workspace_patterns(None, Some(&json(r#"{"name":"p"}"#))), Vec::<String>::new());
    assert_eq!(workspace_patterns(None, None), Vec::<String>::new());
}

#[test]
fn pnpm_patterns_come_first_then_the_root_workspaces() {
    let manifest = json(r#"{"workspaces":["w/*"]}"#);
    assert_eq!(workspace_patterns(Some("packages:\n  - p/*\n"), Some(&manifest)), strings(&["p/*", "w/*"]));
}

#[test]
fn the_root_package_json_workspaces_field_is_read_from_disk() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"name":"root","workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
        ("packages/math/src/index.ts", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "@acme/math", "app.ts"), some("packages/math/src/index.ts"));
}

#[test]
fn yarns_object_form_of_workspaces_is_read_from_disk() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":{"packages":["packages/*"]}}"#),
        ("app.ts", SRC),
        ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
        ("packages/math/src/index.ts", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "@acme/math", "app.ts"), some("packages/math/src/index.ts"));
}

#[test]
fn a_negated_pattern_removes_a_package_the_globs_had_picked_up() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*","!packages/legacy","!packages/old-*"]}"#),
        ("packages/a/package.json", r#"{"name":"a"}"#),
        ("packages/a/index.ts", SRC),
        ("packages/legacy/package.json", r#"{"name":"legacy"}"#),
        ("packages/legacy/index.ts", SRC),
        ("packages/old-x/package.json", r#"{"name":"old-x"}"#),
        ("packages/old-x/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(project.packages.keys().map(String::as_str).collect::<Vec<_>>(), vec!["a"]);
}

#[test]
fn on_a_duplicate_name_the_first_package_found_wins() {
    let fx = Fixture::new(&[
        ("pnpm-workspace.yaml", "packages:\n  - b/*\n"),
        ("package.json", r#"{"workspaces":["a/*"]}"#),
        ("app.ts", SRC),
        ("a/x/package.json", r#"{"name":"dup"}"#),
        ("a/x/index.ts", SRC),
        ("b/z/package.json", r#"{"name":"dup"}"#),
        ("b/z/index.ts", SRC),
        ("b/y/package.json", r#"{"name":"dup"}"#),
        ("b/y/index.ts", SRC),
    ]);
    // pnpm's patterns first, and within one pattern sorted matches.
    assert_eq!(resolve(&fx.load(), "dup", "app.ts"), some("b/y/index.ts"));
}

#[test]
fn a_malformed_missing_or_nameless_manifest_is_skipped_rather_than_thrown_on() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("packages/bad/package.json", "{ not json"),
        ("packages/bad/index.ts", SRC),
        ("packages/nameless/package.json", r#"{"version":"1.0.0"}"#),
        ("packages/nameless/index.ts", SRC),
        ("packages/numeric/package.json", r#"{"name":5}"#),
        ("packages/numeric/index.ts", SRC),
        ("packages/missing/index.ts", SRC),
        ("packages/good/package.json", r#"{"name":"good"}"#),
        ("packages/good/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(project.packages.keys().map(String::as_str).collect::<Vec<_>>(), vec!["good"]);
    assert!(has_note(&project, &["packages/bad/package.json"]), "{:?}", project.notes);
}

#[test]
fn a_package_directory_with_no_indexed_file_is_not_a_package() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("packages/empty/package.json", r#"{"name":"empty"}"#),
        ("packages/empty/README.md", "# empty"),
        ("packages/full/package.json", r#"{"name":"full"}"#),
        ("packages/full/index.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(project.packages.keys().map(String::as_str).collect::<Vec<_>>(), vec!["full"]);
}

#[test]
fn node_modules_is_never_a_workspace_package() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*","node_modules/*","**"]}"#),
        ("app.ts", SRC),
        ("node_modules/vendored/package.json", r#"{"name":"vendored"}"#),
        ("node_modules/vendored/index.ts", SRC),
        ("packages/a/package.json", r#"{"name":"a"}"#),
        ("packages/a/index.ts", SRC),
    ]);
    let project = fx.load();
    assert!(!project.packages.contains_key("vendored"));
    assert_eq!(resolve(&project, "vendored", "app.ts"), None);
    assert_eq!(resolve(&project, "a", "app.ts"), some("packages/a/index.ts"));
}

fn tree(files: &[&str]) -> DirectoryTree {
    DirectoryTree::from_files(files.iter().copied())
}

#[test]
fn a_star_glob_matches_child_directories_holding_indexed_files_but_no_dot_or_node_modules() {
    let tree = tree(&[
        "packages/b/index.ts",
        "packages/a/src/index.ts",
        "packages/.cache/x.ts",
        "packages/node_modules/y/index.ts",
        "other/z.ts",
    ]);
    assert_eq!(expand_pattern("packages/*", &tree), strings(&["packages/a", "packages/b"]));
    assert_eq!(expand_pattern("./packages/*", &tree), strings(&["packages/a", "packages/b"]));
    assert_eq!(expand_pattern("packages/*/src", &tree), strings(&["packages/a/src"]));
}

#[test]
fn a_question_mark_glob_matches_exactly_one_character() {
    let tree = tree(&["pkg1/a.ts", "pkg2/a.ts", "pkg10/a.ts", "pkg/a.ts"]);
    assert_eq!(expand_pattern("pkg?", &tree), strings(&["pkg1", "pkg2"]));
}

#[test]
fn a_double_star_matches_the_start_and_every_directory_below_it() {
    let tree = tree(&[
        "packages/a/src/index.ts",
        "packages/a/nested/deep/z.ts",
        "packages/b/index.ts",
        "packages/.cache/x.ts",
        "packages/node_modules/y/index.ts",
    ]);
    assert_eq!(
        expand_pattern("packages/**", &tree),
        strings(&[
            "packages",
            "packages/a",
            "packages/a/nested",
            "packages/a/nested/deep",
            "packages/a/src",
            "packages/b",
        ])
    );
    let all = expand_pattern("**", &tree);
    assert!(!all.contains(&String::new()), "the root is never a member: {all:?}");
    assert!(all.contains(&"packages/b".to_string()));
}

#[test]
fn a_double_star_descends_at_most_six_levels() {
    let tree = tree(&["d/1/2/3/4/5/6/7/x.ts"]);
    let matched = expand_pattern("d/**", &tree);
    assert!(matched.contains(&"d/1/2/3/4/5/6".to_string()), "{matched:?}");
    assert!(!matched.contains(&"d/1/2/3/4/5/6/7".to_string()), "{matched:?}");
}

#[test]
fn a_literal_pattern_names_a_directory_only_when_the_tree_holds_it() {
    let tree = tree(&["apps/web/main.ts"]);
    assert_eq!(expand_pattern("apps/web", &tree), strings(&["apps/web"]));
    assert_eq!(expand_pattern("apps/missing", &tree), Vec::<String>::new());
}

#[test]
fn a_pattern_climbing_out_or_naming_the_root_names_nothing() {
    let tree = tree(&["apps/web/main.ts", "x.ts"]);
    for pattern in ["../apps", "apps/../apps", ".", "", "./"] {
        assert_eq!(expand_pattern(pattern, &tree), Vec::<String>::new(), "{pattern:?}");
    }
}

#[test]
fn workspace_packages_apply_negation_and_keep_the_first_of_a_duplicate_name() {
    let tree = tree(&["a/x/i.ts", "b/y/i.ts", "b/old-z/i.ts", "b/nameless/i.ts"]);
    let manifests: BTreeMap<String, Json> = [
        ("a/x", r#"{"name":"dup"}"#),
        ("b/y", r#"{"name":"dup"}"#),
        ("b/old-z", r#"{"name":"old"}"#),
        ("b/nameless", r#"{}"#),
    ]
    .into_iter()
    .map(|(dir, manifest)| (dir.to_string(), json(manifest)))
    .collect();
    let packages = workspace_packages(&strings(&["b/*", "a/*", "!b/old-*"]), &tree, &manifests);
    assert_eq!(packages.keys().map(String::as_str).collect::<Vec<_>>(), vec!["dup"]);
    assert_eq!(packages["dup"].dir, "b/y");
}

// --- tsconfig: which config, and its paths ------------------------------------------

#[test]
fn a_base_url_anchored_wildcard_alias_resolves_to_the_directory_it_names() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"]}}}"#),
        ("app.ts", SRC),
        ("src/utils/index.ts", SRC),
        ("src/helpers.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/utils", "app.ts"), some("src/utils/index.ts"));
    assert_eq!(resolve(&project, "@/helpers.js", "app.ts"), some("src/helpers.ts"));
    assert_eq!(resolve(&project, "@/missing", "app.ts"), None);
}

#[test]
fn an_exact_alias_key_matches_only_that_literal_specifier() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"config":["./src/config.ts"]}}}"#),
        ("app.ts", SRC),
        ("src/config.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "config", "app.ts"), some("src/config.ts"));
    assert_eq!(resolve(&project, "config/x", "app.ts"), None);
    assert_eq!(resolve(&project, "configs", "app.ts"), None);
}

#[test]
fn a_keys_targets_are_tried_in_declaration_order() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./first/*","./second/*"]}}}"#),
        ("app.ts", SRC),
        ("first/a.ts", SRC),
        ("second/a.ts", SRC),
        ("second/b.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("first/a.ts"));
    assert_eq!(resolve(&project, "@/b", "app.ts"), some("second/b.ts"));
}

#[test]
fn every_matching_paths_key_contributes_in_declaration_order() {
    let fx = Fixture::new(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"@lib/*":["./lib/*"],"@lib/special":["./special.ts"]}}}"#,
        ),
        ("app.ts", SRC),
        ("special.ts", SRC),
        ("lib/x.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@lib/special", "app.ts"), some("special.ts"));
    assert_eq!(resolve(&project, "@lib/x", "app.ts"), some("lib/x.ts"));
}

#[test]
fn expand_paths_candidates_unions_matching_keys_without_repeats() {
    let config = EffectiveConfig {
        resolve_dir: "apps/web".to_string(),
        paths: vec![
            PathsEntry { pattern: "@/*".to_string(), targets: strings(&["./src/*", "./gen/*"]) },
            PathsEntry { pattern: "@/x".to_string(), targets: strings(&["./special/x.ts", "./src/x"]) },
            PathsEntry { pattern: "@/xy".to_string(), targets: strings(&["./never.ts"]) },
            PathsEntry { pattern: "@/*".to_string(), targets: strings(&["../../../out/*", "/abs/*"]) },
        ],
    };
    assert_eq!(
        expand_paths_candidates(&config, "@/x"),
        strings(&["apps/web/src/x", "apps/web/gen/x", "apps/web/special/x.ts"])
    );
    assert_eq!(expand_paths_candidates(&config, "other"), Vec::<String>::new());
}

#[test]
fn without_a_base_url_targets_resolve_against_the_configs_own_directory() {
    let fx = Fixture::new(&[
        ("apps/web/tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#),
        ("apps/web/page.ts", SRC),
        ("apps/web/src/x.ts", SRC),
        ("src/x.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/x", "apps/web/page.ts"), some("apps/web/src/x.ts"));
    assert_eq!(project.tsconfig_for("apps/web").map(|config| config.resolve_dir.as_str()), Some("apps/web"));
}

#[test]
fn a_tsconfig_beats_a_jsconfig_in_the_same_directory() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./ts/*"]}}}"#),
        ("jsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./js/*"]}}}"#),
        ("app.ts", SRC),
        ("ts/a.ts", SRC),
        ("js/a.ts", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "@/a", "app.ts"), some("ts/a.ts"));
}

#[test]
fn a_jsconfig_is_read_the_same_way_when_there_is_no_tsconfig() {
    let fx = Fixture::new(&[
        (
            "jsconfig.json",
            "{\n  // a jsconfig\n  \"compilerOptions\": {\"paths\": {\"@/*\": [\"./js/*\"]}},\n}\n",
        ),
        ("app.js", SRC),
        ("js/a.js", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "@/a", "app.js"), some("js/a.js"));
}

#[test]
fn the_nearest_config_wins_over_a_more_distant_one() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./root/*"]}}}"#),
        ("app.ts", SRC),
        ("root/a.ts", SRC),
        ("apps/web/tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./own/*"]}}}"#),
        ("apps/web/src/page.ts", SRC),
        ("apps/web/own/a.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("root/a.ts"));
    assert_eq!(resolve(&project, "@/a", "apps/web/src/page.ts"), some("apps/web/own/a.ts"));
}

#[test]
fn the_nearest_config_wins_even_when_it_declares_no_paths() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./root/*"]}}}"#),
        ("app.ts", SRC),
        ("root/a.ts", SRC),
        ("apps/web/tsconfig.json", r#"{"compilerOptions":{"strict":true}}"#),
        ("apps/web/src/page.ts", SRC),
    ]);
    let project = fx.load();
    assert!(project.tsconfig_by_dir.contains_key("apps/web"));
    assert!(project.tsconfig_for("apps/web/src").is_none());
    assert_eq!(resolve(&project, "@/a", "apps/web/src/page.ts"), None);
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("root/a.ts"));
}

#[test]
fn an_alias_target_climbing_out_of_the_project_is_not_offered() {
    let fx = Fixture::new(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"paths":{"@/*":["../outside/*","/etc/*","./src/*"],"evil":["../../etc/passwd"]}}}"#,
        ),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("src/a.ts"));
    assert_eq!(resolve(&project, "evil", "app.ts"), None);
}

// --- tsconfig: extends ------------------------------------------------------------

#[test]
fn a_config_with_no_paths_of_its_own_inherits_the_ones_it_extends() {
    let fx = Fixture::new(&[
        ("configs/base.json", r#"{"compilerOptions":{"paths":{"@/*":["../src/*"]}}}"#),
        ("tsconfig.json", r#"{"extends":"./configs/base.json"}"#),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("src/a.ts"));
    assert_eq!(project.tsconfig_for("").map(|config| config.resolve_dir.as_str()), Some("configs"));
}

#[test]
fn an_extends_entry_without_json_or_naming_a_directory_is_found() {
    let fx = Fixture::new(&[
        ("configs/base.json", r#"{"compilerOptions":{"paths":{"@a/*":["../a/*"]}}}"#),
        ("shared/tsconfig.json", r#"{"compilerOptions":{"paths":{"@b/*":["../b/*"]}}}"#),
        ("one/tsconfig.json", r#"{"extends":"../configs/base"}"#),
        ("one/x.ts", SRC),
        ("two/tsconfig.json", r#"{"extends":"../shared"}"#),
        ("two/x.ts", SRC),
        ("a/m.ts", SRC),
        ("b/m.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@a/m", "one/x.ts"), some("a/m.ts"));
    assert_eq!(resolve(&project, "@b/m", "two/x.ts"), some("b/m.ts"));
}

#[test]
fn a_later_extends_entry_overrides_an_earlier_one() {
    let fx = Fixture::new(&[
        ("first.json", r#"{"compilerOptions":{"paths":{"@/*":["./first/*"]}}}"#),
        ("second.json", r#"{"compilerOptions":{"paths":{"@/*":["./second/*"]}}}"#),
        ("nopaths.json", r#"{"compilerOptions":{"strict":true}}"#),
        ("tsconfig.json", r#"{"extends":["./first.json","./second.json","./nopaths.json"]}"#),
        ("app.ts", SRC),
        ("first/a.ts", SRC),
        ("second/a.ts", SRC),
    ]);
    // A later entry with no paths does not clear what an earlier one gave.
    assert_eq!(resolve(&fx.load(), "@/a", "app.ts"), some("second/a.ts"));
}

#[test]
fn a_configs_own_paths_replace_an_inherited_map_whole() {
    let fx = Fixture::new(&[
        ("base.json", r#"{"compilerOptions":{"paths":{"@a/*":["./base-a/*"],"@b/*":["./base-b/*"]}}}"#),
        ("tsconfig.json", r#"{"extends":"./base.json","compilerOptions":{"paths":{"@a/*":["./own-a/*"]}}}"#),
        ("app.ts", SRC),
        ("base-a/x.ts", SRC),
        ("base-b/x.ts", SRC),
        ("own-a/x.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@a/x", "app.ts"), some("own-a/x.ts"));
    assert_eq!(resolve(&project, "@b/x", "app.ts"), None);
}

#[test]
fn a_base_url_without_paths_of_its_own_leaves_an_inherited_maps_directory_alone() {
    let fx = Fixture::new(&[
        ("configs/base.json", r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#),
        ("tsconfig.json", r#"{"extends":"./configs/base.json","compilerOptions":{"baseUrl":"./other"}}"#),
        ("app.ts", SRC),
        ("configs/src/x.ts", SRC),
        ("other/src/x.ts", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "@/x", "app.ts"), some("configs/src/x.ts"));
}

#[test]
fn inherited_paths_keep_the_base_url_of_the_config_that_declares_them() {
    let fx = Fixture::new(&[
        ("configs/base.json", r#"{"compilerOptions":{"baseUrl":"../src","paths":{"@/*":["*"]}}}"#),
        ("apps/web/tsconfig.json", r#"{"extends":"../../configs/base.json"}"#),
        ("apps/web/page.ts", SRC),
        ("apps/web/x.ts", SRC),
        ("src/x.ts", SRC),
    ]);
    assert_eq!(resolve(&fx.load(), "@/x", "apps/web/page.ts"), some("src/x.ts"));
}

#[test]
fn a_cyclic_extends_chain_neither_hangs_nor_fails() {
    let fx = Fixture::new(&[
        ("a.json", r#"{"extends":"./b.json","compilerOptions":{"paths":{"@a/*":["./a/*"]}}}"#),
        ("b.json", r#"{"extends":"./a.json"}"#),
        ("tsconfig.json", r#"{"extends":"./a.json"}"#),
        (
            "self/tsconfig.json",
            r#"{"extends":"./tsconfig.json","compilerOptions":{"paths":{"@s/*":["./s/*"]}}}"#,
        ),
        ("app.ts", SRC),
        ("a/x.ts", SRC),
        ("self/page.ts", SRC),
        ("self/s/x.ts", SRC),
    ]);
    let project = fx.load();
    assert!(has_note(&project, &["cyclic"]), "{:?}", project.notes);
    assert_eq!(resolve(&project, "@a/x", "app.ts"), some("a/x.ts"));
    assert_eq!(resolve(&project, "@s/x", "self/page.ts"), some("self/s/x.ts"));
}

#[test]
fn a_package_named_extends_is_looked_up_under_node_modules_up_the_tree() {
    let fx = Fixture::new(&[
        (
            "node_modules/@acme/tsconfig/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@/*":["src/*"]}}}"#,
        ),
        (
            "node_modules/shared-config/strict.json",
            r#"{"compilerOptions":{"baseUrl":"../..","paths":{"~/*":["lib/*"]}}}"#,
        ),
        ("apps/web/tsconfig.json", r#"{"extends":"@acme/tsconfig"}"#),
        ("apps/web/page.ts", SRC),
        ("apps/api/tsconfig.json", r#"{"extends":"shared-config/strict"}"#),
        ("apps/api/main.ts", SRC),
        ("src/x.ts", SRC),
        ("lib/y.ts", SRC),
    ]);
    let project = fx.load();
    assert_eq!(resolve(&project, "@/x", "apps/web/page.ts"), some("src/x.ts"));
    assert_eq!(resolve(&project, "~/y", "apps/api/main.ts"), some("lib/y.ts"));
}

#[test]
fn a_missing_extends_target_is_a_note_and_the_configs_own_paths_still_apply() {
    let fx = Fixture::new(&[
        (
            "tsconfig.json",
            r#"{"extends":["./nope.json","@missing/pkg"],"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#,
        ),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert!(has_note(&project, &["tsconfig.json", "nope.json"]), "{:?}", project.notes);
    assert!(has_note(&project, &["tsconfig.json", "@missing/pkg"]), "{:?}", project.notes);
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("src/a.ts"));
}

#[test]
fn extends_candidates_follow_tscs_lookup() {
    assert_eq!(
        extends_candidates("apps/web/tsconfig.json", "./base"),
        strings(&["apps/web/base.json", "apps/web/base/tsconfig.json"])
    );
    assert_eq!(
        extends_candidates("apps/web/tsconfig.json", "../../configs/base.json"),
        strings(&["configs/base.json"])
    );
    assert_eq!(
        extends_candidates("apps/web/tsconfig.json", "@tsconfig/node18/tsconfig.json"),
        strings(&[
            "apps/web/node_modules/@tsconfig/node18/tsconfig.json",
            "apps/node_modules/@tsconfig/node18/tsconfig.json",
            "node_modules/@tsconfig/node18/tsconfig.json",
        ])
    );
    assert_eq!(
        extends_candidates("tsconfig.json", "@tsconfig/strictest"),
        strings(&["node_modules/@tsconfig/strictest.json", "node_modules/@tsconfig/strictest/tsconfig.json"])
    );
    for entry in ["/etc/base.json", "../outside.json", "", ".hidden"] {
        assert_eq!(extends_candidates("tsconfig.json", entry), Vec::<String>::new(), "{entry:?}");
    }
}

// --- tsconfig: baseUrl ------------------------------------------------------------

#[test]
fn own_resolve_dir_is_the_base_url_or_the_configs_directory() {
    let root = Path::new("/project/root");
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", None, root), some("apps/web"));
    assert_eq!(own_resolve_dir("tsconfig.json", None, root), some(""));
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", Some("."), root), some("apps/web"));
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", Some("./src"), root), some("apps/web/src"));
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", Some("../.."), root), some(""));
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", Some("../../.."), root), None);
    assert_eq!(own_resolve_dir("tsconfig.json", Some("/project/root/src"), root), some("src"));
    assert_eq!(own_resolve_dir("tsconfig.json", Some("/elsewhere/src"), root), None);
}

/// tsc reads `\` as a separator and `/x` or `C:/x` as rooted on every OS, so
/// whether a `baseUrl` is absolute must not depend on the host's `Path` (on
/// Windows a drive-less `/project/root/src` is not `is_absolute()`).
#[test]
fn a_base_urls_separators_and_rootedness_do_not_depend_on_the_host() {
    let root = Path::new("/project/root");
    assert_eq!(own_resolve_dir("tsconfig.json", Some("\\project\\root\\src"), root), some("src"));
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", Some(".\\src"), root), some("apps/web/src"));
    assert_eq!(own_resolve_dir("apps/web/tsconfig.json", Some("..\\..\\lib"), root), some("lib"));
    assert_eq!(own_resolve_dir("tsconfig.json", Some("C:/elsewhere/src"), root), None);
    assert_eq!(own_resolve_dir("tsconfig.json", Some("C:\\elsewhere\\src"), root), None);
}

#[cfg(windows)]
#[test]
fn a_drive_less_base_url_is_on_the_roots_drive() {
    let root = Path::new("C:\\project\\root");
    assert_eq!(own_resolve_dir("tsconfig.json", Some("/project/root/src"), root), some("src"));
    assert_eq!(own_resolve_dir("tsconfig.json", Some("D:/project/root/src"), root), None);
}

#[test]
fn a_base_url_climbing_out_of_the_project_voids_the_configs_own_paths_only() {
    let fx = Fixture::new(&[
        ("base.json", r#"{"compilerOptions":{"paths":{"@base/*":["./base/*"]}}}"#),
        (
            "tsconfig.json",
            r#"{"extends":"./base.json","compilerOptions":{"baseUrl":"../..","paths":{"@own/*":["./src/*"]}}}"#,
        ),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
        ("base/a.ts", SRC),
        (
            "lone/tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":"../../..","paths":{"@own/*":["./src/*"]}}}"#,
        ),
        ("lone/x.ts", SRC),
    ]);
    let project = fx.load();
    assert!(has_note(&project, &["tsconfig.json", "baseUrl"]), "{:?}", project.notes);
    assert_eq!(resolve(&project, "@own/a", "app.ts"), None);
    assert_eq!(resolve(&project, "@base/a", "app.ts"), some("base/a.ts"));
    assert_eq!(resolve(&project, "@own/a", "lone/x.ts"), None);
}

#[test]
fn an_absolute_base_url_inside_the_project_is_made_relative() {
    let fx = Fixture::new(&[("app.ts", SRC), ("src/a.ts", SRC)]);
    let base_url = fx.root.join("src").to_string_lossy().replace('\\', "/");
    fx.write(
        "tsconfig.json",
        &format!(r#"{{"compilerOptions":{{"baseUrl":{base_url:?},"paths":{{"@/*":["*"]}}}}}}"#),
    );
    assert_eq!(resolve(&fx.load(), "@/a", "app.ts"), some("src/a.ts"));
}

#[test]
fn the_resolved_config_is_shared_by_every_directory_it_governs() {
    let fx = Fixture::new(&[
        ("base.json", r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#),
        ("apps/x/tsconfig.json", r#"{"extends":"../../base.json"}"#),
        ("apps/x/a.ts", SRC),
        ("apps/y/tsconfig.json", r#"{"extends":"../../base.json"}"#),
        ("apps/y/a.ts", SRC),
        ("src/m.ts", SRC),
    ]);
    let project = fx.load();
    let x = project.tsconfig_by_dir["apps/x"].as_ref().expect("inherited paths");
    let y = project.tsconfig_by_dir["apps/y"].as_ref().expect("inherited paths");
    assert!(Arc::ptr_eq(x, y));
    assert_eq!(resolve(&project, "@/m", "apps/x/a.ts"), some("src/m.ts"));
    assert_eq!(resolve(&project, "@/m", "apps/y/a.ts"), some("src/m.ts"));
}

// --- tsconfig: JSONC and malformed configs -------------------------------------------

#[test]
fn comments_and_trailing_commas_in_a_tsconfig_are_read_the_same_as_strict_json() {
    let fx = Fixture::new(&[
        (
            "tsconfig.json",
            "{\n  // line comment\n  /* block\n     comment */\n  \"compilerOptions\": {\n    \"paths\": {\n      \"@/*\": [\"./src/*\",], // trailing\n    },\n  },\n}\n",
        ),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert!(project.notes.is_empty(), "{:?}", project.notes);
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("src/a.ts"));
}

#[test]
fn a_malformed_tsconfig_is_a_note_and_still_shadows_its_parent() {
    let fx = Fixture::new(&[
        ("tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
        ("apps/web/tsconfig.json", "{ \"compilerOptions\": "),
        ("apps/web/page.ts", SRC),
        ("apps/arr/tsconfig.json", "[]"),
        ("apps/arr/page.ts", SRC),
    ]);
    let project = fx.load();
    assert!(has_note(&project, &["apps/web/tsconfig.json"]), "{:?}", project.notes);
    assert!(has_note(&project, &["apps/arr/tsconfig.json"]), "{:?}", project.notes);
    assert_eq!(resolve(&project, "@/a", "apps/web/page.ts"), None);
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("src/a.ts"));
}

#[test]
fn a_tsconfig_plugins_entry_is_never_loaded() {
    let fx = Fixture::new(&[
        (
            "tsconfig.json",
            r#"{"compilerOptions":{"plugins":[{"name":"./evil-plugin"},{"transform":"./evil-plugin.js"}],"paths":{"@/*":["./src/*"]}}}"#,
        ),
        ("evil-plugin.js", "require('fs').writeFileSync(__dirname + '/PWNED', 'x');\n"),
        ("evil-plugin/index.js", "require('fs').writeFileSync(__dirname + '/../PWNED', 'x');\n"),
        ("app.ts", SRC),
        ("src/a.ts", SRC),
    ]);
    let project = fx.load();
    assert!(project.notes.is_empty(), "{:?}", project.notes);
    assert!(!fx.root.join("PWNED").exists());
    assert_eq!(resolve(&project, "@/a", "app.ts"), some("src/a.ts"));
}

#[test]
fn malformed_configs_never_fail_the_load() {
    let fx = Fixture::new(&[
        ("package.json", "{\"workspaces\": [\"packages/*\""),
        ("pnpm-workspace.yaml", "packages: [unterminated\n  - ]]\n"),
        ("tsconfig.json", "/* never closed"),
        ("jsconfig.json", "{}"),
        ("a.ts", SRC),
        ("packages/p/package.json", "null"),
        ("packages/p/tsconfig.json", r#"{"extends": 5, "compilerOptions": {"paths": [1, 2], "baseUrl": 7}}"#),
        ("packages/p/index.ts", SRC),
    ]);
    let project = TsProject::load(&fx.root).expect("a bad config is a note, never an error");
    assert!(project.packages.is_empty());
    assert!(has_note(&project, &["package.json"]), "{:?}", project.notes);
    assert!(has_note(&project, &["tsconfig.json"]), "{:?}", project.notes);
    assert_eq!(resolve(&project, "./a", "packages/p/index.ts"), None);
    assert_eq!(resolve(&project, "../../a", "packages/p/index.ts"), some("a.ts"));
}

#[test]
fn an_empty_project_loads_to_an_empty_model() {
    let fx = Fixture::new(&[]);
    let project = fx.load();
    assert!(project.existence.is_empty());
    assert!(project.packages.is_empty());
    assert!(project.notes.is_empty());
}

// --- JSON and JSONC ---------------------------------------------------------------

#[test]
fn strip_jsonc_removes_comments_and_trailing_commas_but_never_string_contents() {
    let text = r#"{
  "url": "http://example.com", // trailing comment
  "block": "/* not a comment */",
  "line": "// not a comment",
  "comma": "a,}",
  "quote": "a\"//b", /* inline */ "list": [1, 2, /* c */],
}"#;
    let parsed = parse_jsonc(text).expect("parses after stripping");
    assert_eq!(parsed.get("url").and_then(Json::as_str), Some("http://example.com"));
    assert_eq!(parsed.get("block").and_then(Json::as_str), Some("/* not a comment */"));
    assert_eq!(parsed.get("line").and_then(Json::as_str), Some("// not a comment"));
    assert_eq!(parsed.get("comma").and_then(Json::as_str), Some("a,}"));
    assert_eq!(parsed.get("quote").and_then(Json::as_str), Some("a\"//b"));
    assert_eq!(parsed.get("list").and_then(Json::as_array).map(<[Json]>::len), Some(2));
    assert_eq!(keys(&parsed), vec!["url", "block", "line", "comma", "quote", "list"]);
}

#[test]
fn a_comment_becomes_whitespace_so_tokens_are_never_glued() {
    assert_eq!(strip_jsonc("1/*x*/2"), "1 2");
    assert_eq!(parse_jsonc("{\"a\": true// c\n}"), Some(json(r#"{"a":true}"#)));
    assert_eq!(parse_jsonc("[1, // c\n]"), Some(json("[1]")));
    assert_eq!(parse_jsonc("{\"a\":1/* c */,}"), Some(json(r#"{"a":1}"#)));
}

#[test]
fn strict_json_rejects_comments_and_trailing_commas() {
    assert_eq!(parse_json("{\"a\": 1 // c\n}"), None);
    assert_eq!(parse_json("{\"a\": 1,}"), None);
    assert!(parse_jsonc("{\"a\": 1,}").is_some());
}

#[test]
fn object_keys_keep_their_source_order() {
    assert_eq!(keys(&json(r#"{"b":1,"a":2,"c":{"z":1,"y":2}}"#)), vec!["b", "a", "c"]);
    assert_eq!(keys(json(r#"{"c":{"z":1,"y":2}}"#).get("c").unwrap()), vec!["z", "y"]);
}

#[test]
fn a_duplicate_key_keeps_its_first_position_and_its_last_value() {
    let parsed = json(r#"{"a":"1","b":"2","a":"3"}"#);
    assert_eq!(keys(&parsed), vec!["a", "b"]);
    assert_eq!(parsed.get("a").and_then(Json::as_str), Some("3"));
}

// --- presence -------------------------------------------------------------------

#[test]
fn a_created_file_resolves_without_touching_the_disk() {
    let fx = Fixture::new(&[("a.ts", SRC)]);
    let mut project = fx.load();
    assert_eq!(resolve(&project, "./b", "a.ts"), None);
    // Never written to disk: only the hook makes it exist.
    project.file_presence_changed(&rel("b.ts"), true);
    assert_eq!(resolve(&project, "./b", "a.ts"), some("b.ts"));
}

#[test]
fn a_deleted_file_stops_resolving_without_touching_the_disk() {
    let fx = Fixture::new(&[("a.ts", SRC), ("b.ts", SRC)]);
    let mut project = fx.load();
    assert_eq!(resolve(&project, "./b", "a.ts"), some("b.ts"));
    // Still on disk: only the hook makes it gone.
    project.file_presence_changed(&rel("b.ts"), false);
    assert_eq!(resolve(&project, "./b", "a.ts"), None);
}

#[test]
fn presence_updates_are_idempotent() {
    let fx = Fixture::new(&[("a.ts", SRC), ("b.ts", SRC)]);
    let mut project = fx.load();
    let loaded: BTreeSet<RelPath> = project.existence.clone();

    project.file_presence_changed(&rel("c.ts"), true);
    project.file_presence_changed(&rel("c.ts"), true);
    assert_eq!(project.existence.len(), loaded.len() + 1);
    project.file_presence_changed(&rel("a.ts"), true);
    assert_eq!(project.existence.len(), loaded.len() + 1);

    project.file_presence_changed(&rel("c.ts"), false);
    project.file_presence_changed(&rel("c.ts"), false);
    project.file_presence_changed(&rel("never.ts"), false);
    assert_eq!(project.existence, loaded);
}

#[test]
fn a_delete_lets_the_next_candidate_take_over() {
    let fx = Fixture::new(&[("a.ts", SRC), ("x.ts", SRC), ("x.js", SRC)]);
    let mut project = fx.load();
    assert_eq!(resolve(&project, "./x.js", "a.ts"), some("x.ts"));
    project.file_presence_changed(&rel("x.ts"), false);
    assert_eq!(resolve(&project, "./x.js", "a.ts"), some("x.js"));
    project.file_presence_changed(&rel("x.ts"), true);
    assert_eq!(resolve(&project, "./x.js", "a.ts"), some("x.ts"));
}

#[test]
fn a_file_created_in_a_new_directory_resolves_through_the_configs_above_it() {
    let fx = Fixture::new(&[
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("packages/math/package.json", r#"{"name":"@acme/math"}"#),
        ("packages/math/src/index.ts", SRC),
        ("apps/web/tsconfig.json", r#"{"compilerOptions":{"paths":{"@/*":["./src/*"]}}}"#),
        ("apps/web/page.ts", SRC),
    ]);
    let mut project = fx.load();
    assert_eq!(resolve(&project, "@acme/math/geometry/area", "app.ts"), None);

    project.file_presence_changed(&rel("packages/math/src/geometry/area.ts"), true);
    project.file_presence_changed(&rel("apps/web/newdir/view.ts"), true);
    project.file_presence_changed(&rel("apps/web/src/fresh.ts"), true);

    assert_eq!(
        resolve(&project, "@acme/math/geometry/area", "app.ts"),
        some("packages/math/src/geometry/area.ts")
    );
    assert_eq!(resolve(&project, "@/fresh", "apps/web/newdir/view.ts"), some("apps/web/src/fresh.ts"));
}

#[test]
fn the_extractors_presence_hook_updates_the_model() {
    let fx = Fixture::new(&[("a.ts", SRC), ("b.ts", SRC)]);
    let extractor = TypeScriptExtractor;
    let mut project = extractor.load_project(&fx.root).unwrap();
    extractor.file_presence_changed(&mut project, &rel("c.ts"), true);
    extractor.file_presence_changed(&mut project, &rel("b.ts"), false);
    assert_eq!(resolve(&project, "./c", "a.ts"), some("c.ts"));
    assert_eq!(resolve(&project, "./b", "a.ts"), None);
}

// --- the conformance fixture ------------------------------------------------------

fn conformance_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/project")
}

#[test]
fn the_existence_set_is_the_sdk_walks_file_list() {
    let root = conformance_root();
    let project = TsProject::load(&root).unwrap();
    let extensions: Vec<String> = GRAMMAR_EXTENSIONS.iter().map(|ext| (*ext).to_string()).collect();
    let exclude: Vec<String> = EXCLUDE_DIRS.iter().map(|dir| (*dir).to_string()).collect();
    let walked: BTreeSet<RelPath> = walk_project(&root, &extensions, &exclude).into_iter().collect();
    assert!(!walked.is_empty());
    assert_eq!(project.existence, walked);
    assert!(project.existence.contains(&rel("src/util.js")));
    assert!(project.existence.contains(&rel("packages/geom/src/point.ts")));
}

#[test]
fn the_conformance_fixture_resolves_its_alias_workspace_and_private_imports() {
    let project = TsProject::load(&conformance_root()).unwrap();
    assert!(project.notes.is_empty(), "{:?}", project.notes);
    assert_eq!(
        resolve(&project, "~geom/point", "packages/app/src/alias.ts"),
        some("packages/geom/src/point.ts")
    );
    assert_eq!(resolve(&project, "@fx/geom", "packages/app/src/root.ts"), some("packages/geom/src/index.ts"));
    assert_eq!(
        resolve(&project, "@fx/geom/point", "packages/app/src/subpath.ts"),
        some("packages/geom/src/point.ts")
    );
    assert_eq!(resolve(&project, "#int/secret", "src/privateUse.ts"), some("src/int/secret.ts"));
}

// --- symlinks -------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_symlinked_workspace_package_resolves_under_its_link_spelling() {
    let fx = Fixture::new(&[
        (".gitignore", "store/\n"),
        ("package.json", r#"{"workspaces":["packages/*"]}"#),
        ("app.ts", SRC),
        ("store/real/package.json", r#"{"name":"linked"}"#),
        ("store/real/src/index.ts", SRC),
    ]);
    fs::create_dir_all(fx.root.join("packages")).unwrap();
    std::os::unix::fs::symlink("../store/real", fx.root.join("packages/linked")).unwrap();
    let project = fx.load();
    assert_eq!(resolve(&project, "linked", "app.ts"), some("packages/linked/src/index.ts"));
}
