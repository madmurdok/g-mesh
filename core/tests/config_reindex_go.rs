//! A `go.mod`/`go.work` save re-extracts only the files and importers its
//! edit can move, through the real Go plugin (GM-545,
//! docs/architecture/gm-509-selective-config-reindex.md, section 3.6).
//!
//! How a re-extract is seen, as in `config_reindex_typescript.rs`: after the
//! index is built, every `.go` file is rewritten on disk to declare `mark_v2`
//! instead of `mark_v1`, and none of those edits is routed. A file core
//! re-extracts shows `mark_v2`; a file left alone still shows `mark_v1`.
//!
//! The first watch-file save of a fresh index has no stored facts and
//! reindexes the language, whose walk stores them (the plugin's
//! `resolutionFacts` trailer); the save under test is the second one. The
//! manifest is the shipped one with `semantic_pass` off, so `go/packages` is
//! never loaded, and its command pointing at the binary `core/build.rs`
//! builds in `plugins/go`.
//!
//! Controls: `resolution_delta = false` in the manifest (each save
//! reindexes: every file shows `mark_v2`); no `bulkFactsLine` write in
//! `plugins/go/bulkindex.go`'s `runBulkIndex` (no facts are stored: the
//! `indexed` assertion fails).

#![cfg(unix)]

mod typescript_registry;
use typescript_registry::Harness;

fn harness() -> Harness {
    let binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../plugins/go/g-mesh-plugin-go")
        .canonicalize()
        .expect("the Go plugin binary core/build.rs builds");
    Harness::with_manifest("go", |text| {
        assert!(text.contains("semantic_pass = true"), "the shipped manifest's capability line moved");
        assert!(text.contains("command = \"./g-mesh-plugin-go\""), "the shipped manifest's command moved");
        text.replace("semantic_pass = true", "semantic_pass = false")
            .replace("command = \"./g-mesh-plugin-go\"", &format!("command = \"{}\"", binary.display()))
    })
}

const APP_MOD: &str = "module example.com/app\n\ngo 1.22\n";
const TOOLS_MOD: &str = "module example.com/tools\n\ngo 1.22\n";

/// `(path, imports)` of every `.go` file, each the only file of its
/// package; each also declares its mark. `tools/` is the nested module
/// `example.com/tools`. The `n*` packages keep the selection under core's
/// `FALLBACK_SHARE_PERCENT` of the indexed files, above which it reindexes
/// the language whole.
const FILES: &[(&str, &str)] = &[
    ("tools/gen/gen.go", ""),
    ("tools/util/util.go", ""),
    ("cmd/cmd.go", "import _ \"example.com/tools/gen\"\n\n"),
    ("client/client.go", "import _ \"example.com/ext/x\"\n\n"),
    ("core/core.go", ""),
    ("lib/lib.go", "import _ \"example.com/app/core\"\n\n"),
    ("n1/n1.go", ""),
    ("n2/n2.go", ""),
    ("n3/n3.go", ""),
    ("n4/n4.go", ""),
    ("n5/n5.go", ""),
    ("n6/n6.go", ""),
];

fn source(path: &str, imports: &str, mark: &str) -> String {
    let package = path.rsplit('/').next().unwrap().trim_end_matches(".go");
    format!("package {package}\n\n{imports}func {mark}() int {{\n\treturn 1\n}}\n")
}

/// Writes the project, then saves `go.mod` once so the index is built and
/// holds resolution facts.
fn indexed() -> Harness {
    let harness = harness();
    harness.write("go.mod", APP_MOD);
    harness.write("go.work", "go 1.22\n\nuse (\n\t.\n\t./tools\n)\n");
    harness.write("tools/go.mod", TOOLS_MOD);
    for (path, imports) in FILES {
        harness.write(path, &source(path, imports, "mark_v1"));
    }
    harness.route("go.mod");
    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new());
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len(), "the first save indexed every file");
    let facts = harness.conn.with(|c| g_mesh::storage::schema::resolution_facts(c, "go")).unwrap();
    assert!(facts.is_some(), "the reindex's walk stored resolution facts");

    for (path, imports) in FILES {
        harness.write(path, &source(path, imports, "mark_v2"));
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

/// Renaming the nested module re-extracts the two files it re-keys and
/// `cmd/cmd.go`, whose import named the old path; not the other importers.
/// The re-extracted importer's import of `example.com/tools/gen` no longer
/// reaches a project container.
#[test]
fn a_nested_module_rename_re_extracts_its_files_and_the_importers_of_its_path() {
    let harness = indexed();
    let before = harness.imports("cmd/cmd.go");
    harness.write("tools/go.mod", "module example.com/tools2\n\ngo 1.22\n");
    harness.route("tools/go.mod");

    assert_eq!(
        marked("mark_v2", &harness),
        vec!["cmd/cmd.go", "tools/gen/gen.go", "tools/util/util.go"],
        "only the re-keyed files and the importer of `example.com/tools` were re-extracted"
    );
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len() - 3);
    let after = harness.imports("cmd/cmd.go");
    let reaches_gen =
        |resolved: &[String]| resolved.iter().any(|target| target.ends_with("example.com/tools/gen"));
    assert!(
        reaches_gen(&before.0),
        "before the rename the import reached the tools/gen container: {before:?}"
    );
    assert!(
        !reaches_gen(&after.0),
        "after the rename `example.com/tools/gen` is no project package: {after:?}"
    );
}

/// A `replace` of `example.com/ext` re-extracts its importer and no other
/// file.
#[test]
fn a_replace_edit_re_extracts_only_the_importers_of_the_replaced_module() {
    let harness = indexed();
    harness.write("go.mod", &format!("{APP_MOD}\nreplace example.com/ext => ../ext\n"));
    harness.route("go.mod");

    assert_eq!(
        marked("mark_v2", &harness),
        vec!["client/client.go"],
        "only the importer of `example.com/ext`"
    );
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len() - 1);
}

/// `require`, `go` and `toolchain` edits re-extract nothing and reindex
/// nothing.
#[test]
fn a_require_and_go_directive_edit_re_extracts_nothing() {
    let harness = indexed();
    harness.write(
        "go.mod",
        "module example.com/app\n\ngo 1.23.0\n\ntoolchain go1.23.1\n\nrequire example.com/ext v1.2.0\n",
    );
    harness.route("go.mod");

    assert_eq!(marked("mark_v2", &harness), Vec::<String>::new(), "no file was re-extracted");
    assert_eq!(marked("mark_v1", &harness).len(), FILES.len());
}
