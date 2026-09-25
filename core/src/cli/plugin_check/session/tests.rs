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
    let conn = open_index().unwrap();
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
