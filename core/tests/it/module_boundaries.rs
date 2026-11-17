//! Names that may be spelled only on one side of a module boundary:
//! storage reaches graph and embedding through `model`, `link` and
//! its own seams, a row read from SQL becomes an `EmbedInput` only through
//! `EmbedInput::from_row`, and the MCP byte budget lives in `mcp`. The
//! module edges themselves are `scripts/module-cycles.py`'s
//! (`core/tests/scripts/module_cycles_script.rs`); this checks the names.
//!
//! Non-test code only, by the rule `state_dir.rs`'s spelling check uses: a
//! file under a `tests` directory or named `tests.rs`/`*_tests.rs` is test
//! code, and so is everything from a `#[cfg(test)] mod tests {` on; `//`
//! comment lines (doc comments included) are skipped.

use std::fs;
use std::path::{Path, PathBuf};

fn src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn is_test_file(rel: &str) -> bool {
    rel.ends_with("/tests.rs")
        || rel == "tests.rs"
        || rel.ends_with("_tests.rs")
        || rel.split('/').any(|part| part == "tests")
}

/// `source` up to its inline `#[cfg(test)] mod tests {`, without `//`
/// comment lines, numbered from 1.
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut kept = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
        if trimmed.starts_with("#[cfg(")
            && trimmed.contains("test")
            && (next == "mod tests {" || next == "pub(crate) mod tests {")
        {
            break;
        }
        if !trimmed.starts_with("//") {
            kept.push((i + 1, *line));
        }
    }
    kept
}

/// `rel:line: needle` for every non-test line of a file under `src/` that
/// `scope` accepts and that spells one of `needles`.
fn spellings(scope: impl Fn(&str) -> bool, needles: &[&str]) -> Vec<String> {
    let src = src();
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();
    let mut found = Vec::new();
    for file in files {
        let rel = file.strip_prefix(&src).unwrap().to_string_lossy().replace('\\', "/");
        if is_test_file(&rel) || !scope(&rel) {
            continue;
        }
        let source = fs::read_to_string(&file).unwrap();
        for (number, line) in production_lines(&source) {
            for needle in needles {
                if line.contains(needle) {
                    found.push(format!("{rel}:{number}: {needle}"));
                }
            }
        }
    }
    found
}

/// The scan reads what it claims to: the needles below do occur in
/// production code where they belong, so an empty result elsewhere is not
/// a scan that saw nothing.
#[test]
fn the_scan_finds_the_names_where_they_belong() {
    assert!(!spellings(|rel| rel.starts_with("embedding/"), &["EmbeddingPipeline"]).is_empty());
    assert!(!spellings(|rel| rel == "embedding/text.rs", &["EmbedInput {"]).is_empty());
    assert!(!spellings(|rel| rel.starts_with("mcp/"), &["MAX_RESPONSE_BYTES", "fit_budget"]).is_empty());
    assert!(!spellings(|rel| rel.starts_with("storage/"), &["crate::model"]).is_empty());
}

/// Storage names nothing of graph or embedding: it reads embedded text
/// through `NodeText` and writes vectors through `VectorWrite`.
/// Control: put back `use crate::graph::queries;` in `storage/write.rs`.
#[test]
fn storage_names_neither_graph_nor_embedding() {
    let found = spellings(
        |rel| rel.starts_with("storage/"),
        &[
            "crate::graph",
            "crate::embedding",
            "EmbeddingPipeline",
            "ComputedBatch",
            "EmbedInput",
            "node_text(",
        ],
    );
    assert!(found.is_empty(), "storage spells graph/embedding names:\n{}", found.join("\n"));
}

/// Outside `embedding/text.rs`, an `EmbedInput` is built only by
/// `EmbedInput::from_row` (or `From<&NodeRecord>`), never as a struct
/// literal that could read the columns in another order.
/// Control: build the `EmbedInput` in `embedding/backfill.rs` as a literal
/// instead of `EmbedInput::from_row(row)`.
#[test]
fn an_embed_input_is_built_only_in_embedding_text() {
    let found = spellings(|rel| rel != "embedding/text.rs", &["EmbedInput {"]);
    assert!(found.is_empty(), "EmbedInput literals outside embedding/text.rs:\n{}", found.join("\n"));
}

/// The MCP response byte budget is spelled only under `mcp/`
/// (`mcp::response_bound`).
/// Control: move `MAX_RESPONSE_BYTES` back into `graph/pagination.rs` (and
/// use it there).
#[test]
fn the_response_byte_budget_lives_in_mcp() {
    let found =
        spellings(|rel| !rel.starts_with("mcp/"), &["MAX_RESPONSE_BYTES", "bound_page", "fit_budget"]);
    assert!(found.is_empty(), "the MCP byte budget is spelled outside mcp/:\n{}", found.join("\n"));
}
