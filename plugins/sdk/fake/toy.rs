//! The **toy** persona (`--toy <defect>`, `core/tests/plugin_check.rs`): a
//! conformant plugin for the toy `.fk` language, plus one deliberate defect.
//!
//! It is the fake `g-mesh plugins check` is tested against, so the conformant
//! baseline (`none`) passes every check and each other defect is that
//! baseline plus one change that breaks exactly one rule. See
//! `core/tests/plugin_check.rs`'s module doc for why it is one program.
//!
//! The language: `fn NAME` declares a function, `call NAME` calls one
//! declared in the same file, and `use FILE NAME` references a name from
//! another file (a `pending_symbol` placeholder, upgraded to a resolved
//! cross-file edge by the semantic pass).
//!
//! Lengths and columns are counted in UTF-16 code units, as the Node fake
//! this replaces counted them.
//!
//! # Defects
//!
//! - `shape`: placeholders carry no `target` (and a `:`-spelled qualified name).
//! - `stream-order-late`: the bulk walk emits each file's edges before its
//!   declarations.
//! - `stream-order-cross-file`: an extra unresolved edge onto another file's node.
//! - `same-file-rule`: same-file `CALLS` edges are `resolved: false`.
//! - `bulk-repeat`: `a.fk`'s bulk walk adds a node whose id holds the pid.
//! - `whitespace-moves-range`: the `File` node's end column is the text length.
//! - `deletes-unknown`: every non-empty diff deletes `never-emitted-node`.
//! - `incremental-ids`: declarations are `#func:` ids outside the bulk walk.
//! - `stale-ranges`: a diff only upserts ids it never sent before.
//! - `defines-from-symbol`: a `DEFINES` edge from the first function to the second.
//! - `language`: declarations say `fake-dialect`.
//! - `container`: `a.fk`'s bulk walk adds a `nativeKind: "container"` node.
//! - `diff-other-file`: diffs for any file but `b.fk` upsert `b.fk`'s `File` node.
//! - `eager-engine` / `undeclared-engine`: the semantic engine marker is
//!   written at start rather than at the first `semanticPass`.
//! - `hang`: `fileChanged` is never answered.
//! - `bulk-hang`: the bulk walk never finishes.
//! - `bulk-dies` / `bulk-dies-silently`: the bulk walk exits 3 having written
//!   nothing to stdout - with a line on stderr, or without one.
//!
//! The semantic engine marker is a line with this process's pid appended to
//! `$G_MESH_PLUGIN_CHECK_MARKER_DIR/semantic-engine-started`.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process;
use std::thread;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::{append, handshake_as, method_of, serve, write, Out};

const LANGUAGE: &str = "fake";
const PLUGIN_VERSION: &str = "0.0.0-fake";
const MARKER_DIR_ENV: &str = "G_MESH_PLUGIN_CHECK_MARKER_DIR";
const ENGINE_MARKER: &str = "semantic-engine-started";

/// Which walk an extraction is for: several defects exist in only one.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// The one-shot `--bulk-index` walk.
    Bulk,
    /// `fileChanged` / `semanticPass` on the long-lived process.
    Control,
}

struct Rows {
    nodes: Vec<Value>,
    edges: Vec<Value>,
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn is_word(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// UTF-16 column of the first `needle` in `line`.
fn column_of(line: &str, needle: &str) -> usize {
    line.find(needle).map(|at| utf16_len(&line[..at])).unwrap_or(0)
}

fn node(
    id: &str,
    kind: &str,
    name: &str,
    qualified_name: &str,
    file_path: &str,
    r: [usize; 4],
) -> Map<String, Value> {
    let mut node = Map::new();
    node.insert("id".into(), json!(id));
    node.insert("kind".into(), json!(kind));
    node.insert("name".into(), json!(name));
    node.insert("qualifiedName".into(), json!(qualified_name));
    node.insert("filePath".into(), json!(file_path));
    node.insert(
        "range".into(),
        json!({ "start": { "line": r[0], "col": r[1] }, "end": { "line": r[2], "col": r[3] } }),
    );
    node.insert("visibility".into(), json!("public"));
    node.insert("language".into(), json!(LANGUAGE));
    node
}

fn edge(from_id: &str, to_id: &str, kind: &str, resolved: bool) -> Map<String, Value> {
    // The `incremental-ids` defect renames node ids only: edge ids are spelled
    // with the bulk scheme either way, so the control path keeps deleting the
    // bulk rows' edges and nothing but the node ids diverges.
    let id = format!("{from_id} -{kind}-> {to_id}").replace("#func:", "#fn:");
    let mut edge = Map::new();
    edge.insert("id".into(), json!(id));
    edge.insert("fromId".into(), json!(from_id));
    edge.insert("toId".into(), json!(to_id));
    edge.insert("kind".into(), json!(kind));
    edge.insert("source".into(), json!("syntactic"));
    edge.insert("engine".into(), json!("fake-parser"));
    edge.insert("resolved".into(), json!(resolved));
    edge
}

/// The whitespace-separated words of `line` when it is exactly `keyword`
/// followed by `arity` more words.
fn statement<'a>(line: &'a str, keyword: &str, arity: usize) -> Option<Vec<&'a str>> {
    let words: Vec<&str> = line.split_whitespace().collect();
    (words.len() == arity + 1 && words[0] == keyword).then(|| words[1..].to_vec())
}

fn declaration(line: &str) -> Option<&str> {
    statement(line, "fn", 1).map(|w| w[0]).filter(|name| is_word(name))
}

fn extract(defect: &str, file_path: &str, text: &str, mode: Mode) -> Rows {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let tail = utf16_len(text.rsplit('\n').next().unwrap_or_default());
    let file_id = format!("{file_path}#file");
    // Conformant: the file range ends at (newline count, length of the final
    // unterminated line) - which a space before the last newline cannot move.
    let end_col = if defect == "whitespace-moves-range" { utf16_len(text) } else { tail };
    let base_name = file_path.rsplit('/').next().unwrap_or(file_path);
    let mut file = node(&file_id, "File", base_name, file_path, file_path, [0, 0, lines.len() - 1, end_col]);
    file.insert("visibility".into(), json!("file"));
    nodes.push(file);

    let fn_prefix = if defect == "incremental-ids" && mode == Mode::Control { "#func:" } else { "#fn:" };
    let fn_id = |name: &str| format!("{file_path}{fn_prefix}{name}");
    let declared: Vec<&str> = lines.iter().filter_map(|line| declaration(line)).collect();

    let mut fns = Vec::new();
    let mut current = file_id.clone();
    for (row, line) in lines.iter().enumerate() {
        let end = utf16_len(line.trim_end());
        if let Some(name) = declaration(line) {
            let id = fn_id(name);
            current = id.clone();
            fns.push(id.clone());
            let mut decl =
                node(&id, "Function", name, name, file_path, [row, column_of(line, "fn"), row, end]);
            if defect == "language" {
                decl.insert("language".into(), json!("fake-dialect"));
            }
            nodes.push(decl);
            edges.push(edge(&file_id, &id, "DEFINES", true));
            edges.push(edge(&file_id, &id, "EXPORTS", true));
        } else if let Some(name) =
            statement(line, "call", 1).map(|w| w[0]).filter(|name| is_word(name) && declared.contains(name))
        {
            edges.push(edge(&current, &fn_id(name), "CALLS", defect != "same-file-rule"));
        } else if let Some(words) = statement(line, "use", 2).filter(|w| is_word(w[1])) {
            let (target, name) = (words[0], words[1]);
            let id = format!("{file_path}#use:{target}#{name}");
            let qualified_name =
                if defect == "shape" { format!("{target}:{name}") } else { format!("{target}#{name}") };
            let mut placeholder = node(
                &id,
                "Module",
                name,
                &qualified_name,
                file_path,
                [row, column_of(line, "use"), row, end],
            );
            placeholder.insert("visibility".into(), json!("file"));
            placeholder.insert("nativeKind".into(), json!("pending_symbol"));
            if defect != "shape" {
                placeholder
                    .insert("target".into(), json!({ "scope": { "file": target }, "key": { "name": name } }));
            }
            nodes.push(placeholder);
            edges.push(edge(&current, &id, "REFERENCES", false));
            if defect == "stream-order-cross-file" {
                edges.push(edge(&current, &format!("{target}#fn:{name}"), "REFERENCES", false));
            }
        }
    }

    if defect == "defines-from-symbol" && fns.len() >= 2 {
        edges.push(edge(&fns[0], &fns[1], "DEFINES", true));
    }
    if defect == "bulk-repeat" && mode == Mode::Bulk && file_path == "a.fk" {
        // No edge onto it: the control path never learns this id, so an edge
        // from the File node would pin that node against deletion.
        let id = format!("{file_path}#var:run-{}", process::id());
        nodes.push(node(&id, "Variable", "run", "run", file_path, [0, 0, 0, 0]));
    }
    if defect == "container" && mode == Mode::Bulk && file_path == "a.fk" {
        let mut container = node("fake-container-pkg", "Module", "pkg", "pkg", "", [0, 0, 0, 0]);
        container.insert("nativeKind".into(), json!("container"));
        nodes.push(container);
    }
    Rows {
        nodes: nodes.into_iter().map(Value::Object).collect(),
        edges: edges.into_iter().map(Value::Object).collect(),
    }
}

/// Every `.fk` file under `dir`, relative to `root` with `/` separators,
/// each directory's entries in name order.
fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out);
        } else if entry.file_name().to_string_lossy().ends_with(".fk") {
            let relative = path.strip_prefix(root).unwrap_or(&path);
            let parts: Vec<String> =
                relative.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            out.push(parts.join("/"));
        }
    }
}

fn walk_all(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

/// A missing or unreadable file reads as empty, as a deleted one should.
fn read(root: &Path, file_path: &str) -> String {
    fs::read_to_string(root.join(file_path)).unwrap_or_default()
}

fn start_semantic_engine(started: &mut bool) {
    if *started {
        return;
    }
    *started = true;
    if let Some(dir) = std::env::var_os(MARKER_DIR_ENV).filter(|dir| !dir.is_empty()) {
        append(&Path::new(&dir).join(ENGINE_MARKER), &process::id().to_string());
    }
}

/// The toy persona's entry point: `bulk_root` selects the walk, otherwise
/// `root` is the project the control plane serves.
pub(super) fn run(defect: &str, bulk_root: Option<&Path>, root: Option<&Path>) {
    if let Some(root) = bulk_root {
        process::exit(bulk(defect, root));
    }
    let root = root.map(Path::to_path_buf).unwrap_or_default();
    let mut engine_started = false;
    if defect == "eager-engine" || defect == "undeclared-engine" {
        start_semantic_engine(&mut engine_started);
    }

    let out: Out = std::sync::Arc::new(std::sync::Mutex::new(io::stdout()));
    handshake_as(&out, LANGUAGE, PLUGIN_VERSION);
    let mut cache: HashMap<String, Rows> = HashMap::new();
    serve(|message| {
        let id = match message.get("id") {
            Some(id) if !id.is_null() => id.clone(),
            _ => return,
        };
        let method = method_of(&message);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method.as_str() {
            "fileChanged" => {
                if defect == "hang" {
                    return;
                }
                let file_path = params.get("filePath").and_then(Value::as_str).unwrap_or_default();
                file_changed(defect, &root, &mut cache, file_path)
            }
            "semanticPass" => {
                start_semantic_engine(&mut engine_started);
                let paths: Vec<String> = params
                    .get("filePaths")
                    .and_then(Value::as_array)
                    .map(|paths| paths.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                semantic_pass(defect, &root, paths)
            }
            _ => json!({ "acknowledged": true }),
        };
        write(&out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    });
}

/// The bulk walk; returns the exit status.
fn bulk(defect: &str, root: &Path) -> i32 {
    match defect {
        "bulk-hang" => loop {
            thread::sleep(Duration::from_secs(1));
        },
        // A plugin that fails the way a real one does when its runtime cannot
        // even load it: a non-zero exit with nothing on stdout - with a word
        // on stderr, or (the second spelling) without one.
        "bulk-dies" => {
            eprintln!("fk-extractor: cannot open the toy grammar");
            return 3;
        }
        "bulk-dies-silently" => return 3,
        _ => {}
    }
    let mut stdout = io::stdout().lock();
    let mut out = |value: &Value| {
        let _ = writeln!(stdout, "{value}");
    };
    for file_path in walk_all(root) {
        let rows = extract(defect, &file_path, &read(root, &file_path), Mode::Bulk);
        if defect == "stream-order-late" {
            // The File node, then every edge, then the rest of the nodes.
            out(&rows.nodes[0]);
            rows.edges.iter().for_each(&mut out);
            rows.nodes[1..].iter().for_each(&mut out);
        } else {
            rows.nodes.iter().for_each(&mut out);
            rows.edges.iter().for_each(&mut out);
        }
    }
    let _ = stdout.flush();
    0
}

fn id_of(item: &Value) -> &str {
    item.get("id").and_then(Value::as_str).unwrap_or_default()
}

fn file_changed(defect: &str, root: &Path, cache: &mut HashMap<String, Rows>, file_path: &str) -> Value {
    let next = extract(defect, file_path, &read(root, file_path), Mode::Control);
    let previous = cache.remove(file_path).unwrap_or(Rows { nodes: Vec::new(), edges: Vec::new() });
    let mut upserts = [Vec::new(), Vec::new()];
    let mut deletes = [Vec::new(), Vec::new()];
    for (slot, (before, after)) in
        [(&previous.nodes, &next.nodes), (&previous.edges, &next.edges)].into_iter().enumerate()
    {
        // Gone ids are deleted; new or changed ones are upserted in place (by
        // id). (The TS plugin deletes and re-upserts a changed node instead;
        // both are conformant.)
        let before_by_id: BTreeMap<&str, &Value> = before.iter().map(|item| (id_of(item), item)).collect();
        let after_by_id: BTreeMap<&str, &Value> = after.iter().map(|item| (id_of(item), item)).collect();
        for item in before {
            if !after_by_id.contains_key(id_of(item)) {
                deletes[slot].push(json!(id_of(item)));
            }
        }
        for item in after {
            // The `stale-ranges` defect only sends ids it has never sent
            // before, so a declaration that is still there but has moved or
            // grown is never re-sent - every id check still passes, and the
            // index keeps the old range.
            let changed = match before_by_id.get(id_of(item)) {
                None => true,
                Some(old) => defect != "stale-ranges" && *old != item,
            };
            if changed {
                upserts[slot].push(item.clone());
            }
        }
    }
    cache.insert(file_path.to_string(), next);
    let [mut upsert_nodes, upsert_edges] = upserts;
    let [mut delete_node_ids, delete_edge_ids] = deletes;
    let empty = upsert_nodes.is_empty()
        && delete_node_ids.is_empty()
        && upsert_edges.is_empty()
        && delete_edge_ids.is_empty();
    if !empty && defect == "deletes-unknown" {
        delete_node_ids.push(json!("never-emitted-node"));
    }
    if !empty && defect == "diff-other-file" && file_path != "b.fk" {
        let other = extract(defect, "b.fk", &read(root, "b.fk"), Mode::Control);
        upsert_nodes.push(other.nodes[0].clone());
    }
    json!({
        "upsertNodes": upsert_nodes,
        "deleteNodeIds": delete_node_ids,
        "upsertEdges": upsert_edges,
        "deleteEdgeIds": delete_edge_ids,
    })
}

/// `(target, name)` of a placeholder id `<file>#use:<target>#<name>`.
fn placeholder_address(to_id: &str) -> Option<(&str, &str)> {
    let rest = &to_id[to_id.find("#use:")? + "#use:".len()..];
    let (target, name) = rest.rsplit_once('#')?;
    (!target.is_empty() && is_word(name)).then_some((target, name))
}

fn semantic_pass(defect: &str, root: &Path, file_paths: Vec<String>) -> Value {
    let files = if file_paths.is_empty() { walk_all(root) } else { file_paths };
    let mut upsert_edges = Vec::new();
    for file_path in files {
        let rows = extract(defect, &file_path, &read(root, &file_path), Mode::Control);
        for e in &rows.edges {
            let to_id = e.get("toId").and_then(Value::as_str).unwrap_or_default();
            let Some((target, name)) = placeholder_address(to_id) else { continue };
            let from_id = e.get("fromId").and_then(Value::as_str).unwrap_or_default();
            // The semantic tier's answer crosses files, as a semantic answer may.
            let mut upgraded = edge(from_id, &format!("{target}#fn:{name}"), "REFERENCES", true);
            upgraded.insert("source".into(), json!("semantic"));
            upgraded.insert("engine".into(), json!("fake-types"));
            upsert_edges.push(Value::Object(upgraded));
        }
    }
    json!({ "upsertNodes": [], "deleteNodeIds": [], "upsertEdges": upsert_edges, "deleteEdgeIds": [] })
}
