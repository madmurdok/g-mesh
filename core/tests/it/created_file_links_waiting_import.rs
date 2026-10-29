//! A relative TS import whose file does not exist yet links once that file
//! is created, by re-extracting only its importer, through the real
//! TypeScript plugin: `./missing` once `src/missing.ts` or
//! `src/missing/index.ts` appears, `./m.js` re-pointed from `src/m.js` to a
//! created `src/m.ts`, `./missing` re-pointed from `src/missing/index.ts`
//! and `./m` from `src/m.tsx` to a created higher-priority candidate, while
//! an import naming the lower candidate explicitly stays on it, a split
//! literal the extractor folds to `./missing`, after a plugin restart, and
//! `..` naming the project root once a root `index.ts` appears (and back to
//! `index.tsx` once it is deleted). A deleted candidate's importer falls
//! back to the remaining candidate or is left unresolved, and links again
//! once a candidate is created; a call through a re-export of a deleted file
//! goes back onto its placeholder; no `IMPORTS` or `CALLS` edge is left
//! pointing at a deleted node.
//!
//! The plugin answers the created file's first `fileChanged` with the
//! addresses that file answers, plus the candidate each address resolved to
//! before; core looks up the importers stored as pointing at them and
//! re-extracts those. A deleted file's importers are re-extracted by core
//! itself, from the imports linked to the file before its rows go.
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
/// once that is deleted, it falls back to `index.tsx`: core re-extracts the
/// importers of a deleted file in the delete's own round trip.
#[test]
fn an_import_naming_the_root_moves_to_a_created_root_index_and_back_once_it_is_deleted() {
    let harness = indexed("import { mark_v1 } from \"..\";\n", &["index.tsx"]);
    assert_eq!(harness.imports("src/a.ts").0, vec!["index.tsx:index.tsx".to_string()], "linked to index.tsx");

    create(&harness, "index.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["index.ts:index.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);

    // Unrouted: only a re-extract by the delete itself shows it.
    harness.write("src/a.ts", &source("import { mark_v1 } from \"..\";\n", "mark_v3"));
    delete(&harness, "index.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["index.tsx:index.tsx".to_string()], Vec::new()));
    assert_eq!(marked("mark_v3", &harness), vec!["src/a.ts".to_string()], "re-extracted by the delete");
    assert_eq!(dangling(&harness), 0);
}

/// Deletes `path` from disk and routes it, as the watcher does a deleted
/// file.
fn delete(harness: &Harness, path: &str) {
    std::fs::remove_file(harness.root().join(path)).unwrap();
    harness.route(path);
}

/// `IMPORTS` and `CALLS` edges whose target node does not exist.
fn dangling(harness: &Harness) -> i64 {
    harness.conn.with(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM edges e LEFT JOIN nodes t ON t.id = e.toId \
             WHERE e.kind IN ('IMPORTS', 'CALLS') AND t.id IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap()
    })
}

/// Files owed a re-extract (`reextract_owed_files`), sorted.
fn owed(harness: &Harness) -> Vec<String> {
    let owed = harness.conn.with(g_mesh::storage::schema::owed_reextracts).unwrap();
    owed.into_iter().flat_map(|(_, _, files)| files).collect()
}

const IMPORT_M: &str = "import { mark_v1 } from \"./m\";\n";

/// `./m` linked to `src/m.ts` falls back to the remaining candidate once
/// `src/m.ts` is deleted, by a re-extract of only its importer.
#[test]
fn deleting_the_linked_candidate_falls_back_to_the_remaining_one() {
    for (remaining, link) in [("src/m/index.ts", "src/m/index.ts:index.ts"), ("src/m.tsx", "src/m.tsx:m.tsx")]
    {
        let harness = indexed(IMPORT_M, &["src/m.ts", remaining]);
        assert_eq!(harness.imports("src/a.ts").0, vec!["src/m.ts:m.ts".to_string()], "linked to src/m.ts");

        delete(&harness, "src/m.ts");

        assert_eq!(harness.imports("src/a.ts"), (vec![link.to_string()], Vec::new()), "{remaining}");
        assert_only_the_importer_re_extracted(&harness);
        assert_eq!(dangling(&harness), 0, "{remaining}");
        assert!(owed(&harness).is_empty(), "{remaining}: {:?}", owed(&harness));
    }
}

/// Deleting the only candidate leaves `./m` unresolved and listed, exactly
/// as a fresh extract with `src/m.ts` absent stores it.
#[test]
fn deleting_the_only_candidate_leaves_the_import_unresolved_as_a_fresh_extract_does() {
    let harness = indexed(IMPORT_M, &["src/m.ts"]);
    assert_eq!(harness.imports("src/a.ts").0, vec!["src/m.ts:m.ts".to_string()], "linked to src/m.ts");

    delete(&harness, "src/m.ts");

    let (resolved, unresolved) = harness.imports("src/a.ts");
    assert!(resolved.is_empty(), "{resolved:?}");
    assert_eq!(unresolved.len(), 1, "./m listed as unresolved: {unresolved:?}");
    assert_eq!((resolved, unresolved), indexed(IMPORT_M, &[]).imports("src/a.ts"), "as a fresh extract");
    assert_only_the_importer_re_extracted(&harness);
    assert_eq!(dangling(&harness), 0);
    assert!(owed(&harness).is_empty(), "{:?}", owed(&harness));
}

/// Once its only candidate is deleted, `./m` links again to a re-created
/// `src/m.ts` or a created `src/m/index.ts`, by a re-extract of the
/// importer: no edit of it, no whole-language reindex.
#[test]
fn an_import_whose_file_was_deleted_links_to_a_re_created_candidate() {
    for (created, link) in [("src/m.ts", "src/m.ts:m.ts"), ("src/m/index.ts", "src/m/index.ts:index.ts")] {
        let harness = indexed(IMPORT_M, &["src/m.ts"]);
        delete(&harness, "src/m.ts");
        assert!(harness.imports("src/a.ts").0.is_empty(), "./m unresolved");
        // Unrouted: only a re-extract by the creation shows it.
        harness.write("src/a.ts", &source(IMPORT_M, "mark_v3"));

        create(&harness, created);

        assert_eq!(harness.imports("src/a.ts"), (vec![link.to_string()], Vec::new()), "{created}");
        assert_eq!(marked("mark_v3", &harness), vec!["src/a.ts".to_string()], "{created}: re-extracted");
        assert_eq!(marked("mark_v2", &harness), Vec::<String>::new(), "{created}: no whole-language reindex");
        assert_eq!(dangling(&harness), 0, "{created}");
    }
}

/// `src/m.ts` and its importer `src/a.ts` deleted in one batch, `src/b.ts`
/// importing `src/a.ts`: routing `src/m.ts` does not re-extract the gone
/// `src/a.ts` (its rows stay for its own delete, nothing is left owed);
/// routing `src/a.ts` then re-extracts `src/b.ts`, whose `./a` is left
/// unresolved.
#[test]
fn a_deleted_importer_is_left_to_its_own_delete() {
    let harness = indexed(IMPORT_M, &["src/m.ts"]);
    let import_a = "import { mark_v1 } from \"./a\";\n";
    harness.write("src/b.ts", &source(import_a, "mark_v1"));
    harness.route("src/b.ts");
    harness.write("src/b.ts", &source(import_a, "mark_v2"));
    assert_eq!(harness.imports("src/b.ts").0, vec!["src/a.ts:a.ts".to_string()], "linked to src/a.ts");
    std::fs::remove_file(harness.root().join("src/m.ts")).unwrap();
    std::fs::remove_file(harness.root().join("src/a.ts")).unwrap();

    harness.route("src/m.ts");

    assert!(marked("mark_v1", &harness).contains(&"src/a.ts".to_string()), "src/a.ts keeps its rows");
    assert!(owed(&harness).is_empty(), "{:?}", owed(&harness));

    harness.route("src/a.ts");

    assert!(!marked("mark_v1", &harness).contains(&"src/a.ts".to_string()), "src/a.ts is retired");
    assert_eq!(marked("mark_v2", &harness), vec!["src/b.ts".to_string()], "only src/b.ts re-extracted");
    let (resolved, unresolved) = harness.imports("src/b.ts");
    assert!(resolved.is_empty() && unresolved.len() == 1, "./a unresolved: {resolved:?} {unresolved:?}");
    assert_eq!(dangling(&harness), 0);
    assert!(owed(&harness).is_empty(), "{:?}", owed(&harness));
}

/// `(target file:name, resolved, linkedFrom is set)` of each `CALLS` edge
/// out of `file`, sorted.
fn calls_of(harness: &Harness, file: &str) -> Vec<(String, bool, bool)> {
    harness.conn.with(|c| {
        let mut statement = c
            .prepare(
                "SELECT t.filePath || ':' || t.name, e.resolved, e.linkedFrom IS NOT NULL FROM edges e \
                 JOIN nodes f ON f.id = e.fromId JOIN nodes t ON t.id = e.toId \
                 WHERE e.kind = 'CALLS' AND f.filePath = ?1 ORDER BY 1",
            )
            .unwrap();
        statement
            .query_map([file], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    })
}

/// `src/a.ts` calls `f` imported from `./b`, which re-exports it from
/// `./m`. Deleting `src/m.ts` re-extracts `src/b.ts` only, and `src/a.ts`'s
/// call, linked onto `src/m.ts`'s `f`, goes back onto its unresolved
/// placeholder in `src/a.ts`; re-creating `src/m.ts` links it again.
#[test]
fn a_call_through_a_re_export_of_a_deleted_file_goes_back_onto_its_placeholder() {
    let harness = Harness::new();
    let mut files: Vec<(&str, &str)> =
        FILLERS.iter().map(|path| (*path, "export const filler = 1;\n")).collect();
    let m = "export function f() {\n  return 1;\n}\n";
    files.extend([
        ("src/m.ts", m),
        ("src/b.ts", "export { f } from \"./m\";\n"),
        ("src/a.ts", "import { f } from \"./b\";\nexport function g() {\n  return f();\n}\n"),
    ]);
    for (path, contents) in &files {
        harness.write(path, contents);
        harness.route(path);
    }
    let linked = vec![("src/m.ts:f".to_string(), true, true)];
    assert_eq!(calls_of(&harness, "src/a.ts"), linked, "linked through the re-export");

    delete(&harness, "src/m.ts");

    let calls = calls_of(&harness, "src/a.ts");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].0.starts_with("src/a.ts:") && !calls[0].1 && !calls[0].2,
        "on its placeholder: {calls:?}"
    );
    assert_eq!(dangling(&harness), 0);
    assert!(owed(&harness).is_empty(), "{:?}", owed(&harness));

    harness.write("src/m.ts", m);
    harness.route("src/m.ts");

    assert_eq!(calls_of(&harness, "src/a.ts"), linked, "linked again");
    assert_eq!(dangling(&harness), 0);
}
