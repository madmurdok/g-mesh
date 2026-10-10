use super::*;
use crate::daemon::manifest::{Capabilities, WorkspaceConfig};

// -----------------------------------------------------------------
// GM-375: a fresh worktree's unbuilt cargo-workspace plugin binary
// names the missing artifact and the build command, instead of a bare
// "No such file or directory" that blames the plugin.
// -----------------------------------------------------------------

/// A manifest whose `command` is `<workspace>/target/debug/g-mesh-plugin-fake`,
/// the exact shape `plugins/python/plugin.toml` and
/// `plugins/rust/plugin.toml` declare (a dev-time path into the
/// workspace's own `target/`, resolved relative to the manifest's own
/// directory). The binary is deliberately never created.
fn fake_workspace_plugin_manifest(command: PathBuf) -> PluginManifest {
    PluginManifest {
        language: "fake".to_string(),
        protocol_version: 2,
        plugin_version: "0.0.0".to_string(),
        command,
        args: Vec::new(),
        extensions: vec![".fake".to_string()],
        fingerprint_ignore: Vec::new(),
        manifest_dir: PathBuf::from("/dev/null"),
        capabilities: Capabilities::default(),
        workspace: WorkspaceConfig::default(),
        non_symbol_queries: Default::default(),
        symbol_query_prefixes: Default::default(),
        reexports: Default::default(),
    }
}

/// `run_bulk` against a manifest whose cargo-workspace binary was never
/// built must report the same friendly hint
/// `daemon::plugin::missing_workspace_binary_hint` gives the daemon's own
/// spawn sites - not the bare `Command::spawn` OS error this test would
/// see if the GM-375 check above `command.spawn()` in `run_bulk` were
/// removed (confirmed by temporarily reverting it: the assertion below
/// fails on the unpatched function, quoting "No such file or directory"
/// instead).
#[test]
fn run_bulk_names_a_missing_workspace_binary_instead_of_the_bare_os_error() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-fake");
    let manifest = fake_workspace_plugin_manifest(binary.clone());
    let scratch = Scratch::create().unwrap();

    let run = run_bulk(&manifest, &scratch, Duration::from_secs(5));

    let failure = run.failure.expect("a missing binary must fail the bulk run");
    assert!(failure.contains(&binary.display().to_string()), "{failure}");
    assert!(failure.contains("has not been built yet"), "{failure}");
    assert!(failure.contains("cargo build --workspace"), "{failure}");
    assert!(!failure.contains("No such file or directory"), "{failure}");
    assert!(!failure.contains("failed to spawn"), "{failure}");
}

/// Same claim as above, for `run_session`'s control-plane spawn - the
/// other site `Command::new(&manifest.command).spawn()` was called
/// unconditionally before GM-375.
#[test]
fn run_session_names_a_missing_workspace_binary_instead_of_the_bare_os_error() {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let binary = workspace.path().join("target").join("debug").join("g-mesh-plugin-fake");
    let manifest = fake_workspace_plugin_manifest(binary.clone());
    let scratch = Scratch::create().unwrap();
    let conn = open_index(&manifest).unwrap();
    let target = EditTarget {
        file_path: "a.fake".to_string(),
        line: 1,
        original: b"fn a\n".to_vec(),
        edited: b"fn a \n".to_vec(),
        declaration: None,
    };

    let session = run_session(
        &manifest,
        &scratch,
        &conn,
        &target,
        RoundTripTimeouts::default(),
        Duration::from_secs(5),
    );

    let failure = session.failure.expect("a missing binary must fail the session");
    assert!(failure.contains(&binary.display().to_string()), "{failure}");
    assert!(failure.contains("has not been built yet"), "{failure}");
    assert!(failure.contains("cargo build --workspace"), "{failure}");
    assert!(!failure.contains("No such file or directory"), "{failure}");
    assert!(!failure.contains("failed to spawn"), "{failure}");
}

#[test]
fn the_whitespace_edit_goes_before_the_last_newline() {
    let (edited, line) = whitespace_edit(b"a\nb\n").unwrap();
    assert_eq!(edited, b"a\nb \n");
    assert_eq!(line, 2);
}

/// A file that does not end in a newline keeps its final line untouched:
/// the space goes at the end of the line before it.
#[test]
fn the_whitespace_edit_leaves_an_unterminated_final_line_alone() {
    let (edited, line) = whitespace_edit(b"a\nb").unwrap();
    assert_eq!(edited, b"a \nb");
    assert_eq!(line, 1);
}

#[test]
fn the_whitespace_edit_goes_before_the_carriage_return_of_a_crlf_file() {
    let (edited, line) = whitespace_edit(b"a\r\nb\r\n").unwrap();
    assert_eq!(edited, b"a\r\nb \r\n");
    assert_eq!(line, 2);
}

#[test]
fn a_file_without_any_newline_offers_no_whitespace_edit() {
    assert!(whitespace_edit(b"export const a = 1;").is_none());
}

/// A node as a bulk line would carry it, parsed through the real wire
/// type - `(start line, end line)` is all [`declaration_edit`] reads.
fn wire_node(id: &str, kind: &str, native_kind: Option<&str>, lines: (u32, u32)) -> WireNode {
    let extra = native_kind
        .map(|k| format!(",\"nativeKind\":\"{k}\",\"target\":{{\"scope\":{{\"file\":\"b.fk\"}},\"key\":{{\"name\":\"x\"}}}}"))
        .unwrap_or_default();
    let json = format!(
        "{{\"id\":\"{id}\",\"kind\":\"{kind}\",\"name\":\"{id}\",\"qualifiedName\":\"{id}\",\"filePath\":\"a.fk\",\
         \"range\":{{\"start\":{{\"line\":{},\"col\":0}},\"end\":{{\"line\":{},\"col\":1}}}},\"visibility\":\"public\",\
         \"language\":\"fake\"{extra}}}",
        lines.0, lines.1
    );
    match BulkItem::parse(&json).unwrap() {
        BulkItem::Node(node) => *node,
        _ => panic!("not a node"),
    }
}

/// The first declaration by position - not the `File` node, not a
/// placeholder, even when those start earlier - and the break goes before
/// its *last* line, growing it.
#[test]
fn the_declaration_edit_breaks_the_first_declarations_last_line() {
    let nodes = [
        wire_node("file", "File", None, (0, 4)),
        wire_node("import", "Module", Some("pending_symbol"), (0, 0)),
        wire_node("later", "Function", None, (3, 3)),
        wire_node("first", "Function", None, (1, 2)),
    ];
    let edit = declaration_edit(b"import x\nfn first {\n}\nfn later\n", &nodes).unwrap();
    assert_eq!(edit.node_id, "first");
    assert_eq!(edit.line, 3);
    assert_eq!(edit.edited, b"import x\nfn first {\n\n}\nfn later\n");
}

#[test]
fn the_declaration_edit_moves_a_one_line_declaration_on_the_first_line_and_keeps_crlf() {
    let nodes = [wire_node("f", "Function", None, (0, 0))];
    let edit = declaration_edit(b"fn f\r\nfn g\r\n", &nodes).unwrap();
    assert_eq!(edit.line, 1);
    assert_eq!(edit.edited, b"\r\nfn f\r\nfn g\r\n");
}

#[test]
fn no_declaration_edit_without_a_declaration_or_past_the_end_of_the_file() {
    let only_file = [wire_node("file", "File", None, (0, 1))];
    assert!(declaration_edit(b"a\nb\n", &only_file).is_none());
    let beyond = [wire_node("f", "Function", None, (0, 9))];
    assert!(declaration_edit(b"a\nb\n", &beyond).is_none());
}

#[test]
fn bulk_line_numbers_count_blank_lines() {
    let lines = parse_bulk_lines(b"not json\n\n{\"broken\":\r\n");
    assert_eq!(lines.iter().map(|l| l.line_no).collect::<Vec<_>>(), vec![1, 3]);
    assert!(lines.iter().all(|l| l.item.is_err()));
}

/// The tee must hand back exactly the bytes the consumer read, whichever
/// of `read`/`fill_buf`+`consume` the consumer used - `read_frame` uses
/// both (`read_until` for headers, `read_exact` for the body).
#[test]
fn the_tee_reader_logs_exactly_what_was_consumed() {
    let frame = b"Content-Length: 2\r\n\r\n{}Content-Length: 4\r\n\r\nnull";
    let mut tee =
        TeeReader { inner: BufReader::with_capacity(3, Cursor::new(frame.to_vec())), log: Vec::new() };
    assert_eq!(read_frame(&mut tee).unwrap().unwrap(), b"{}");
    assert_eq!(tee.log, b"Content-Length: 2\r\n\r\n{}");
    assert_eq!(read_frame(&mut tee).unwrap().unwrap(), b"null");
    assert_eq!(tee.log, frame);
}

// -----------------------------------------------------------------
// GM-337: quoting a plugin's own stderr into the failure about it
// -----------------------------------------------------------------

#[test]
fn quoted_stderr_shows_the_last_lines_indented_past_the_verdict_column() {
    let stderr = b"node:internal/modules/cjs/loader:1228\n  throw err;\n\nError: Cannot find module\n";
    let quoted =
        quote_stderr(stderr, "bulk run 1: the bulk index exited with exit code: 1".to_string(), true);

    assert_eq!(
        quoted.lines().collect::<Vec<_>>(),
        vec![
            "bulk run 1: the bulk index exited with exit code: 1",
            "            its stderr, last 3 of 3 line(s):",
            "            | node:internal/modules/cjs/loader:1228",
            "            |   throw err;",
            "            | Error: Cannot find module",
        ],
    );
    // Nothing a plugin prints can be read back as a check verdict by
    // `report`'s line format, whatever it prints.
    for line in quoted.lines().skip(1) {
        assert!(
            !line.starts_with("  PASS") && !line.starts_with("  FAIL") && !line.starts_with("  SKIP"),
            "{line}"
        );
    }
}

#[test]
fn quoted_stderr_keeps_the_tail_of_a_plugin_that_logs_steadily() {
    let stderr: Vec<u8> = (0..STDERR_LINES_QUOTED + 5)
        .fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("line {i}\n"));
            acc
        })
        .into_bytes();
    let quoted = quote_stderr(&stderr, "failed".to_string(), true);

    assert!(
        quoted.contains(&format!(
            "its stderr, last {STDERR_LINES_QUOTED} of {} line(s):",
            STDERR_LINES_QUOTED + 5
        )),
        "{quoted}"
    );
    // The banner a runtime prints on the way out is the last thing it
    // writes, so the tail is the half worth keeping.
    assert!(quoted.contains(&format!("| line {}", STDERR_LINES_QUOTED + 4)), "{quoted}");
    assert!(!quoted.contains("| line 0\n") && !quoted.ends_with("| line 0"), "{quoted}");
    assert_eq!(quoted.matches("\n            | ").count(), STDERR_LINES_QUOTED, "{quoted}");
}

#[test]
fn a_silent_plugins_own_fate_says_so_and_anything_else_stays_as_it_was() {
    assert_eq!(
        quote_stderr(b"", "the bulk index exited with exit code: 1".to_string(), true),
        "the bulk index exited with exit code: 1, having written nothing to stderr",
    );
    // A failure that is the kit's own (reading ids back out of the
    // index, say) gains nothing from a note about the plugin's silence.
    assert_eq!(
        quote_stderr(b"   \n\n", "reading a.fk's node ids back from the index: locked".to_string(), false),
        "reading a.fk's node ids back from the index: locked",
    );
}

#[test]
fn quoted_stderr_survives_bytes_that_are_not_utf8() {
    let quoted = quote_stderr(&[0xff, 0xfe, b'\n', b'o', b'k'], "failed".to_string(), true);
    assert!(quoted.contains("| ok"), "{quoted}");
}

// -----------------------------------------------------------------
// GM-516: `capabilities.files-created-resolves`' evidence.
// -----------------------------------------------------------------

fn files_created_pair(target: &str, importer: &str) -> FilesCreatedPair {
    FilesCreatedPair {
        target: target.to_string(),
        target_text: "fn created\n".to_string(),
        importer: importer.to_string(),
        importer_text: format!("import {target}\n"),
    }
}

/// Every way a pair cannot be run is a finding naming its field, and a
/// runnable pair - including one in a subdirectory that does not exist yet -
/// has none. Control: return no findings from `files_created_pair_findings`
/// (each of the invalid cases below then reports nothing).
#[test]
fn an_unrunnable_files_created_pair_is_named_field_by_field() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("a.fk"), "fn alpha\n").unwrap();
    let mut manifest = fake_workspace_plugin_manifest(PathBuf::from("true"));
    manifest.extensions = vec![".fk".to_string()];
    let findings = |target: &str, importer: &str| {
        files_created_pair_findings(&manifest, workspace.path(), &files_created_pair(target, importer))
    };

    assert_eq!(findings("new/c.fk", "d.fk"), Vec::<String>::new());

    let existing = findings("a.fk", "d.fk");
    assert_eq!(existing.len(), 1, "{existing:?}");
    assert!(existing[0].starts_with("[files_created] target = \"a.fk\" already exists"), "{existing:?}");

    let unclaimed = findings("c.fk", "d.txt");
    assert_eq!(unclaimed.len(), 1, "{unclaimed:?}");
    assert!(unclaimed[0]
        .starts_with("[files_created] importer = \"d.txt\" has none of the manifest's extensions"));

    let same = findings("c.fk", "c.fk");
    assert_eq!(same, vec!["[files_created] target and importer are the same path (\"c.fk\")".to_string()]);

    for escaping in ["../c.fk", "./c.fk", "", "/tmp/c.fk"] {
        let found = findings(escaping, "d.fk");
        assert!(
            found.iter().any(|f| f.starts_with("[files_created] target = ") && f.contains("not a plain")),
            "{escaping:?}: {found:?}"
        );
    }
}

/// `import_rows` reads, for an `IMPORTS` edge onto a core-owned container,
/// the files of that container's `DEFINES` members, once each - the half of
/// the amended D2 the fake plugin cannot exercise (it emits no containers).
/// An edge onto a plain node reports no members, and an edge from another
/// file is not the importer's. Control: drop the `containers` subquery from
/// `import_rows` (the container row then reports no member files).
#[test]
fn import_rows_report_a_containers_member_files() {
    let manifest = fake_workspace_plugin_manifest(PathBuf::from("true"));
    let store = open_index(&manifest).unwrap();
    store.with(|conn| {
        conn.execute_batch(
            "INSERT INTO nodes (id, kind, name, qualifiedName, filePath, startLine, startCol, endLine, endCol, language, nativeKind) VALUES
               ('imp',  'File',     'importer', 'importer.py', 'importer.py',   0, 0, 1, 0, 'fake', NULL),
               ('pkg',  'Module',   'target',   'pkg.target',  '',              0, 0, 0, 0, 'fake', 'container'),
               ('m1',   'Function', 'one',      'one',         'pkg/target.py', 0, 0, 1, 0, 'fake', NULL),
               ('m2',   'Function', 'two',      'two',         'pkg/target.py', 2, 0, 3, 0, 'fake', NULL),
               ('tf',   'File',     'target',   'pkg/target.py','pkg/target.py',0, 0, 3, 0, 'fake', NULL),
               ('other','File',     'other',    'other.py',    'other.py',      0, 0, 1, 0, 'fake', NULL);
             INSERT INTO containers (nodeId, language, key, parentKey, memberCount) VALUES
               ('pkg', 'fake', 'pkg.target', NULL, 2);
             INSERT INTO edges (id, fromId, toId, kind, source, engine, resolved) VALUES
               ('d1', 'pkg',   'm1',  'DEFINES', 'syntactic', 'core', 1),
               ('d2', 'pkg',   'm2',  'DEFINES', 'syntactic', 'core', 1),
               ('i1', 'imp',   'pkg', 'IMPORTS', 'syntactic', 'fake', 1),
               ('i2', 'imp',   'tf',  'IMPORTS', 'syntactic', 'fake', 1),
               ('i3', 'other', 'tf',  'IMPORTS', 'syntactic', 'fake', 1);",
        )
    })
    .unwrap();

    let rows = import_rows(&store, "importer.py").unwrap();
    assert_eq!(rows.len(), 2, "only the importer's own IMPORTS edges: {rows:?}");
    let container = rows.iter().find(|row| row.to_native_kind.as_deref() == Some("container")).unwrap();
    assert_eq!(container.to_file, "");
    assert_eq!(container.container_member_files, vec!["pkg/target.py".to_string()]);
    assert!(container.resolved);
    let file = rows.iter().find(|row| row.to_kind == "File").unwrap();
    assert_eq!(file.to_file, "pkg/target.py");
    assert!(file.container_member_files.is_empty(), "{file:?}");
}

// -----------------------------------------------------------------
// GM-544: the resolution-delta version bump per watch-file format.
// Every case states the whole expected text, so "only the version
// changed, every other byte kept" is the assertion itself.
// -----------------------------------------------------------------

fn bump(name: &str, text: &str) -> Option<(String, String)> {
    version_bump_for(name, text)
}

fn bumped(text: &str, field: &str) -> Option<(String, String)> {
    Some((text.to_string(), field.to_string()))
}

#[test]
fn a_cargo_package_version_gets_the_suffix_and_nothing_else_moves() {
    let text = "# top comment\n[package]\nname = \"alpha\"\nversion   =  \"0.1.0\"  # keep me\nedition = \"2021\"\n\n\
                [dependencies]\nserde = { version = \"1.0\" }\n";
    let expected =
        "# top comment\n[package]\nname = \"alpha\"\nversion   =  \"0.1.0-plugin-check\"  # keep me\n\
                    edition = \"2021\"\n\n[dependencies]\nserde = { version = \"1.0\" }\n";
    assert_eq!(bump("Cargo.toml", text), bumped(expected, "`[package].version`"));
}

#[test]
fn a_cargo_version_in_another_table_before_the_package_is_never_the_one_edited() {
    // A `[dependencies.foo]` table has its own `version = "..."` line, and
    // it comes first; the edit still lands under `[package]`.
    let text = "[dependencies.foo]\nversion = \"9.9\"\n\n[package]\nname = \"a\"\nversion = \"0.1.0\"\n";
    let expected = "[dependencies.foo]\nversion = \"9.9\"\n\n[package]\nname = \"a\"\nversion = \"0.1.0-plugin-check\"\n";
    assert_eq!(bump("Cargo.toml", text), bumped(expected, "`[package].version`"));
}

#[test]
fn a_virtual_cargo_workspace_bumps_its_workspace_package_version() {
    let text = "[workspace]\nmembers = [\"crates/a\"]\n\n[workspace.package]\nversion = \"2.0.0\"\n";
    let expected =
        "[workspace]\nmembers = [\"crates/a\"]\n\n[workspace.package]\nversion = \"2.0.0-plugin-check\"\n";
    assert_eq!(bump("Cargo.toml", text), bumped(expected, "`[workspace.package].version`"));
}

#[test]
fn a_cargo_package_version_wins_over_the_workspace_package_version() {
    let text = "[workspace.package]\nversion = \"2.0.0\"\n\n[package]\nname = \"a\"\nversion = \"0.1.0\"\n";
    let expected = "[workspace.package]\nversion = \"2.0.0\"\n\n[package]\nname = \"a\"\nversion = \"0.1.0-plugin-check\"\n";
    assert_eq!(bump("Cargo.toml", text), bumped(expected, "`[package].version`"));
}

#[test]
fn a_cargo_toml_without_its_own_version_string_is_not_bumped_and_none_is_inserted() {
    for text in [
        // A virtual workspace root.
        "[workspace]\nmembers = [\"crates/a\"]\nresolver = \"2\"\n",
        // An inherited version: a dotted key, not a string.
        "[package]\nname = \"a\"\nversion.workspace = true\n",
        // No version at all (Cargo defaults it; inserting one is a real change).
        "[package]\nname = \"a\"\n",
        // An empty and a multi-line string are never edited.
        "[package]\nname = \"a\"\nversion = \"\"\n",
        "[package]\nname = \"a\"\nversion = \"\"\"0.1.0\"\"\"\n",
        // Not TOML.
        "[package\nversion = \"0.1.0\"\n",
    ] {
        assert_eq!(bump("Cargo.toml", text), None, "{text}");
    }
}

#[test]
fn a_literal_string_version_and_crlf_line_ends_are_kept() {
    let text = "[package]\r\nname = 'a'\r\nversion = '0.1.0' # c\r\n";
    let expected = "[package]\r\nname = 'a'\r\nversion = '0.1.0-plugin-check' # c\r\n";
    assert_eq!(bump("Cargo.toml", text), bumped(expected, "`[package].version`"));
}

#[test]
fn a_pyproject_project_version_gets_the_suffix() {
    let text =
        "[build-system]\nrequires = [\"hatchling\"]\n\n[project]\nname = \"demo\"\nversion = \"0.1.0\"\n\
                dependencies = [\"requests>=2\"]\n\n[tool.poetry]\nversion = \"7.0.0\"\n";
    let expected = "[build-system]\nrequires = [\"hatchling\"]\n\n[project]\nname = \"demo\"\n\
                    version = \"0.1.0-plugin-check\"\ndependencies = [\"requests>=2\"]\n\n[tool.poetry]\n\
                    version = \"7.0.0\"\n";
    assert_eq!(bump("pyproject.toml", text), bumped(expected, "`[project].version`"));
}

#[test]
fn a_poetry_pyproject_bumps_the_tool_poetry_version() {
    let text = "[tool.poetry]\nname = \"demo\"\nversion = \"1.2.3\"\n\n[tool.poetry.dependencies]\npython = \"^3.11\"\n";
    let expected = "[tool.poetry]\nname = \"demo\"\nversion = \"1.2.3-plugin-check\"\n\n\
                    [tool.poetry.dependencies]\npython = \"^3.11\"\n";
    assert_eq!(bump("pyproject.toml", text), bumped(expected, "`[tool.poetry].version`"));
    // A `[project]` table without a version falls through to poetry's.
    let text = "[project]\nname = \"demo\"\ndynamic = [\"version\"]\n\n[tool.poetry]\nversion = \"1.2.3\"\n";
    let expected = "[project]\nname = \"demo\"\ndynamic = [\"version\"]\n\n[tool.poetry]\nversion = \"1.2.3-plugin-check\"\n";
    assert_eq!(bump("pyproject.toml", text), bumped(expected, "`[tool.poetry].version`"));
}

#[test]
fn a_pyproject_without_a_static_version_is_not_bumped_and_none_is_inserted() {
    for text in [
        "[project]\nname = \"demo\"\ndynamic = [\"version\"]\n",
        "[project]\nname = \"demo\"\n",
        "[tool.black]\nline-length = 100\n",
        // A Cargo-shaped table is not a pyproject version.
        "[package]\nversion = \"0.1.0\"\n",
    ] {
        assert_eq!(bump("pyproject.toml", text), None, "{text}");
    }
}

#[test]
fn a_go_mod_single_line_require_version_gets_the_suffix() {
    let text = "module example.com/m\n\ngo 1.22\n\nrequire golang.org/x/text v0.14.0 // indirect\n";
    let expected =
        "module example.com/m\n\ngo 1.22\n\nrequire golang.org/x/text v0.14.0-plugin-check // indirect\n";
    assert_eq!(bump("go.mod", text), bumped(expected, "the `require golang.org/x/text` version"));
}

#[test]
fn a_go_mod_require_block_skips_comments_and_incompatible_versions() {
    let text = "module example.com/m\n\ngo 1.22\n\nrequire (\n\t// require example.com/c v1.0.0\n\
                \texample.com/old v2.0.0+incompatible\n\texample.com/new v1.4.0\n)\n";
    let expected = "module example.com/m\n\ngo 1.22\n\nrequire (\n\t// require example.com/c v1.0.0\n\
                    \texample.com/old v2.0.0+incompatible\n\texample.com/new v1.4.0-plugin-check\n)\n";
    assert_eq!(bump("go.mod", text), bumped(expected, "the `require example.com/new` version"));
}

#[test]
fn a_go_mod_without_a_usable_require_rewrites_the_go_directive() {
    let text = "module example.com/m\n\ngo 1.22\n";
    assert_eq!(bump("go.mod", text), bumped("module example.com/m\n\ngo 1.22.0\n", "the `go` directive"));
    let text = "module example.com/m\r\n\r\ngo 1.22.3\r\n\r\nrequire example.com/old v2.0.0+incompatible\r\n";
    let expected =
        "module example.com/m\r\n\r\ngo 1.22\r\n\r\nrequire example.com/old v2.0.0+incompatible\r\n";
    assert_eq!(bump("go.mod", text), bumped(expected, "the `go` directive"));
}

#[test]
fn a_go_mod_with_neither_a_require_nor_a_plain_go_directive_is_not_bumped() {
    for text in
        ["module example.com/m\n", "module example.com/m\n\ngo 1\n", "module example.com/m\n\ngo 1.22rc1\n"]
    {
        assert_eq!(bump("go.mod", text), None, "{text}");
    }
}

#[test]
fn the_format_follows_the_file_name_and_any_other_name_takes_the_json_rule() {
    let cargo = "[package]\nname = \"a\"\nversion = \"0.1.0\"\n";
    assert!(bump("Cargo.toml", cargo).is_some());
    assert_eq!(bump("pyproject.toml", cargo), None);
    // A TOML file under any other name is read as JSON, which it is not.
    assert_eq!(bump("Pipfile", cargo), None);
    for (name, text) in
        [("setup.cfg", "[metadata]\nversion = 0.1.0\n"), ("setup.py", "setup(version=\"0.1.0\")\n")]
    {
        assert_eq!(bump(name, text), None, "{name}");
    }
    assert_eq!(
        bump("composer.json", "{\"name\": \"x\", \"version\": \"1.0.0\"}"),
        bumped("{\"name\": \"x\", \"version\": \"1.0.0-plugin-check\"}", "the top-level `version`")
    );
}

#[test]
fn the_json_rule_appends_to_a_top_level_version_or_inserts_one() {
    let text = "{\n  \"name\": \"x\",\n  \"dependencies\": { \"version\": \"1.0.0\" },\n  \"version\": \"1.0.0\"\n}\n";
    let expected =
        "{\n  \"name\": \"x\",\n  \"dependencies\": { \"version\": \"1.0.0\" },\n  \"version\": \"1.0.0-plugin-check\"\n}\n";
    assert_eq!(version_bump(text).as_deref(), Some(expected));
    assert_eq!(
        version_bump("{\"name\": \"x\"}").as_deref(),
        Some("{\"version\": \"0.0.0-plugin-check\",\"name\": \"x\"}")
    );
    assert_eq!(version_bump("{}").as_deref(), Some("{\"version\": \"0.0.0-plugin-check\"}"));
    assert_eq!(version_bump("{\"version\": 3}"), None);
    assert_eq!(version_bump("[1, 2]"), None);
    assert_eq!(version_bump("{\"version\": \"1\" // c\n}"), None);
}

fn watching(globs: &[&str]) -> PluginManifest {
    let mut manifest = fake_workspace_plugin_manifest(PathBuf::from("/dev/null"));
    manifest.workspace.watch_files = globs.iter().map(|g| globset::Glob::new(g).unwrap()).collect();
    manifest.workspace.exclude_dirs = vec!["target".to_string()];
    manifest
}

fn write_tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, contents) in files {
        let path = dir.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    dir
}

#[test]
fn the_chooser_passes_a_virtual_workspace_root_for_the_shallowest_bumpable_member() {
    let dir = write_tree(&[
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
        ("crates/beta/Cargo.toml", "[package]\nname = \"beta\"\nversion = \"0.2.0\"\n"),
        ("crates/alpha/Cargo.toml", "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n"),
        ("target/x/Cargo.toml", "[package]\nversion = \"9.0.0\"\n"),
    ]);
    let chosen = choose_version_bump(&watching(&["Cargo.toml"]), dir.path()).expect("a member is bumpable");
    assert_eq!(chosen.file_path, "crates/alpha/Cargo.toml");
    assert_eq!(chosen.field, "`[package].version`");
    assert_eq!(chosen.original, b"[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n");
    assert_eq!(chosen.edited, b"[package]\nname = \"alpha\"\nversion = \"0.1.0-plugin-check\"\n");
}

#[test]
fn the_chooser_finds_nothing_when_only_unsupported_watch_files_exist() {
    let dir = write_tree(&[("setup.cfg", "[metadata]\nversion = 0.1.0\n"), ("setup.py", "setup()\n")]);
    let manifest = watching(&["pyproject.toml", "setup.cfg", "setup.py"]);
    assert!(choose_version_bump(&manifest, dir.path()).is_none());
}
