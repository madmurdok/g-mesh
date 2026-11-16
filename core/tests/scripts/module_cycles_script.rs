//! `scripts/module-cycles.py` over core's real `src`: the module edges the
//! core cycle was broken at stay absent, and the largest cycle stays small.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The edges whose removal broke core's large module cycle.
const REMOVED_EDGES: [&str; 8] = [
    "project_walk->watcher",
    "protocol->graph",
    "shim->mcp",
    "languages->daemon",
    "storage->languages",
    "storage->daemon",
    "config->storage",
    "embedding->daemon",
];

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
fn the_removed_core_edges_stay_absent_and_no_cycle_exceeds_five_modules() {
    let mut args = absent_args(&REMOVED_EDGES);
    args.extend(["--max-scc".to_string(), "5".to_string()]);
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
