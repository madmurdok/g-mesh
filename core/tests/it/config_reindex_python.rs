//! A `pyproject.toml` save re-extracts only the files and importers its edit
//! can move, through the real Python plugin (GM-544,
//! docs/architecture/gm-509-selective-config-reindex.md, section 3.6).
//!
//! How a re-extract is seen, as in `config_reindex_typescript.rs`: after the
//! index is built, every `.py` file is rewritten on disk to declare `mark_v2`
//! instead of `mark_v1`, and none of those edits is routed. A file core
//! re-extracts shows `mark_v2`; a file left alone still shows `mark_v1`.
//!
//! The first watch-file save of a fresh index has no stored facts and
//! reindexes the language, whose walk stores them (the plugin's
//! `resolutionFacts` trailer); the save under test is the second one. The
//! manifest is the shipped one with `semantic_pass` off, so no language
//! server is involved.
//!
//! Controls: `resolution_delta = false` in the manifest (each save
//! reindexes: every file shows `mark_v2`); `resolution_facts` returning
//! `None` in `plugins/python/src/extractor/mod.rs` (no facts are stored: the
//! `indexed` assertion fails).

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

fn harness() -> Harness {
    Harness::with_manifest("python", |text| {
        assert!(text.contains("semantic_pass = true"), "the shipped manifest's capability line moved");
        text.replace("semantic_pass = true", "semantic_pass = false")
    })
}

const ONE_ROOT: &str = "[project]\nname = \"x\"\nversion = \"1.0.0\"\ndependencies = [\"requests\"]\n\n\
                        [tool.poetry]\npackages = [{ include = \"pkg\", from = \"src\" }]\n";
const TWO_ROOTS: &str = "[project]\nname = \"x\"\nversion = \"1.0.0\"\ndependencies = [\"requests\"]\n\n\
                         [tool.poetry]\npackages = [{ include = \"pkg\", from = \"src\" }, \
                         { include = \"pkg2\", from = \"other\" }]\n";

/// `(path, imports)` of every `.py` file; each also declares its mark.
/// `other/pkg2` is outside every root until [`TWO_ROOTS`]. The `n*.py`
/// modules keep the selection under core's `FALLBACK_SHARE_PERCENT` of the
/// indexed files, above which it reindexes the language whole.
const FILES: &[(&str, &str)] = &[
    ("src/pkg/__init__.py", ""),
    ("src/pkg/core.py", ""),
    ("src/pkg/a.py", "import pkg2.util\n"),
    ("src/pkg/b.py", "from pkg import core\n"),
    ("src/pkg/c.py", "import os\n"),
    ("other/pkg2/__init__.py", ""),
    ("other/pkg2/util.py", ""),
    ("src/pkg/n1.py", ""),
    ("src/pkg/n2.py", ""),
    ("src/pkg/n3.py", ""),
    ("src/pkg/n4.py", ""),
    ("src/pkg/n5.py", ""),
    ("src/pkg/n6.py", ""),
];

fn source(imports: &str, mark: &str) -> String {
    format!("{imports}def {mark}():\n    return 1\n")
}

/// Writes the project, then saves `pyproject.toml` once so the index is
/// built and holds resolution facts.
fn indexed() -> Harness {
    let harness = harness();
    harness.write("pyproject.toml", ONE_ROOT);
    for (path, imports) in FILES {
        harness.write(path, &source(imports, "mark_v1"));
    }
    harness.route("pyproject.toml");
    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new());
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len(), "the first save indexed every file");
    let facts = harness.conn.with(|c| g_mesh::storage::schema::resolution_facts(c, "python")).unwrap();
    assert!(facts.is_some(), "the reindex's walk stored resolution facts");

    for (path, imports) in FILES {
        harness.write(path, &source(imports, "mark_v2"));
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

/// Adding the `other` root re-extracts the two files it re-keys and
/// `src/pkg/a.py`, whose `import pkg2.util` named nothing of ours before;
/// not the other importers. The re-extracted importer then holds a resolved
/// `IMPORTS` edge onto the `pkg2.util` container (`:pkg2.util`: a
/// container node has no file path).
#[test]
fn an_added_root_re_extracts_its_files_and_the_importers_it_resolves() {
    let harness = indexed();
    let before = harness.imports("src/pkg/a.py");
    harness.write("pyproject.toml", TWO_ROOTS);
    harness.route("pyproject.toml");

    assert_eq!(
        marked("mark_v2", &harness),
        vec!["other/pkg2/__init__.py", "other/pkg2/util.py", "src/pkg/a.py"],
        "only the re-keyed files and the importer under `pkg2` were re-extracted"
    );
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len() - 3);
    let after = harness.imports("src/pkg/a.py");
    assert!(
        !before.0.contains(&":pkg2.util".to_string()),
        "before the edit `pkg2.util` was not one of ours: {before:?}"
    );
    assert!(
        after.0.contains(&":pkg2.util".to_string()),
        "the re-extracted importer reaches the module under the new root: {after:?}"
    );
}

/// A version bump and a dependency edit re-extract nothing and reindex
/// nothing.
#[test]
fn a_pyproject_version_and_dependency_edit_re_extracts_nothing() {
    let harness = indexed();
    harness.write(
        "pyproject.toml",
        &ONE_ROOT.replace("1.0.0", "1.0.1").replace("\"requests\"", "\"requests>=2\", \"attrs\""),
    );
    harness.route("pyproject.toml");

    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new(), "no file was re-extracted");
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len());
}
