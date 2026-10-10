//! A `Cargo.toml` save re-extracts only the files and importers its edit can
//! move, through the real Rust plugin (GM-544,
//! docs/architecture/gm-509-selective-config-reindex.md, section 3.6).
//!
//! How a re-extract is seen, as in `config_reindex_typescript.rs`: after the
//! index is built, every `.rs` file is rewritten on disk to declare `mark_v2`
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
//! `None` in `plugins/rust/src/extractor/mod.rs` (no facts are stored: the
//! `indexed` assertion fails).

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

fn harness() -> Harness {
    Harness::with_manifest("rust", |text| {
        assert!(text.contains("semantic_pass = true"), "the shipped manifest's capability line moved");
        text.replace("semantic_pass = true", "semantic_pass = false")
    })
}

const ALPHA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n";

/// `(path, items)` of every `.rs` file; each also declares its mark.
/// `lib2.rs` and the `extra.rs` only it declares are orphans until
/// `alpha`'s `[lib]` names `lib2.rs`. The `n*.rs` orphans keep the selection
/// under core's `FALLBACK_SHARE_PERCENT` of the indexed files, above which
/// it reindexes the language whole.
const FILES: &[(&str, &str)] = &[
    ("alpha/src/lib.rs", "pub mod a;\n"),
    ("alpha/src/a.rs", "pub fn f() {}\n"),
    ("alpha/src/lib2.rs", "pub mod a;\npub mod extra;\n"),
    ("alpha/src/extra.rs", "pub fn g() {}\n"),
    ("beta/src/lib.rs", "pub mod uses_a;\npub mod plain;\nuse alpha::extra;\n"),
    ("beta/src/uses_a.rs", "use alpha::a::f;\n"),
    ("beta/src/plain.rs", "use std::io;\n"),
    ("beta/src/n1.rs", ""),
    ("beta/src/n2.rs", ""),
    ("beta/src/n3.rs", ""),
    ("beta/src/n4.rs", ""),
    ("beta/src/n5.rs", ""),
    ("beta/src/n6.rs", ""),
    ("beta/src/n7.rs", ""),
    ("beta/src/n8.rs", ""),
];

fn source(items: &str, mark: &str) -> String {
    format!("{items}pub fn {mark}() -> u32 {{\n    1\n}}\n")
}

/// Writes the workspace, then saves the root `Cargo.toml` once so the index
/// is built and holds resolution facts.
fn indexed() -> Harness {
    let harness = harness();
    harness.write("Cargo.toml", "[workspace]\nmembers = [\"alpha\", \"beta\"]\n");
    harness.write("alpha/Cargo.toml", ALPHA);
    harness.write("beta/Cargo.toml", "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n");
    for (path, items) in FILES {
        harness.write(path, &source(items, "mark_v1"));
    }
    harness.route("Cargo.toml");
    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new());
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len(), "the first save indexed every file");
    let facts = harness.conn.with(|c| g_mesh::storage::schema::resolution_facts(c, "rust")).unwrap();
    assert!(facts.is_some(), "the reindex's walk stored resolution facts");

    for (path, items) in FILES {
        harness.write(path, &source(items, "mark_v2"));
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

/// Pointing `alpha`'s `[lib]` at `lib2.rs` re-extracts the two roots, the
/// newly declared `extra.rs`, and `beta/src/lib.rs`, whose `use alpha::extra;`
/// was an import of `alpha` while `extra` was no module; not `a.rs` (same
/// key) nor the other importers. The re-extracted importer then holds a
/// resolved `IMPORTS` edge onto the `alpha::extra` container
/// (`:alpha::extra`: a container node has no file path).
#[test]
fn a_lib_path_move_re_extracts_the_re_keyed_files_and_the_parents_importers() {
    let harness = indexed();
    let before = harness.imports("beta/src/lib.rs");
    harness.write("alpha/Cargo.toml", &format!("{ALPHA}\n[lib]\npath = \"src/lib2.rs\"\n"));
    harness.route("alpha/Cargo.toml");

    assert_eq!(
        marked("mark_v2", &harness),
        vec!["alpha/src/extra.rs", "alpha/src/lib.rs", "alpha/src/lib2.rs", "beta/src/lib.rs"],
        "only the re-keyed files and the importer of `alpha` were re-extracted"
    );
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len() - 4);
    let after = harness.imports("beta/src/lib.rs");
    assert!(
        !before.0.contains(&":alpha::extra".to_string()),
        "before the move `alpha::extra` was no module: {before:?}"
    );
    assert!(
        after.0.contains(&":alpha::extra".to_string()),
        "the re-extracted importer reaches the new module: {after:?}"
    );
}

/// A version bump, a new dependency and a feature table re-extract nothing
/// and reindex nothing.
#[test]
fn a_cargo_toml_version_and_dependency_edit_re_extracts_nothing() {
    let harness = indexed();
    harness.write(
        "alpha/Cargo.toml",
        &format!(
            "{}\n[dependencies]\nserde = \"1\"\n\n[features]\nx = []\n",
            ALPHA.replace("0.1.0", "0.2.0")
        ),
    );
    harness.route("alpha/Cargo.toml");

    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new(), "no file was re-extracted");
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len());
}
