//! The bulk walk's `resolutionFacts` trailer is written only by an extractor
//! that has facts: the toy plugin keeps the `Extractor` default (`None`), so
//! its stream is nodes and edges only. The trailer of an extractor that has
//! facts is pinned by the TypeScript plugin's own tests
//! (`plugins/typescript/tests/resolution_delta.rs`).

use std::process::Command;

/// Control: write the trailer unconditionally in `run::bulk_index` (as
/// `{"resolutionFacts": null}` when there are none).
#[test]
fn a_plugin_without_facts_writes_no_trailer() {
    let root = tempfile_dir();
    std::fs::write(root.join("a.toy"), "fn a\ncall a\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_g-mesh-plugin-toy"))
        .arg("--bulk-index")
        .arg(&root)
        .output()
        .expect("the toy plugin runs");
    let _ = std::fs::remove_dir_all(&root);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<serde_json::Value> =
        stdout.lines().map(|line| serde_json::from_str(line).expect("every line is JSON")).collect();
    assert!(!lines.is_empty(), "the walk streamed a.toy");
    for line in &lines {
        assert!(line.get("resolutionFacts").is_none(), "no trailer: {line}");
        assert!(line.get("id").is_some(), "every line is a node or an edge: {line}");
    }
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("g-mesh-sdk-bulk-trailer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
