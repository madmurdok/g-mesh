//! `scripts/module-cycles.py` over core's real `src`: the module edges the
//! core cycle was broken at stay absent, and the largest cycle stays small.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The edges whose removal broke core's large module cycle and the
/// `storage`/`graph`/`embedding` cycle: storage reaches graph and embedding
/// only through the `model` leaf, the `link` module and the vector seams;
/// `mcp` reaches its host only through the `McpHost` trait it owns.
const REMOVED_EDGES: [&str; 16] = [
    "project_walk->watcher",
    "protocol->graph",
    "shim->mcp",
    "languages->daemon",
    "storage->languages",
    "storage->daemon",
    "config->storage",
    "embedding->daemon",
    "storage->graph",
    "storage->embedding",
    "graph->storage",
    "link->storage",
    "model->storage",
    "model->graph",
    "plugin_manifest->graph",
    "mcp->daemon",
];

/// GM-707: `watcher` keeps only the event source and `ingest` sits below
/// `daemon`, so neither reaches up into the daemon (`select_with` lives in
/// `ingest::select`), `watcher` no longer reaches `protocol` or `embedding`,
/// `ingest` does not reach back into `watcher`, and `cli` reads
/// `mtime_millis` from `fsutil`, not `watcher`.
const INGEST_SPLIT_ABSENT_EDGES: [&str; 6] = [
    "watcher->daemon",
    "watcher->protocol",
    "watcher->embedding",
    "ingest->daemon",
    "ingest->watcher",
    "cli->watcher",
];

/// GM-707: no `crate::daemon` path anywhere under `watcher/` or `ingest/`,
/// tests included (`test_plugin` moved below both).
const INGEST_SPLIT_ABSENT_EDGES_WITH_TESTS: [&str; 2] = ["watcher->daemon", "ingest->daemon"];

fn repo_root() -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/..")).to_path_buf()
}

fn module_cycles(src: &Path, args: &[&str]) -> Output {
    Command::new("python3")
        .arg(repo_root().join("scripts/module-cycles.py"))
        .arg("--src")
        .arg(src)
        .args(args)
        .current_dir(repo_root())
        .output()
        .expect("python3 must be on PATH")
}

fn describe(output: &Output) -> String {
    format!(
        "status: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn absent_args(edges: &[&str]) -> Vec<String> {
    edges.iter().flat_map(|edge| ["--absent".to_string(), (*edge).to_string()]).collect()
}

#[test]
fn the_removed_core_edges_stay_absent_and_core_has_no_module_cycle() {
    let mut args = absent_args(&REMOVED_EDGES);
    args.extend(["--max-scc".to_string(), "1".to_string()]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let output = module_cycles(&repo_root().join("core/src"), &args);

    assert!(output.status.success(), "{}", describe(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for edge in REMOVED_EDGES {
        assert!(
            stdout.contains(&format!("ok: edge {edge} absent")),
            "{edge} was not checked\n{}",
            describe(&output)
        );
    }
}

fn assert_absent(edges: &[&str], extra: &[&str]) {
    let mut args = absent_args(edges);
    args.extend(extra.iter().map(|arg| (*arg).to_string()));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    let output = module_cycles(&repo_root().join("core/src"), &args);

    assert!(output.status.success(), "{}", describe(&output));
    let stdout = String::from_utf8_lossy(&output.stdout);
    for edge in edges {
        assert!(
            stdout.contains(&format!("ok: edge {edge} absent")),
            "{edge} was not checked\n{}",
            describe(&output)
        );
    }
}

#[test]
fn watcher_and_ingest_never_reach_into_the_daemon_and_cli_never_into_the_watcher() {
    assert_absent(&INGEST_SPLIT_ABSENT_EDGES, &[]);
}

#[test]
fn no_test_under_watcher_or_ingest_reaches_into_the_daemon() {
    assert_absent(&INGEST_SPLIT_ABSENT_EDGES_WITH_TESTS, &["--include-tests"]);
}

/// The check above passes vacuously if the script cannot see an edge; on a
/// two-module cycle it must report both the edge and the cycle size.
#[test]
fn the_checker_fails_on_a_present_edge_and_an_oversized_cycle() {
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("lib.rs"), "mod a;\nmod b;\n").unwrap();
    fs::write(src.path().join("a.rs"), "use crate::b::Y;\n").unwrap();
    fs::write(src.path().join("b.rs"), "use crate::a::X;\n").unwrap();

    let edge = module_cycles(src.path(), &["--absent", "a->b"]);
    assert_eq!(edge.status.code(), Some(1), "{}", describe(&edge));
    assert!(String::from_utf8_lossy(&edge.stdout).contains("FAIL: edge a->b present"), "{}", describe(&edge));

    let size = module_cycles(src.path(), &["--max-scc", "1"]);
    assert_eq!(size.status.code(), Some(1), "{}", describe(&size));
    assert!(
        String::from_utf8_lossy(&size.stdout).contains("FAIL: largest SCC has 2 modules"),
        "{}",
        describe(&size)
    );
}

/// Writes `files` (relative path, contents) under a fresh temp dir with a `lib.rs` that declares
/// nothing: the script reads every other `.rs` file by its path.
fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("lib.rs"), "").unwrap();
    for (rel, text) in files {
        let path = src.path().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    src
}

fn layers(src: &Path) -> (Output, String) {
    let output = module_cycles(src, &["--layers"]);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    (output, stdout)
}

#[test]
fn the_real_tree_has_no_upward_crate_edge() {
    let (output, stdout) = layers(&repo_root().join("core/src"));

    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout.contains("ok: no upward crate edges"), "{}", describe(&output));
}

#[test]
fn layers_fails_on_an_upward_production_edge_and_names_it() {
    let src = fixture(&[("config.rs", "use crate::daemon::Thing;\n"), ("daemon.rs", "pub struct Thing;\n")]);

    let (output, stdout) = layers(src.path());

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let line = stdout
        .lines()
        .find(|l| l.contains("upward edge config -> daemon"))
        .unwrap_or_else(|| panic!("{}", describe(&output)));
    assert!(line.contains("(g-mesh-base -> g-mesh)"), "{line}");
    assert!(line.contains("prod"), "{line}");
}

#[test]
fn layers_fails_on_an_upward_edge_that_only_test_code_has() {
    let in_cfg_test = fixture(&[
        ("config.rs", "#[cfg(test)]\nmod tests {\n    use crate::daemon::Thing;\n}\n"),
        ("daemon.rs", ""),
    ]);
    let (output, stdout) = layers(in_cfg_test.path());
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let line = stdout
        .lines()
        .find(|l| l.contains("upward edge config -> daemon"))
        .unwrap_or_else(|| panic!("{}", describe(&output)));
    assert!(line.contains("test"), "{line}");

    let in_tests_file = fixture(&[
        ("config/mod.rs", ""),
        ("config/tests.rs", "use crate::daemon::Thing;\n"),
        ("daemon.rs", ""),
    ]);
    let (output, stdout) = layers(in_tests_file.path());
    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    let line = stdout
        .lines()
        .find(|l| l.contains("upward edge config::tests -> daemon"))
        .unwrap_or_else(|| panic!("{}", describe(&output)));
    assert!(line.contains("test"), "{line}");
}

#[test]
fn layers_fails_on_a_module_the_crate_map_does_not_cover() {
    let src = fixture(&[("config.rs", ""), ("not_in_the_map.rs", "")]);

    let (output, stdout) = layers(src.path());

    assert_eq!(output.status.code(), Some(1), "{}", describe(&output));
    assert!(stdout.contains("FAIL: module not_in_the_map is not in the crate map"), "{}", describe(&output));
}

#[test]
fn layers_passes_on_a_downward_edge() {
    let src = fixture(&[("daemon.rs", "use crate::config::Thing;\n"), ("config.rs", "pub struct Thing;\n")]);

    let (output, stdout) = layers(src.path());

    assert!(output.status.success(), "{}", describe(&output));
    assert!(stdout.contains("ok: no upward crate edges"), "{}", describe(&output));
}
