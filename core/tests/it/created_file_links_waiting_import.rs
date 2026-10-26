//! A relative TS import whose file does not exist yet links once that file
//! is created, by re-extracting only its importer, through the real
//! TypeScript plugin: `./missing` once `src/missing.ts` or
//! `src/missing/index.ts` appears, `./m.js` re-pointed from `src/m.js` to a
//! created `src/m.ts`, `./missing` re-pointed from `src/missing/index.ts`
//! and `./m` from `src/m.tsx` to a created higher-priority candidate, while
//! an import naming the lower candidate explicitly stays on it, a split
//! literal the extractor folds to `./missing`, after a plugin restart, and
//! `..` naming the project root once a root `index.ts` appears (and back to
//! `index.tsx` once it is deleted).
//!
//! The plugin answers the created file's first `fileChanged` with the
//! addresses that file answers, plus the candidate each address resolved to
//! before; core looks up the importers stored as pointing at them and
//! re-extracts those.
//!
//! How a re-extract is seen: after the index is built every `.ts` file is
//! rewritten on disk to declare `mark_v2` instead of `mark_v1`, and none of
//! those edits is routed. A re-extracted (or reindexed) file shows `mark_v2`.
//! Six filler files keep the one importer under core's 30% whole-language
//! fallback, which would link the import too.
//!
//! Controls: `TsProject::source_changed` returns `None` (the import stays
//! where it was); the linked half of `indexed_importers`' File arm deleted
//! (`./m.js` stays on `src/m.js`); the `candidates_shadowed_by` keys dropped
//! from `TsProject::source_changed` (`./missing` stays on
//! `src/missing/index.ts`, `./m` on `src/m.tsx`, and the explicit importers
//! are not re-extracted).

#![cfg(unix)]

use crate::typescript_registry::Harness;

const FILLERS: [&str; 6] = ["src/n1.ts", "src/n2.ts", "src/n3.ts", "src/n4.ts", "src/n5.ts", "src/n6.ts"];

fn source(import: &str, mark: &str) -> String {
    format!("{import}export function {mark}() {{\n  return 1;\n}}\n")
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

/// Indexes the fillers, `extra` and `src/a.ts` (written with `import`), one
/// route each, then rewrites every one of them to `mark_v2` unrouted.
fn indexed(import: &str, extra: &[&str]) -> Harness {
    let harness = Harness::new();
    let mut files: Vec<(&str, &str)> = FILLERS.iter().map(|path| (*path, "")).collect();
    files.extend(extra.iter().map(|path| (*path, "")));
    files.push(("src/a.ts", import));
    for (path, import) in &files {
        harness.write(path, &source(import, "mark_v1"));
        harness.route(path);
    }
    assert_eq!(marked("mark_v1", &harness).len(), files.len(), "every file indexed");
    for (path, import) in &files {
        harness.write(path, &source(import, "mark_v2"));
    }
    harness
}

/// Creates `path` and routes it, as the watcher does a created file.
fn create(harness: &Harness, path: &str) {
    harness.write(path, "export const created = 1;\n");
    harness.route(path);
}

fn assert_only_the_importer_re_extracted(harness: &Harness) {
    assert_eq!(
        marked("mark_v2", harness),
        vec!["src/a.ts".to_string()],
        "only src/a.ts was re-extracted: no whole-language reindex"
    );
}

#[test]
fn creating_the_ts_file_of_an_extensionless_import_links_it() {
    let harness = indexed("import { created } from \"./missing\";\n", &[]);
    assert!(harness.imports("src/a.ts").0.is_empty(), "./missing waits unlinked");

    create(&harness, "src/missing.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/missing.ts:missing.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

#[test]
fn creating_the_directory_index_of_an_extensionless_import_links_it() {
    let harness = indexed("import { created } from \"./missing\";\n", &[]);
    assert!(harness.imports("src/a.ts").0.is_empty(), "./missing waits unlinked");

    create(&harness, "src/missing/index.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/missing/index.ts:index.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// `./m.js` names `src/m.ts` before `src/m.js` (TS's ESM rule), so an import
/// linked to `src/m.js` moves to `src/m.ts` once it is created.
#[test]
fn a_js_import_linked_to_the_js_file_moves_to_a_created_ts_file() {
    let harness = indexed("import { mark_v1 } from \"./m.js\";\n", &["src/m.js"]);
    assert_eq!(harness.imports("src/a.ts").0, vec!["src/m.js:m.js".to_string()], "linked to src/m.js");

    create(&harness, "src/m.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/m.ts:m.ts".to_string()], Vec::new()));
    // src/m.js was rewritten too and is not an importer: left alone.
    assert_only_the_importer_re_extracted(&harness);
}

/// A specifier split across a template substitution is stored folded
/// (`./missing`), so it is found by its stored target, also when the file is
/// created while the plugin is down and first seen by the restarted one.
#[test]
fn a_split_literal_import_links_when_its_file_is_created_across_a_restart() {
    let harness = indexed("export const lazy = import(`./${'mis'}sing`);\n", &[]);
    assert!(harness.imports("src/a.ts").0.is_empty(), "the folded ./missing waits unlinked");

    harness.registry.sleep_all_now("restart");
    harness.write("src/missing.ts", "export const created = 1;\n");
    harness.route("src/missing.ts");
    // A sleeping plugin queues the change; the next request wakes it to replay.
    assert_eq!(harness.registry.replay_pending(&harness.conn), 1, "src/missing.ts replayed");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/missing.ts:missing.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// `./missing` linked to `src/missing/index.ts` moves to a created
/// `src/missing.ts`, which comes first among its candidates.
#[test]
fn an_import_linked_to_the_directory_index_moves_to_a_created_ts_file() {
    let harness = indexed("import { mark_v1 } from \"./missing\";\n", &["src/missing/index.ts"]);
    assert_eq!(
        harness.imports("src/a.ts").0,
        vec!["src/missing/index.ts:index.ts".to_string()],
        "linked to src/missing/index.ts"
    );

    create(&harness, "src/missing.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/missing.ts:missing.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// `./m` linked to `src/m.tsx` moves to a created `src/m.ts`.
#[test]
fn an_import_linked_to_the_tsx_file_moves_to_a_created_ts_file() {
    let harness = indexed("import { mark_v1 } from \"./m\";\n", &["src/m.tsx"]);
    assert_eq!(harness.imports("src/a.ts").0, vec!["src/m.tsx:m.tsx".to_string()], "linked to src/m.tsx");

    create(&harness, "src/m.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/m.ts:m.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// An import naming the lower candidate itself (`./m.tsx`, `./m/index`) is
/// selected by that candidate's path like any importer linked there, and
/// its re-extract keeps it where it was: `src/m.ts` is not among its
/// candidates.
#[test]
fn an_import_naming_the_lower_candidate_explicitly_stays_on_it() {
    for (import, lower, link) in [
        ("import { mark_v1 } from \"./m.tsx\";\n", "src/m.tsx", "src/m.tsx:m.tsx"),
        ("import { mark_v1 } from \"./m/index\";\n", "src/m/index.ts", "src/m/index.ts:index.ts"),
    ] {
        let harness = indexed(import, &[lower]);
        assert_eq!(harness.imports("src/a.ts").0, vec![link.to_string()], "{import} linked to {lower}");

        create(&harness, "src/m.ts");

        assert_eq!(harness.imports("src/a.ts"), (vec![link.to_string()], Vec::new()), "{import}");
        assert_only_the_importer_re_extracted(&harness);
    }
}

/// `..` in `src/a.ts` names the project root, which only its index file
/// answers: it waits unlinked until a root `index.ts` is created.
#[test]
fn creating_the_root_index_links_an_import_naming_the_root() {
    let harness = indexed("import { created } from \"..\";\n", &[]);
    assert!(harness.imports("src/a.ts").0.is_empty(), ".. waits unlinked");

    create(&harness, "index.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["index.ts:index.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// `..` linked to the root `index.tsx` moves to a created root `index.ts`;
/// once that is deleted, the importer's next edit falls back to
/// `index.tsx`. Core drops an edge into a deleted file with its node and
/// leaves the importer's link to that importer's next reparse.
#[test]
fn an_import_naming_the_root_moves_to_a_created_root_index_and_back_once_it_is_deleted() {
    let harness = indexed("import { mark_v1 } from \"..\";\n", &["index.tsx"]);
    assert_eq!(harness.imports("src/a.ts").0, vec!["index.tsx:index.tsx".to_string()], "linked to index.tsx");

    create(&harness, "index.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["index.ts:index.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);

    std::fs::remove_file(harness.root().join("index.ts")).unwrap();
    harness.route("index.ts");
    harness.write("src/a.ts", &source("import { mark_v1 } from \"..\";\n", "mark_v3"));
    harness.route("src/a.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["index.tsx:index.tsx".to_string()], Vec::new()));
}
