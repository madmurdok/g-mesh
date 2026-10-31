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
//! pointing at a deleted node. The same holds across the targets of a
//! tsconfig alias (`@/x` from `generated/x.ts` to a created `src/x.ts`), the
//! entries of a workspace package and an `imports` key, while an importer of
//! another alias is left alone.
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
//! are not re-extracted); the `fallbacks_shadowed_by` keys dropped from
//! `TsProject::source_changed` (`@/x`, `@scope/p` and `#x` stay where they
//! were).
//!
//! An import of an alias, a workspace package or a `#` key that resolves to
//! nothing waits at the first base of its entry and links once any file it
//! can name is created, re-extracting only its importer and recording its
//! bindings; creating a file that hundreds of linked importers name through
//! the same alias, or editing such a hub after a plugin restart, reindexes
//! nothing. Controls (GM-666): `record_specifier` ignores `miss_address`
//! (no waiting import links); the `waiting_addresses_for` keys dropped from
//! `TsProject::source_changed` (`generated/x.ts`, the package entry, `#x`
//! and `@env/a` stay unlinked); `waiting_address` skips the workspace branch
//! or answers `None` for `#` specifiers; core's own-path arm in
//! `select_affected` removed (the hub edit reindexes the language) or
//! `round_trip` passing no waiting importers (the `#x` and `@env/a` importers
//! link but keep no bindings).
//!
//! A `paths` target that climbs out of its capture (`"@m/*": ["./*/..",
//! "./gen/*"]`) names one file for every `@m/lib/<y>`: a created `lib.ts`
//! re-points an importer linked to `gen/lib/x.ts` and links one waiting at
//! `gen/lib/x`. Control: `climbing_target_prefixes` returns no keys.

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

/// Like [`indexed`] over the fillers and `files` (`(path, import)`), with
/// real `configs` (tsconfig.json, package.json text) that are never routed.
/// Configs and files are all written before the first route, so the model
/// the plugin loads then holds the configs and the directories they sit in
/// (a package.json is read only in a directory holding a file).
fn indexed_with_configs(configs: &[(&str, &str)], files: &[(&str, &str)]) -> Harness {
    let harness = Harness::new();
    for (path, contents) in configs {
        harness.write(path, contents);
    }
    let files: Vec<(&str, &str)> =
        FILLERS.iter().map(|path| (*path, "")).chain(files.iter().copied()).collect();
    for (path, import) in &files {
        harness.write(path, &source(import, "mark_v1"));
    }
    for (path, _) in &files {
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

const ALIAS_TSCONFIG: (&str, &str) =
    ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*","generated/*"]}}}"#);

/// `@/*` tries `src/*` before `generated/*`: `@/x` linked to
/// `generated/x.ts` moves to a created `src/x.ts`, by a re-extract of only
/// its importer.
#[test]
fn an_alias_import_linked_to_a_later_target_moves_to_a_created_earlier_one() {
    let harness = indexed_with_configs(
        &[ALIAS_TSCONFIG],
        &[("generated/x.ts", ""), ("src/a.ts", "import { mark_v1 } from \"@/x\";\n")],
    );
    assert_eq!(
        harness.imports("src/a.ts").0,
        vec!["generated/x.ts:x.ts".to_string()],
        "linked to generated/x.ts"
    );

    create(&harness, "src/x.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/x.ts:x.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// A package without entry fields tries `src/index` before `index`:
/// `@scope/p` linked to `packages/p/index.ts` moves to a created
/// `packages/p/src/index.ts`, by a re-extract of only its importer.
#[test]
fn a_package_import_linked_to_its_root_index_moves_to_a_created_source_index() {
    let harness = indexed_with_configs(
        &[
            ("package.json", r#"{"name":"root","workspaces":["packages/*"]}"#),
            ("packages/p/package.json", r#"{"name":"@scope/p"}"#),
        ],
        &[("packages/p/index.ts", ""), ("src/a.ts", "import { mark_v1 } from \"@scope/p\";\n")],
    );
    assert_eq!(
        harness.imports("src/a.ts").0,
        vec!["packages/p/index.ts:index.ts".to_string()],
        "linked to packages/p/index.ts"
    );

    create(&harness, "packages/p/src/index.ts");

    assert_eq!(
        harness.imports("src/a.ts"),
        (vec!["packages/p/src/index.ts:index.ts".to_string()], Vec::new())
    );
    assert_only_the_importer_re_extracted(&harness);
}

/// An `imports` key listing `./src/x.ts` before `./gen/x.ts`: `#x` linked to
/// `gen/x.ts` moves to a created `src/x.ts`, by a re-extract of only its
/// importer.
#[test]
fn a_hash_import_linked_to_a_later_target_moves_to_a_created_earlier_one() {
    let harness = indexed_with_configs(
        &[("package.json", r##"{"name":"root","imports":{"#x":["./src/x.ts","./gen/x.ts"]}}"##)],
        &[("gen/x.ts", ""), ("src/a.ts", "import { mark_v1 } from \"#x\";\n")],
    );
    assert_eq!(harness.imports("src/a.ts").0, vec!["gen/x.ts:x.ts".to_string()], "linked to gen/x.ts");

    create(&harness, "src/x.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/x.ts:x.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// An `imports` target `.` (a module name) names the project root: `#r`
/// listing `.` before `./gen/r.ts` and linked to `gen/r.ts` moves to a
/// created root `index.ts`, by a re-extract of only its importer.
#[test]
fn a_hash_import_naming_the_root_moves_to_a_created_root_index() {
    let harness = indexed_with_configs(
        &[("package.json", r##"{"name":"root","imports":{"#r":[".","./gen/r.ts"]}}"##)],
        &[("gen/r.ts", ""), ("src/a.ts", "import { mark_v1 } from \"#r\";\n")],
    );
    assert_eq!(harness.imports("src/a.ts").0, vec!["gen/r.ts:r.ts".to_string()], "linked to gen/r.ts");

    create(&harness, "index.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["index.ts:index.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// The importer of another alias (`@/y`, linked to `generated/y.ts`) is not
/// selected by the creation of `src/x.ts`: it is not re-extracted and keeps
/// its link, while `@/x` moves.
#[test]
fn an_import_of_another_alias_is_left_alone_by_the_create() {
    let harness = indexed_with_configs(
        &[ALIAS_TSCONFIG],
        &[
            ("generated/x.ts", ""),
            ("generated/y.ts", ""),
            ("src/b.ts", "import { mark_v1 } from \"@/y\";\n"),
            ("src/a.ts", "import { mark_v1 } from \"@/x\";\n"),
        ],
    );
    let other = vec!["generated/y.ts:y.ts".to_string()];
    assert_eq!(harness.imports("src/b.ts").0, other, "linked to generated/y.ts");

    create(&harness, "src/x.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["src/x.ts:x.ts".to_string()], Vec::new()));
    assert_eq!(harness.imports("src/b.ts"), (other, Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

// -------------------------------------------------------------------------
// GM-666: an alias, package or `#` import that waits at its first base
// -------------------------------------------------------------------------

const IMPORT_CREATED: &str = "import { created } from ";

/// `src/a.ts` waits on `specifier`; its source on disk calls the import, so
/// only a re-extract after the create records the call's binding.
fn waiting_importer(configs: &[(&str, &str)], extra: &[(&str, &str)], specifier: &str) -> Harness {
    let import = format!("{IMPORT_CREATED}\"{specifier}\";\n");
    let mut files: Vec<(&str, &str)> = extra.to_vec();
    files.push(("src/a.ts", &import));
    let harness = indexed_with_configs(configs, &files);
    assert!(harness.imports("src/a.ts").0.is_empty(), "{specifier} waits unlinked");
    harness.write("src/a.ts", &format!("{import}export function mark_v2() {{\n  return created();\n}}\n"));
    harness
}

/// Creates `path` exporting the function `created`, as the watcher does.
fn create_function(harness: &Harness, path: &str) {
    harness.write(path, "export function created() {\n  return 1;\n}\n");
    harness.route(path);
}

/// `src/a.ts` links to `target`, only it was re-extracted, and its call of
/// the imported function is bound to `target`'s `created`.
fn assert_linked_with_bindings(harness: &Harness, target: &str) {
    assert_eq!(harness.imports("src/a.ts"), (vec![format!("{target}:{}", file_name(target))], Vec::new()));
    assert_only_the_importer_re_extracted(harness);
    let calls = calls_of(harness, "src/a.ts");
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].0, format!("{target}:created"), "{calls:?}");
    assert!(calls[0].1, "the call is resolved: {calls:?}");
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap()
}

const SRC_ALIAS: (&str, &str) =
    ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*","generated/*"]}}}"#);

/// `@/x` waits (neither base exists); creating the first base links it.
#[test]
fn creating_the_first_alias_base_links_a_waiting_import() {
    let harness = waiting_importer(&[SRC_ALIAS], &[], "@/x");

    create_function(&harness, "src/x.ts");

    assert_linked_with_bindings(&harness, "src/x.ts");
}

/// Creating the later base, which is not the address the import waits at,
/// links it just the same.
#[test]
fn creating_a_later_alias_base_links_a_waiting_import() {
    let harness = waiting_importer(&[SRC_ALIAS], &[], "@/x");

    create_function(&harness, "generated/x.ts");

    assert_linked_with_bindings(&harness, "generated/x.ts");
}

/// A workspace package import waits before its entry exists.
#[test]
fn creating_the_entry_of_a_workspace_package_links_a_waiting_import() {
    let harness = waiting_importer(
        &[
            ("package.json", r#"{"name":"root","workspaces":["packages/*"]}"#),
            ("packages/p/package.json", r#"{"name":"@scope/p"}"#),
        ],
        &[("packages/p/other.ts", "")],
        "@scope/p",
    );

    create_function(&harness, "packages/p/src/index.ts");

    assert_linked_with_bindings(&harness, "packages/p/src/index.ts");
}

/// A `#` key whose target is the created file: the address equals the path,
/// which core links by itself, so the importer is re-extracted for its
/// bindings by the waiting set read before the link.
#[test]
fn creating_the_target_of_a_waiting_hash_import_links_it_with_its_bindings() {
    let harness = waiting_importer(
        &[("package.json", r##"{"name":"root","imports":{"#x":"./src/x.ts"}}"##)],
        &[],
        "#x",
    );

    create_function(&harness, "src/x.ts");

    assert_linked_with_bindings(&harness, "src/x.ts");
}

/// A wildcard alias with a static target, `@env/*` -> `src/env.ts`, is the
/// same case through the paths branch.
#[test]
fn creating_the_static_target_of_a_wildcard_alias_links_a_waiting_import_with_its_bindings() {
    let harness = waiting_importer(
        &[("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@env/*":["src/env.ts"]}}}"#)],
        &[],
        "@env/a",
    );

    create_function(&harness, "src/env.ts");

    assert_linked_with_bindings(&harness, "src/env.ts");
}

/// `@/x` and `~/x` both map to `src/*`: one create links both importers.
#[test]
fn one_create_links_the_waiting_imports_of_two_aliases_of_one_directory() {
    let harness = indexed_with_configs(
        &[(
            "tsconfig.json",
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@/*":["src/*"],"~/*":["src/*"]}}}"#,
        )],
        &[
            ("src/a.ts", "import { created } from \"@/x\";\n"),
            ("src/b.ts", "import { created } from \"~/x\";\n"),
        ],
    );
    assert!(harness.imports("src/a.ts").0.is_empty() && harness.imports("src/b.ts").0.is_empty());

    create(&harness, "src/x.ts");

    for importer in ["src/a.ts", "src/b.ts"] {
        assert_eq!(harness.imports(importer).0, vec!["src/x.ts:x.ts".to_string()], "{importer}");
    }
    assert_eq!(marked("mark_v2", &harness), vec!["src/a.ts".to_string(), "src/b.ts".to_string()]);
}

/// A waiting `@/y` is not re-extracted by the create of `src/x.ts`.
#[test]
fn a_waiting_import_of_another_name_is_left_alone_by_the_create() {
    let harness = indexed_with_configs(
        &[SRC_ALIAS],
        &[
            ("src/b.ts", "import { created } from \"@/y\";\n"),
            ("src/a.ts", "import { created } from \"@/x\";\n"),
        ],
    );

    create(&harness, "src/x.ts");

    assert_eq!(harness.imports("src/a.ts").0, vec!["src/x.ts:x.ts".to_string()]);
    assert!(harness.imports("src/b.ts").0.is_empty(), "@/y still waits");
    assert_only_the_importer_re_extracted(&harness);
}

/// Forty importers of `@/shared` are linked; creating the file one more
/// importer waits for re-extracts that importer, and does not reindex the
/// language (a prefix or specifier selection would select forty).
#[test]
fn a_create_in_a_widely_aliased_project_re_extracts_only_the_waiting_importer() {
    let importers: Vec<String> = (0..40).map(|i| format!("src/user{i}.ts")).collect();
    let mut files: Vec<(&str, &str)> = vec![("src/shared.ts", "")];
    files.extend(importers.iter().map(|path| (path.as_str(), "import { created } from \"@/shared\";\n")));
    files.push(("src/a.ts", "import { created } from \"@/new\";\n"));
    let harness = indexed_with_configs(&[SRC_ALIAS], &files);
    assert!(harness.imports("src/a.ts").0.is_empty(), "@/new waits unlinked");

    create(&harness, "src/new.ts");

    assert_eq!(harness.imports("src/a.ts").0, vec!["src/new.ts:new.ts".to_string()]);
    assert_only_the_importer_re_extracted(&harness);
}

/// After a plugin restart the first edit of a hub that more than 30% of the
/// files import through an alias re-extracts the hub only: nobody waited at
/// it, so the own-path key selects nothing and the language is not reindexed.
#[test]
fn the_first_edit_of_a_hub_after_a_plugin_restart_reindexes_nothing() {
    let importers: Vec<String> = (0..5).map(|i| format!("src/user{i}.ts")).collect();
    let mut files: Vec<(&str, &str)> = vec![("src/hub.ts", "")];
    files.extend(importers.iter().map(|path| (path.as_str(), "import { mark_v1 } from \"@/hub\";\n")));
    let harness = indexed_with_configs(&[SRC_ALIAS], &files);
    for importer in &importers {
        assert_eq!(harness.imports(importer).0, vec!["src/hub.ts:hub.ts".to_string()], "{importer}");
    }

    harness.registry.sleep_all_now("restart");
    harness.write("src/hub.ts", &source("", "mark_v2"));
    harness.route("src/hub.ts");
    assert_eq!(harness.registry.replay_pending(&harness.conn), 1, "src/hub.ts replayed");

    assert_eq!(marked("mark_v2", &harness), vec!["src/hub.ts".to_string()], "no importer re-extracted");
    for importer in &importers {
        assert_eq!(
            harness.imports(importer).0,
            vec!["src/hub.ts:hub.ts".to_string()],
            "{importer} kept its link"
        );
    }
}

const CLIMBING_PREFIX_FIRST: (&str, &str) =
    ("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@m/*":["./*/..","./gen/*"]}}}"#);

/// `@m/lib/x` linked to `gen/lib/x.ts` moves to a created `lib.ts`, which
/// `./*/..` names for it, re-extracting only that importer.
#[test]
fn an_import_linked_through_a_target_moves_to_a_file_a_climbing_target_reaches() {
    let harness = indexed_with_configs(
        &[CLIMBING_PREFIX_FIRST],
        &[("gen/lib/x.ts", ""), ("src/a.ts", "import { mark_v1 } from \"@m/lib/x\";\n")],
    );
    assert_eq!(
        harness.imports("src/a.ts").0,
        vec!["gen/lib/x.ts:x.ts".to_string()],
        "linked to gen/lib/x.ts"
    );

    create(&harness, "lib.ts");

    assert_eq!(harness.imports("src/a.ts"), (vec!["lib.ts:lib.ts".to_string()], Vec::new()));
    assert_only_the_importer_re_extracted(&harness);
}

/// `@m/lib/x` waits at `gen/lib/x` under `["./gen/*", "./*/.."]` and links
/// to a created `lib.ts`, with its bindings.
#[test]
fn creating_a_file_a_climbing_target_reaches_links_a_waiting_import() {
    let harness = waiting_importer(
        &[("tsconfig.json", r#"{"compilerOptions":{"baseUrl":".","paths":{"@m/*":["./gen/*","./*/.."]}}}"#)],
        &[],
        "@m/lib/x",
    );

    create_function(&harness, "lib.ts");

    assert_linked_with_bindings(&harness, "lib.ts");
}
