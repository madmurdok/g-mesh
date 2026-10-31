//! A TypeScript config save re-extracts only the importers its edit can move,
//! through the real plugin (docs/architecture/gm-509-selective-config-reindex.md).
//!
//! How a re-extract is seen: after the index is built, every `.ts` file is
//! rewritten on disk to declare `mark_v2` instead of `mark_v1`, and none of
//! those edits is routed. A file core re-extracts (or reindexes) reads the
//! disk and so shows `mark_v2`; a file left alone still shows `mark_v1`.
//!
//! The first watch-file save of a fresh index has no stored facts and
//! reindexes the language, whose walk stores them; the save under test is
//! the second one. The manifest is the shipped one with `semantic_pass`
//! off, so no language server is involved.
//!
//! A save of a file in a config's `extends` chain reloads the model too
//! (GM-670): the plugin declares the chain's files with its facts, core
//! stores them and routes their saves. Controls: `route_settled_path` asks
//! only the static matcher (W1 and the rest drop the chain file); the SDK's
//! `resolution_changed` answers no `watch_paths` (W2: a chain changed by an
//! edit goes unwatched); `set_resolution_facts` appends instead of replacing
//! (W4: `a.json` stays stored); the TS reader records only found candidates
//! (W3: the missing `base.json` is not stored).
//!
//! Controls: `resolution_delta = false` in the manifest (each save
//! reindexes: every file shows `mark_v2`); drop the trailer write in the
//! SDK's `bulk_index` (no facts are stored: every save reindexes).

#![cfg(unix)]

use crate::typescript_registry::Harness;

fn harness() -> Harness {
    Harness::with_manifest("typescript", |text| {
        assert!(text.contains("semantic_pass = true"), "the shipped manifest's capability line moved");
        text.replace("semantic_pass = true", "semantic_pass = false")
    })
}

/// `(path, import line)` of every `.ts` file; each also declares its mark.
const FILES: &[(&str, &str)] = &[
    ("src/app/x.ts", ""),
    ("src/app2/x.ts", ""),
    ("src/a.ts", "import { thing } from \"@app/x\";\n"),
    ("src/c.ts", "import { thing } from \"./app/x\";\n"),
    ("src/d.ts", "import React from \"react\";\n"),
    ("deep/f.ts", "import { thing } from \"@app/x\";\n"),
    ("src/n1.ts", ""),
    ("src/n2.ts", ""),
    ("src/n3.ts", ""),
    ("src/n4.ts", ""),
    ("src/n5.ts", ""),
    ("src/n6.ts", ""),
];

const TSCONFIG: &str = r#"{"compilerOptions":{"baseUrl":".","paths":{"@app/*":["src/app/*"]}}}"#;
const PACKAGE_JSON: &str = r#"{"name":"root","version":"1.0.0","dependencies":{"react":"18.0.0"}}"#;

fn source(import: &str, mark: &str) -> String {
    format!("{import}export function {mark}(): number {{\n  return 1;\n}}\n")
}

/// Writes the project, then saves `tsconfig.json` once so the index is built
/// and holds resolution facts.
fn indexed() -> Harness {
    let harness = harness();
    harness.write("tsconfig.json", TSCONFIG);
    harness.write("deep/tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@deep/*":["x/*"]}}}"#);
    harness.write("package.json", PACKAGE_JSON);
    for (path, import) in FILES {
        harness.write(path, &source(import, "mark_v1"));
    }
    harness.route("tsconfig.json");
    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new());
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len(), "the first save indexed every file");
    let facts = harness.conn.with(|c| g_mesh::storage::schema::resolution_facts(c, "typescript")).unwrap();
    assert!(facts.is_some(), "the reindex's walk stored resolution facts");
    assert_eq!(harness.imports("src/a.ts").0, vec!["src/app/x.ts:x.ts".to_string()]);

    for (path, import) in FILES {
        harness.write(path, &source(import, "mark_v2"));
    }
    harness
}

/// The files whose indexed `Function` nodes include `mark`, sorted.
fn marked(mark: &str, harness: &Harness) -> Vec<String> {
    harness.conn.with(|c| {
        let mut statement = c
            .prepare("SELECT DISTINCT filePath FROM nodes WHERE name = ?1 AND kind = 'Function' ORDER BY 1")
            .unwrap();
        statement
            .query_map([mark], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
}

/// A `paths` edit at the root re-extracts exactly the importer of the edited
/// alias under the root: not the relative or bare importers, and not the one
/// under `deep/`, whose own tsconfig shadows the root's. The re-extracted
/// importer then resolves through the new target.
#[test]
fn a_paths_edit_re_extracts_only_the_aliases_importers() {
    let harness = indexed();
    harness.write("tsconfig.json", &TSCONFIG.replace("src/app/*", "src/app2/*"));
    harness.route("tsconfig.json");

    assert_eq!(marked("mark_v2", &harness), vec!["src/a.ts".to_string()], "only src/a.ts was re-extracted");
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len() - 1);
    assert_eq!(
        harness.imports("src/a.ts"),
        (vec!["src/app2/x.ts:x.ts".to_string()], Vec::new()),
        "the re-extracted importer resolves through the edited alias"
    );
}

/// A `package.json` version (and dependency) bump re-extracts nothing and
/// reindexes nothing.
#[test]
fn a_package_json_version_bump_re_extracts_nothing() {
    let harness = indexed();
    harness.write("package.json", &PACKAGE_JSON.replace("1.0.0", "1.0.1").replace("18.0.0", "18.2.0"));
    harness.route("package.json");

    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new(), "no file was re-extracted");
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len());
}

/// GM-546: the paths swap routed through a whole-language reindex instead of
/// a re-extract. Only three files, all written once and never edited, so the
/// importer's `File` node is unchanged across the two walks; its import edge
/// still moves onto the new target, which existed already. Without
/// `resolution_delta`, and with it but the answer above the threshold
/// (`src/a.ts` is 1 of 3 files, more than 30%).
///
/// A whole reindex is told apart from a re-extract by `src/app/x.ts`,
/// rewritten to `mark_v2` without being routed: only a walk reads it.
///
/// Control: revert `language_swap::plan_attached` to the pre-GM-546
/// predicates (both `dropped` uses back to the target-only rule) ->
/// `src/a.ts` still imports `src/app/x.ts`.
#[test]
fn a_whole_language_paths_reindex_moves_an_unchanged_importers_edge() {
    // Each arm's imports, compared once both ran, so a control fails both.
    let mut arms = Vec::new();
    for resolution_delta in [false, true] {
        let harness = Harness::with_manifest("typescript", |text| {
            assert!(text.contains("resolution_delta = true"), "the shipped manifest's capability line moved");
            let text = text.replace("semantic_pass = true", "semantic_pass = false");
            if resolution_delta {
                text
            } else {
                text.replace("resolution_delta = true", "resolution_delta = false")
            }
        });
        harness.write("tsconfig.json", TSCONFIG);
        harness.write("package.json", PACKAGE_JSON);
        for (path, import) in &FILES[..3] {
            harness.write(path, &source(import, "mark_v1"));
        }
        harness.route("tsconfig.json");
        assert_eq!(harness.imports("src/a.ts").0, vec!["src/app/x.ts:x.ts".to_string()]);

        harness.write("src/app/x.ts", &source("", "mark_v2"));
        harness.write("tsconfig.json", &TSCONFIG.replace("src/app/*", "src/app2/*"));
        harness.route("tsconfig.json");

        assert_eq!(
            marked("mark_v2", &harness),
            vec!["src/app/x.ts".to_string()],
            "resolution_delta {resolution_delta}: the language was reindexed whole"
        );
        arms.push((resolution_delta, harness.imports("src/a.ts")));
    }
    let moved = (vec!["src/app2/x.ts:x.ts".to_string()], Vec::new());
    assert_eq!(
        arms,
        vec![(false, moved.clone()), (true, moved)],
        "the unchanged importer resolves through the edited alias"
    );
}

// --- GM-670: the files of an `extends` chain ------------------------------------

fn alias_to(dir: &str) -> String {
    format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"@app/*":["src/{dir}/*"]}}}}}}"#)
}

fn extends(entry: &str) -> String {
    format!(r#"{{"extends":"{entry}"}}"#)
}

fn stored_watch_paths(harness: &Harness) -> Vec<String> {
    harness.conn.with(|c| g_mesh::storage::schema::watch_paths(c, "typescript")).unwrap()
}

/// Writes `configs` and `extra` beside the usual files, saves the root
/// `tsconfig.json` once so the index is built with its facts and watch paths,
/// then rewrites every `.ts` file to `mark_v2` (unrouted).
fn chained(harness: Harness, configs: &[(&str, &str)], extra: &[(&str, &str)]) -> Harness {
    harness.write("package.json", PACKAGE_JSON);
    harness.write("deep/tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@deep/*":["x/*"]}}}"#);
    for (path, contents) in configs {
        harness.write(path, contents);
    }
    let files: Vec<(&str, &str)> = FILES.iter().chain(extra).copied().collect();
    for (path, import) in &files {
        harness.write(path, &source(import, "mark_v1"));
    }
    harness.route("tsconfig.json");
    assert_eq!(marked("mark_v1", &harness).len(), files.len(), "the first save indexed every file");
    for (path, import) in &files {
        harness.write(path, &source(import, "mark_v2"));
    }
    harness
}

fn only_a_re_extracted(harness: &Harness) {
    assert_eq!(marked("mark_v2", harness), vec!["src/a.ts".to_string()], "only src/a.ts was re-extracted");
}

const APP_X: &str = "src/app/x.ts:x.ts";
const APP2_X: &str = "src/app2/x.ts:x.ts";

/// W1: the child extends `./base.json`; an edit of the base's `paths`
/// re-points the alias importer and re-extracts only it.
///
/// Control (C1): `route_settled_path` uses the static matcher only - the
/// save of `base.json` is dropped and nothing moves.
#[test]
fn a_base_config_edit_re_points_the_alias_importer() {
    let harness = chained(
        harness(),
        &[("tsconfig.json", &extends("./base.json")), ("base.json", &alias_to("app"))],
        &[],
    );
    assert_eq!(harness.imports("src/a.ts").0, vec![APP_X.to_string()]);
    assert!(stored_watch_paths(&harness).contains(&"base.json".to_string()));

    harness.write("base.json", &alias_to("app2"));
    harness.route("base.json");

    only_a_re_extracted(&harness);
    assert_eq!(harness.imports("src/a.ts"), (vec![APP2_X.to_string()], Vec::new()));
}

/// W2: the chain child -> `a.json` -> `b.json` is built by an edit (the bulk
/// walk stored child -> `a.json` only), then `b.json` is edited. The first
/// edit changes nothing resolvable; the second must still reach the model.
///
/// Control (C4): the SDK's `resolution_changed` answers no `watch_paths` -
/// `b.json` is never stored and its edit is dropped.
#[test]
fn a_chain_grown_by_an_edit_is_watched_to_its_end() {
    let harness = chained(
        harness(),
        &[
            ("tsconfig.json", &extends("./a.json")),
            ("a.json", &alias_to("app")),
            ("b.json", &alias_to("app")),
        ],
        &[],
    );
    harness.write("a.json", &extends("./b.json"));
    harness.route("a.json");
    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new(), "the effective paths did not move");
    assert!(
        stored_watch_paths(&harness).contains(&"b.json".to_string()),
        "{:?}",
        stored_watch_paths(&harness)
    );

    harness.write("b.json", &alias_to("app2"));
    harness.route("b.json");

    only_a_re_extracted(&harness);
    assert_eq!(harness.imports("src/a.ts"), (vec![APP2_X.to_string()], Vec::new()));
}

/// W3: `./base.json` is missing when the model loads, then created: the
/// child inherits it.
///
/// Control (C5): the TS reader records only found candidates - `base.json`
/// is not stored and its creation is dropped.
#[test]
fn a_missing_base_config_that_is_created_later_is_inherited() {
    let harness = chained(harness(), &[("tsconfig.json", &extends("./base.json"))], &[]);
    assert!(
        stored_watch_paths(&harness).contains(&"base.json".to_string()),
        "the missing target is declared"
    );
    assert!(harness.imports("src/a.ts").0.is_empty(), "no alias yet");

    harness.write("base.json", &alias_to("app"));
    harness.route("base.json");

    only_a_re_extracted(&harness);
    assert_eq!(harness.imports("src/a.ts"), (vec![APP_X.to_string()], Vec::new()));
}

/// W4: the child switches its `extends` from `./a.json` to `./b.json`: it
/// inherits `b.json` at once, and `a.json` is no longer a stored watch path.
///
/// Control (C2): `set_resolution_facts` appends instead of replacing -
/// `a.json` stays stored.
#[test]
fn a_chain_file_that_left_the_chain_is_no_longer_stored() {
    let harness = chained(
        harness(),
        &[
            ("tsconfig.json", &extends("./a.json")),
            ("a.json", &alias_to("app")),
            ("b.json", &alias_to("app2")),
        ],
        &[],
    );
    assert_eq!(stored_watch_paths(&harness), vec!["a.json".to_string()]);

    harness.write("tsconfig.json", &extends("./b.json"));
    harness.route("tsconfig.json");
    only_a_re_extracted(&harness);
    assert_eq!(harness.imports("src/a.ts"), (vec![APP2_X.to_string()], Vec::new()));
    assert_eq!(stored_watch_paths(&harness), vec!["b.json".to_string()], "a.json left the chain");

    harness.write("a.json", &alias_to("n1"));
    harness.route("a.json");
    assert_eq!(stored_watch_paths(&harness), vec!["b.json".to_string()]);
}

/// W5: `extends: "./base"` names an exact file with no `.json`; editing it
/// re-points.
#[test]
fn an_exact_non_json_base_is_watched_by_its_exact_path() {
    let harness =
        chained(harness(), &[("tsconfig.json", &extends("./base")), ("base", &alias_to("app"))], &[]);
    assert_eq!(harness.imports("src/a.ts").0, vec![APP_X.to_string()]);

    harness.write("base", &alias_to("app2"));
    harness.route("base");

    only_a_re_extracted(&harness);
    assert_eq!(harness.imports("src/a.ts"), (vec![APP2_X.to_string()], Vec::new()));
}

/// W6: an absolute `extends` inside the project root; editing its target
/// re-points.
///
/// Control (C9): absolute entries are dropped - the alias never resolves.
#[test]
fn an_absolute_extends_inside_the_root_is_followed_and_watched() {
    let harness = harness();
    let absolute = format!("{}/cfg/base.json", harness.root().display());
    let base = |dir: &str| {
        format!(r#"{{"compilerOptions":{{"baseUrl":"..","paths":{{"@app/*":["src/{dir}/*"]}}}}}}"#)
    };
    let harness =
        chained(harness, &[("tsconfig.json", &extends(&absolute)), ("cfg/base.json", &base("app"))], &[]);
    assert_eq!(harness.imports("src/a.ts").0, vec![APP_X.to_string()]);

    harness.write("cfg/base.json", &base("app2"));
    harness.route("cfg/base.json");

    only_a_re_extracted(&harness);
    assert_eq!(harness.imports("src/a.ts"), (vec![APP2_X.to_string()], Vec::new()));
}

/// W7: `@/x` waits (its alias points at a directory with no `x`); a base
/// edit maps `@/*` to `lib/*`, where `lib/x.ts` exists, and the import links.
#[test]
fn a_base_edit_links_a_waiting_alias_import() {
    let alias =
        |dir: &str| format!(r#"{{"compilerOptions":{{"baseUrl":".","paths":{{"@/*":["{dir}/*"]}}}}}}"#);
    let harness = chained(
        harness(),
        &[("tsconfig.json", &extends("./base.json")), ("base.json", &alias("missing"))],
        &[("lib/x.ts", ""), ("src/w.ts", "import { thing } from \"@/x\";\n")],
    );
    assert!(harness.imports("src/w.ts").0.is_empty(), "@/x waits");

    harness.write("base.json", &alias("lib"));
    harness.route("base.json");

    assert_eq!(marked("mark_v2", &harness), vec!["src/w.ts".to_string()], "only the waiting importer");
    assert_eq!(harness.imports("src/w.ts"), (vec!["lib/x.ts:x.ts".to_string()], Vec::new()));
}
