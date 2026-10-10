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
//! Controls: `resolution_delta = false` in the manifest (each save
//! reindexes: every file shows `mark_v2`); drop the trailer write in the
//! SDK's `bulk_index` (no facts are stored: every save reindexes).

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

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
