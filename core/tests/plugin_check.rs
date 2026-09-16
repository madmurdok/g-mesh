//! `g-mesh plugins check` - the conformance kit (GM-276) - driven through the
//! real binary, against plugins that are wrong on purpose.
//!
//! # One fake, one defect, one failing check
//!
//! A check that has never been seen failing proves nothing: it may simply not
//! look at what it claims to. So every check the kit reports has a fake plugin
//! here that breaks exactly that rule, and each test asserts two things - that
//! check fails, and *no other* check does. The second half matters as much as
//! the first: a defect that trips three checks shows the checks overlap, and a
//! future plugin author reading a report could not tell which rule they broke.
//!
//! All the fakes are one Node program ([`FAKE_PLUGIN`]) with a `DEFECT`
//! switch, rather than one hand-written plugin per check. The conformant
//! baseline (`DEFECT = "none"`) is the thing every defect is measured
//! against, and one program guarantees each fake is that baseline plus one
//! change - not a second implementation that might be wrong in some other way
//! too. The baseline passing everything (`the_conformant_fake_passes_every_check`)
//! is what makes "only this check fails" meaningful for the rest.
//!
//! The fake speaks a toy language over `tests/fixtures/plugin_check/fake/`:
//! `fn NAME` declares a function, `call NAME` calls one in the same file, and
//! `use FILE NAME` references a name from another file (a `pending_symbol`
//! placeholder). That is enough to produce every kind of row the contract
//! talks about - `File` nodes, declarations, `DEFINES`/`EXPORTS`, a
//! same-file resolved edge, a placeholder edge, and (in its semantic pass) a
//! cross-file upgrade.
//!
//! # And the real plugin
//!
//! `the_typescript_plugin_passes_on_a_small_typescript_fixture` runs the
//! bundled TS plugin (built by `core/build.rs`) over
//! `../plugins/typescript/conformance/project/` - a cross-file import, both
//! re-export forms, a namespace member use (so its semantic pass actually
//! starts tsserver and the lazy-engine check has a marker to judge), an
//! overloaded function and an interface implementation. This is the same
//! fixture CI's per-plugin conformance job runs (GM-277's own module doc,
//! `plugins/typescript/conformance/expect.toml`) - one fixture rather than
//! two diverging copies, since `core/tests/fixtures/plugin_check/typescript/`
//! used to exist purely for this test and CI had nothing to iterate.
//!
//! `the_go_plugin_passes_on_its_own_fixture` and
//! `the_go_plugin_satisfies_its_own_expectations_file` do the same for the
//! bundled Go plugin (also built by `core/build.rs`) over
//! `../plugins/go/conformance/{project,expect.toml}` - a multi-file package,
//! a second package reached through an import, an external `_test` package
//! and a `go.work` naming two modules. They are what puts the Go plugin's
//! conformance run in core's own CI, which is what the design doc asks for
//! ("The same fixtures run in core's CI for every bundled plugin"), and they
//! are GM-280's end-to-end evidence that the container-scoped placeholders
//! that plugin emits are addresses core's linker actually resolves.
//!
//! # `--expect` (GM-277)
//!
//! `the_typescript_plugin_satisfies_its_own_expectations_file` runs the same
//! fixture with `--expect plugins/typescript/conformance/expect.toml` and
//! asserts every expectation passes - the acceptance criterion "the TS
//! fixture passes". The rest of GM-277's own tests
//! (`a_wrong_expectation_reports_a_readable_diff`,
//! `an_ambiguous_symbol_fails_with_its_candidates`,
//! `an_unknown_expectation_key_is_a_hard_parse_error`,
//! `a_namespace_import_caller_needs_the_semantic_pass_to_resolve`) use small,
//! purpose-built fixtures of their own - the toy `fake` language for the
//! three that only exercise `expectations.rs`'s own logic (fast, no tsserver),
//! and the real TS plugin with a `semantic_pass = false` copy of its manifest
//! for the one that has to prove a real semantic-only fact.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_g-mesh");

/// Every check id the kit reports, in report order - asserted in full on
/// every run, so a check silently dropping out of the report fails a test
/// rather than passing every "only X fails" assertion vacuously.
const ALL_CHECKS: [&str; 15] = [
    "session",
    "shape",
    "stream-order",
    "same-file-rule",
    "id-stability.bulk-repeat",
    "id-stability.whitespace-edit",
    "id-stability.deletes-known",
    "id-stability.incremental-matches-bulk",
    "id-stability.declaration-edit-applies",
    "ownership.defines-exports-from-file",
    "ownership.language",
    "ownership.no-container",
    "ownership.diff-stays-in-file",
    "capabilities.semantic-pass-undeclared",
    "capabilities.semantic-engine-lazy",
];

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin_check")
}

/// The bundled TS plugin's own directory - `plugins/typescript`, a sibling of
/// `core/`, named after its manifest's `language` as `read_manifest`
/// requires.
fn ts_plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/typescript")
}

/// The TS plugin's own conformance fixture and expectations file (GM-277) -
/// `plugins/typescript/conformance/{project,expect.toml}`, the same pair
/// CI's per-plugin job runs. See this file's module doc for why this is the
/// one TS fixture rather than a second copy under `tests/fixtures/`.
fn ts_conformance_project() -> PathBuf {
    ts_plugin_dir().join("conformance/project")
}

fn ts_conformance_expect() -> PathBuf {
    ts_plugin_dir().join("conformance/expect.toml")
}

/// The bundled Go plugin (GM-279's scaffold, GM-280's real extractor) and its
/// own conformance pair, laid out exactly like the TS plugin's: the binary
/// `core/build.rs` builds, the fixture, and the expectations file.
fn go_plugin_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugins/go")
}

fn go_conformance_project() -> PathBuf {
    go_plugin_dir().join("conformance/project")
}

fn go_conformance_expect() -> PathBuf {
    go_plugin_dir().join("conformance/expect.toml")
}

/// The fake plugin: a conformant plugin for the toy `.fk` language, plus one
/// deliberate defect selected by `DEFECT`. See this file's module doc.
const FAKE_PLUGIN: &str = r##"// Generated by core/tests/plugin_check.rs - a deliberately configurable fake plugin.
const fs = require("fs");
const path = require("path");

const DEFECT = "__DEFECT__";
const LANGUAGE = "fake";
const MARKER_DIR = process.env.G_MESH_PLUGIN_CHECK_MARKER_DIR || "";

let engineStarted = false;
function startSemanticEngine() {
  if (engineStarted) return;
  engineStarted = true;
  if (MARKER_DIR) fs.appendFileSync(path.join(MARKER_DIR, "semantic-engine-started"), process.pid + "\n");
}

function node(id, kind, name, qualifiedName, filePath, r, extra) {
  return Object.assign(
    {
      id, kind, name, qualifiedName, filePath,
      range: { start: { line: r[0], col: r[1] }, end: { line: r[2], col: r[3] } },
      visibility: "public",
      language: LANGUAGE,
    },
    extra || {},
  );
}

function edge(fromId, toId, kind, resolved, extra) {
  // The `incremental-ids` defect renames node ids only: edge ids are spelled
  // with the bulk scheme either way, so the control path keeps deleting the
  // bulk rows' edges and nothing but the node ids diverges.
  const id = (fromId + " -" + kind + "-> " + toId).split("#func:").join("#fn:");
  return Object.assign(
    { id, fromId, toId, kind, source: "syntactic", engine: "fake-parser", resolved },
    extra || {},
  );
}

// mode: "bulk" (the one-shot --bulk-index walk) or "control" (fileChanged /
// semanticPass on the long-lived process) - several defects exist in only one.
function extract(filePath, text, mode) {
  const nodes = [];
  const edges = [];
  const lines = text.split("\n");
  const tail = text.length - (text.lastIndexOf("\n") + 1);
  const fileId = filePath + "#file";
  // Conformant: the file range ends at (newline count, length of the final
  // unterminated line) - which a space before the last newline cannot move.
  const endCol = DEFECT === "whitespace-moves-range" ? text.length : tail;
  nodes.push(node(fileId, "File", path.basename(filePath), filePath, filePath, [0, 0, lines.length - 1, endCol],
    { visibility: "file" }));

  const fnPrefix = DEFECT === "incremental-ids" && mode === "control" ? "#func:" : "#fn:";
  const fnId = (name) => filePath + fnPrefix + name;
  const declared = new Set();
  for (const line of lines) {
    const m = /^\s*fn\s+(\w+)\s*$/.exec(line);
    if (m) declared.add(m[1]);
  }

  const fns = [];
  let current = fileId;
  lines.forEach((line, row) => {
    const end = line.trimEnd().length;
    let m;
    if ((m = /^\s*fn\s+(\w+)\s*$/.exec(line))) {
      const id = fnId(m[1]);
      current = id;
      fns.push(id);
      const language = DEFECT === "language" ? "fake-dialect" : LANGUAGE;
      nodes.push(node(id, "Function", m[1], m[1], filePath, [row, line.indexOf("fn"), row, end], { language }));
      edges.push(edge(fileId, id, "DEFINES", true));
      edges.push(edge(fileId, id, "EXPORTS", true));
    } else if ((m = /^\s*call\s+(\w+)\s*$/.exec(line)) && declared.has(m[1])) {
      edges.push(edge(current, fnId(m[1]), "CALLS", DEFECT !== "same-file-rule"));
    } else if ((m = /^\s*use\s+(\S+)\s+(\w+)\s*$/.exec(line))) {
      const [, target, name] = m;
      const id = filePath + "#use:" + target + "#" + name;
      const extra = DEFECT === "shape"
        ? { visibility: "file", nativeKind: "pending_symbol" }
        : { visibility: "file", nativeKind: "pending_symbol", target: { scope: { file: target }, key: { name } } };
      const qualifiedName = DEFECT === "shape" ? target + ":" + name : target + "#" + name;
      nodes.push(node(id, "Module", name, qualifiedName, filePath, [row, line.indexOf("use"), row, end], extra));
      edges.push(edge(current, id, "REFERENCES", false));
      if (DEFECT === "stream-order-cross-file") {
        edges.push(edge(current, target + "#fn:" + name, "REFERENCES", false));
      }
    }
  });

  if (DEFECT === "defines-from-symbol" && fns.length >= 2) {
    edges.push(edge(fns[0], fns[1], "DEFINES", true));
  }
  if (DEFECT === "bulk-repeat" && mode === "bulk" && filePath === "a.fk") {
    // No edge onto it: the control path never learns this id, so an edge
    // from the File node would pin that node against deletion.
    nodes.push(node(filePath + "#var:run-" + process.pid, "Variable", "run", "run", filePath, [0, 0, 0, 0]));
  }
  if (DEFECT === "container" && mode === "bulk" && filePath === "a.fk") {
    nodes.push(node("fake-container-pkg", "Module", "pkg", "pkg", "", [0, 0, 0, 0], { nativeKind: "container" }));
  }
  return { nodes, edges };
}

function walk(root, dir, out) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    const abs = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(root, abs, out);
    else if (entry.name.endsWith(".fk")) out.push(path.relative(root, abs).split(path.sep).join("/"));
  }
  return out;
}

function read(root, filePath) {
  try {
    return fs.readFileSync(path.join(root, filePath), "utf8");
  } catch {
    return "";
  }
}

if (process.argv[2] === "--bulk-index") {
  const root = process.argv[3];
  if (DEFECT === "bulk-hang") {
    setInterval(() => {}, 1000);
    return;
  }
  const out = (value) => process.stdout.write(JSON.stringify(value) + "\n");
  for (const filePath of walk(root, root, [])) {
    const { nodes, edges } = extract(filePath, read(root, filePath), "bulk");
    if (DEFECT === "stream-order-late") {
      // The File node, then every edge, then the rest of the nodes.
      out(nodes[0]);
      edges.forEach(out);
      nodes.slice(1).forEach(out);
    } else {
      nodes.forEach(out);
      edges.forEach(out);
    }
  }
  return;
}

const root = process.argv[2];
if (DEFECT === "eager-engine" || DEFECT === "undeclared-engine") startSemanticEngine();

function writeFrame(message) {
  const body = Buffer.from(JSON.stringify(message), "utf8");
  process.stdout.write("Content-Length: " + body.length + "\r\n\r\n");
  process.stdout.write(body);
}

writeFrame({ protocolVersion: __PROTOCOL__, language: LANGUAGE, pluginVersion: "0.0.0-fake" });

const cache = new Map();

function fileChanged(filePath) {
  const next = extract(filePath, read(root, filePath), "control");
  const previous = cache.get(filePath) || { nodes: [], edges: [] };
  cache.set(filePath, next);
  const diff = { upsertNodes: [], deleteNodeIds: [], upsertEdges: [], deleteEdgeIds: [] };
  for (const [key, upsert, remove] of [
    ["nodes", diff.upsertNodes, diff.deleteNodeIds],
    ["edges", diff.upsertEdges, diff.deleteEdgeIds],
  ]) {
    // Gone ids are deleted; new or changed ones are upserted in place (by id).
    // (The TS plugin deletes and re-upserts a changed node instead; both are
    // conformant. This comment used to say the delete was refused by the
    // index's foreign keys - it was, because the kit's index enforced them by
    // accident, GM-292. It no longer does.)
    const before = new Map(previous[key].map((item) => [item.id, JSON.stringify(item)]));
    const after = new Map(next[key].map((item) => [item.id, JSON.stringify(item)]));
    for (const item of previous[key]) if (!after.has(item.id)) remove.push(item.id);
    // The `stale-ranges` defect only sends ids it has never sent before, so a
    // declaration that is still there but has moved or grown is never
    // re-sent - every id check still passes, and the index keeps the old range.
    const changed = DEFECT === "stale-ranges"
      ? (item) => !before.has(item.id)
      : (item) => before.get(item.id) !== after.get(item.id);
    for (const item of next[key]) if (changed(item)) upsert.push(item);
  }
  const empty = diff.upsertNodes.length + diff.deleteNodeIds.length + diff.upsertEdges.length + diff.deleteEdgeIds.length === 0;
  if (!empty && DEFECT === "deletes-unknown") diff.deleteNodeIds.push("never-emitted-node");
  if (!empty && DEFECT === "diff-other-file" && filePath !== "b.fk") {
    diff.upsertNodes.push(extract("b.fk", read(root, "b.fk"), "control").nodes[0]);
  }
  return diff;
}

function semanticPass(filePaths) {
  startSemanticEngine();
  const files = filePaths.length === 0 ? walk(root, root, []) : filePaths;
  const diff = { upsertNodes: [], deleteNodeIds: [], upsertEdges: [], deleteEdgeIds: [] };
  for (const filePath of files) {
    const { edges } = extract(filePath, read(root, filePath), "control");
    for (const e of edges) {
      const m = /#use:(.+)#(\w+)$/.exec(e.toId);
      if (!m) continue;
      // The semantic tier's answer crosses files, as a semantic answer may.
      diff.upsertEdges.push(edge(e.fromId, m[1] + "#fn:" + m[2], "REFERENCES", true,
        { source: "semantic", engine: "fake-types" }));
    }
  }
  return diff;
}

let buffered = Buffer.alloc(0);
process.stdin.on("data", (chunk) => {
  buffered = Buffer.concat([buffered, chunk]);
  for (;;) {
    const headerEnd = buffered.indexOf("\r\n\r\n");
    if (headerEnd < 0) return;
    const length = /content-length:\s*(\d+)/i.exec(buffered.slice(0, headerEnd).toString("utf8"));
    if (!length) return;
    const bodyEnd = headerEnd + 4 + Number(length[1]);
    if (buffered.length < bodyEnd) return;
    const request = JSON.parse(buffered.slice(headerEnd + 4, bodyEnd).toString("utf8"));
    buffered = buffered.slice(bodyEnd);
    if (request.id === undefined || request.id === null) continue;
    if (request.method === "fileChanged") {
      if (DEFECT === "hang") continue;
      writeFrame({ jsonrpc: "2.0", id: request.id, result: fileChanged(request.params.filePath) });
    } else if (request.method === "semanticPass") {
      writeFrame({ jsonrpc: "2.0", id: request.id, result: semanticPass(request.params.filePaths) });
    } else {
      writeFrame({ jsonrpc: "2.0", id: request.id, result: { acknowledged: true } });
    }
  }
});
process.stdin.on("end", () => process.exit(0));
"##;

struct FakePlugin {
    _root: tempfile::TempDir,
    dir: PathBuf,
}

/// Writes the fake as a discoverable plugin directory - `<tmp>/fake/` with a
/// `plugin.toml`, since `read_manifest` requires the directory to be named
/// after the language.
fn install_fake(defect: &str, semantic_pass: bool) -> FakePlugin {
    let root = tempfile::tempdir().expect("failed to create a temp dir for the fake plugin");
    let dir = root.path().join("fake");
    fs::create_dir_all(&dir).unwrap();
    let protocol = g_mesh::protocol::types::CURRENT_PROTOCOL_VERSION;
    fs::write(
        dir.join("plugin.js"),
        FAKE_PLUGIN.replace("__DEFECT__", defect).replace("__PROTOCOL__", &protocol.to_string()),
    )
    .unwrap();
    fs::write(
        dir.join("plugin.toml"),
        format!(
            "[plugin]\nlanguage = \"fake\"\nprotocol_version = {protocol}\nplugin_version = \"0.0.0-fake\"\n\n\
             [plugin.spawn]\ncommand = \"node\"\nargs = [\"./plugin.js\"]\n\n\
             [plugin.languages]\nextensions = [\".fk\"]\n\n\
             [plugin.capabilities]\nsemantic_pass = {semantic_pass}\n"
        ),
    )
    .unwrap();
    FakePlugin { _root: root, dir }
}

struct Run {
    success: bool,
    stdout: String,
    /// Check id -> "PASS" / "FAIL" / "SKIP".
    outcomes: BTreeMap<String, String>,
}

fn run_check(plugin_dir: &Path, fixture: &Path, env: &[(&str, &str)]) -> Run {
    let output = Command::new(BIN)
        .args(["plugins", "check"])
        .arg(plugin_dir)
        .arg("--fixture")
        .arg(fixture)
        .envs(env.iter().copied())
        .output()
        .expect("failed to run g-mesh plugins check");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut outcomes = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        for verdict in ["PASS", "FAIL", "SKIP"] {
            if let Some(id) = rest.strip_prefix(verdict).and_then(|r| r.strip_prefix("  ")) {
                let id = id.split_whitespace().next().unwrap_or_default().to_string();
                assert!(
                    outcomes.insert(id.clone(), verdict.to_string()).is_none(),
                    "{id} reported twice:\n{stdout}"
                );
            }
        }
    }
    let run = Run { success: output.status.success(), stdout, outcomes };
    let reported: Vec<&str> = run.outcomes.keys().map(String::as_str).collect();
    let mut expected: Vec<&str> = ALL_CHECKS.to_vec();
    expected.sort_unstable();
    assert_eq!(
        reported,
        expected,
        "the report must carry every check exactly once:\n{}\nstderr:\n{}",
        run.stdout,
        String::from_utf8_lossy(&output.stderr)
    );
    run
}

impl Run {
    fn failing(&self) -> Vec<&str> {
        self.outcomes.iter().filter(|(_, v)| *v == "FAIL").map(|(k, _)| k.as_str()).collect()
    }

    fn outcome(&self, id: &str) -> &str {
        &self.outcomes[id]
    }
}

/// Runs the fake with `defect` and asserts `expected` is the one failing
/// check, and that the run's exit status says so. Returns the run for any
/// further assertion on its findings.
fn assert_only_failure(defect: &str, semantic_pass: bool, expected: &str, env: &[(&str, &str)]) -> Run {
    assert_only_failure_on(&fixtures().join("fake"), defect, semantic_pass, expected, env)
}

fn assert_only_failure_on(
    fixture: &Path,
    defect: &str,
    semantic_pass: bool,
    expected: &str,
    env: &[(&str, &str)],
) -> Run {
    let fake = install_fake(defect, semantic_pass);
    let run = run_check(&fake.dir, fixture, env);
    assert_eq!(
        run.failing(),
        vec![expected],
        "defect {defect:?} must fail exactly {expected}:\n{}",
        run.stdout
    );
    assert!(!run.success, "a failing check must make the command exit non-zero:\n{}", run.stdout);
    run
}

#[test]
fn the_conformant_fake_passes_every_check() {
    let fixture = fixtures().join("fake");
    let before: Vec<(PathBuf, Vec<u8>)> =
        ["a.fk", "b.fk"].iter().map(|f| (fixture.join(f), fs::read(fixture.join(f)).unwrap())).collect();

    let fake = install_fake("none", true);
    let run = run_check(&fake.dir, &fixture, &[]);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        let expected = if id == "capabilities.semantic-pass-undeclared" { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    assert!(!run.stdout.contains("WARN"), "a v2-speaking plugin gets no legacy warning:\n{}", run.stdout);
    // The emptied-file step really deleted something, so `deletes-known`
    // passed on evidence rather than on an empty list.
    assert!(run.stdout.contains("(a.fk emptied) -> fileChanged: +1 / -3 node(s)"), "{}", run.stdout);
    // Likewise the declaration edit really moved something - `alpha` is the
    // first declaration, on line 1, so the break goes before it and every
    // node of a.fk moves - so `declaration-edit-applies` compared real ranges.
    assert!(
        run.stdout
            .contains("declaration edit: a line break before line 1 of a.fk, the last line of \"alpha\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("the last line of \"alpha\") -> fileChanged: +4 / -0 node(s)"),
        "{}",
        run.stdout
    );

    for (path, contents) in before {
        assert_eq!(
            fs::read(&path).unwrap(),
            contents,
            "the kit must never modify the fixture ({})",
            path.display()
        );
    }
}

/// The same conformant fake with `semantic_pass = false`: core must never send
/// it the request (the wire half of the capability check), and the lazy-engine
/// check does not apply.
#[test]
fn the_conformant_fake_without_semantic_pass_passes_the_undeclared_capability_check() {
    let fake = install_fake("none", false);
    let run = run_check(&fake.dir, &fixtures().join("fake"), &[]);
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome("capabilities.semantic-pass-undeclared"), "PASS", "{}", run.stdout);
    assert_eq!(run.outcome("capabilities.semantic-engine-lazy"), "SKIP", "{}", run.stdout);
    assert!(!run.stdout.contains("-> semanticPass"), "no semanticPass may be sent:\n{}", run.stdout);
}

#[test]
fn a_placeholder_without_a_target_fails_shape() {
    let run = assert_only_failure("shape", true, "shape", &[]);
    assert!(
        run.stdout.contains("\"a.fk#use:b.fk#gamma\" (nativeKind \"pending_symbol\") has no `target`"),
        "{}",
        run.stdout
    );
}

#[test]
fn an_edge_emitted_before_its_nodes_fails_stream_order() {
    let run = assert_only_failure("stream-order-late", true, "stream-order", &[]);
    assert!(
        run.stdout.contains("bulk run 1, NDJSON line 2: edge \"a.fk#file -DEFINES-> a.fk#fn:alpha\""),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("is only emitted later"), "{}", run.stdout);
}

#[test]
fn an_edge_onto_another_files_node_fails_stream_order() {
    let run = assert_only_failure("stream-order-cross-file", true, "stream-order", &[]);
    assert!(
        run.stdout.contains("toId is a node of \"b.fk\", not of the edge's own file \"a.fk\""),
        "{}",
        run.stdout
    );
    assert!(
        run.stdout.contains("fileChanged #1 (a.fk unmodified) fileChanged response"),
        "diffs are judged too:\n{}",
        run.stdout
    );
}

#[test]
fn an_unresolved_same_file_edge_fails_the_same_file_rule() {
    let run = assert_only_failure("same-file-rule", true, "same-file-rule", &[]);
    assert!(
        run.stdout.contains("edge \"a.fk#fn:beta -CALLS-> a.fk#fn:alpha\" (CALLS \"a.fk#fn:beta\" -> \"a.fk#fn:alpha\") is `resolved: false`"),
        "{}",
        run.stdout
    );
}

#[test]
fn ids_that_change_between_bulk_runs_fail_bulk_repeat() {
    let run = assert_only_failure("bulk-repeat", true, "id-stability.bulk-repeat", &[]);
    assert!(run.stdout.contains("node id \"a.fk#var:run-"), "{}", run.stdout);
    assert!(run.stdout.contains("is emitted by bulk run 1 but not by bulk run 2"), "{}", run.stdout);
}

#[test]
fn a_range_moved_by_whitespace_fails_the_whitespace_edit_check() {
    let run = assert_only_failure("whitespace-moves-range", true, "id-stability.whitespace-edit", &[]);
    assert!(run.stdout.contains("upserts node \"a.fk#file\""), "{}", run.stdout);
}

#[test]
fn deleting_an_id_never_emitted_fails_deletes_known() {
    let run = assert_only_failure("deletes-unknown", true, "id-stability.deletes-known", &[]);
    assert!(run.stdout.contains("deleteNodeIds names \"never-emitted-node\""), "{}", run.stdout);
}

#[test]
fn incremental_ids_that_differ_from_bulk_ids_fail_incremental_matches_bulk() {
    let run = assert_only_failure("incremental-ids", true, "id-stability.incremental-matches-bulk", &[]);
    assert!(
        run.stdout
            .contains("node \"a.fk#func:alpha\" of \"a.fk\" is in the index after fileChanged restored"),
        "{}",
        run.stdout
    );
}

/// GM-294. A plugin whose diff never re-sends a declaration that moved keeps
/// every id right - `whitespace-edit`, `deletes-known` and
/// `incremental-matches-bulk` all pass - while the index goes on answering
/// with the old ranges: the user-visible half of GM-292, produced by the
/// plugin instead of by core. Only the declaration edit, judged against a
/// bulk walk of the edited file, sees it.
#[test]
fn a_diff_that_never_resends_a_moved_declaration_fails_declaration_edit_applies() {
    let run = assert_only_failure("stale-ranges", true, "id-stability.declaration-edit-applies", &[]);
    assert!(
        run.stdout.contains("node \"a.fk#fn:alpha\" is at 0:0-0:8 in the index, but a fresh bulk walk of the edited file puts it at 1:0-1:8"),
        "{}",
        run.stdout
    );
}

#[test]
fn a_defines_edge_from_a_symbol_fails_ownership() {
    let run = assert_only_failure("defines-from-symbol", true, "ownership.defines-exports-from-file", &[]);
    assert!(run.stdout.contains("starts at a Function node"), "{}", run.stdout);
}

#[test]
fn a_node_in_another_language_fails_ownership_language() {
    let run = assert_only_failure("language", true, "ownership.language", &[]);
    assert!(
        run.stdout.contains("declares language \"fake-dialect\", but the manifest's language is \"fake\""),
        "{}",
        run.stdout
    );
}

#[test]
fn a_plugin_emitted_container_fails_ownership_no_container() {
    let run = assert_only_failure("container", true, "ownership.no-container", &[]);
    assert!(
        run.stdout.contains("node \"fake-container-pkg\" has nativeKind \"container\""),
        "{}",
        run.stdout
    );
}

#[test]
fn a_diff_touching_another_file_fails_diff_stays_in_file() {
    let run = assert_only_failure("diff-other-file", true, "ownership.diff-stays-in-file", &[]);
    assert!(
        run.stdout.contains("upserts node \"b.fk#file\" of \"b.fk\" in a diff for \"a.fk\""),
        "{}",
        run.stdout
    );
}

#[test]
fn an_engine_started_before_the_first_semantic_pass_fails_the_lazy_check() {
    let run = assert_only_failure("eager-engine", true, "capabilities.semantic-engine-lazy", &[]);
    assert!(
        run.stdout.contains("already existed when the first semanticPass request was written"),
        "{}",
        run.stdout
    );
}

#[test]
fn an_engine_started_without_the_capability_fails_the_undeclared_check() {
    let run = assert_only_failure("undeclared-engine", false, "capabilities.semantic-pass-undeclared", &[]);
    assert!(run.stdout.contains("although its manifest declares semantic_pass = false"), "{}", run.stdout);
}

/// A plugin that never answers `fileChanged` must cost one `session` failure
/// after `G_MESH_FILE_CHANGED_TIMEOUT_MS`, not a hung CLI - and every check
/// that needed the session's answers must be skipped, not passed.
#[test]
fn a_plugin_that_never_answers_fails_session_instead_of_hanging() {
    let started = std::time::Instant::now();
    let run = assert_only_failure("hang", false, "session", &[("G_MESH_FILE_CHANGED_TIMEOUT_MS", "1500")]);
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "took {:?}", started.elapsed());
    assert!(run.stdout.contains("no response within 1.5s"), "{}", run.stdout);
    for id in [
        "id-stability.whitespace-edit",
        "id-stability.deletes-known",
        "id-stability.incremental-matches-bulk",
    ] {
        assert_eq!(run.outcome(id), "SKIP", "{id}:\n{}", run.stdout);
    }
}

/// The bulk walk has no timeout in the daemon, but must have one here. Its
/// budget is `semantic_pass_project_timeout(file_count)`: the overridden floor
/// or 10s per claimed file, whichever is larger - so this runs on a one-file
/// copy of the fixture to keep the wait at 10s.
#[test]
fn a_bulk_index_that_never_finishes_fails_session_instead_of_hanging() {
    let fixture = tempfile::tempdir().unwrap();
    fs::copy(fixtures().join("fake/b.fk"), fixture.path().join("b.fk")).unwrap();
    let started = std::time::Instant::now();
    let run = assert_only_failure_on(
        fixture.path(),
        "bulk-hang",
        false,
        "session",
        &[("G_MESH_SEMANTIC_PASS_PROJECT_TIMEOUT_MS", "1000")],
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "took {:?}", started.elapsed());
    assert!(
        run.stdout.contains("bulk run 1: the bulk index did not finish within 10s and was killed"),
        "{}",
        run.stdout
    );
}

#[test]
fn the_typescript_plugin_passes_on_a_small_typescript_fixture() {
    let run = run_check(&ts_plugin_dir(), &ts_conformance_project(), &[]);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        let expected = if id == "capabilities.semantic-pass-undeclared" { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    // GM-275: the TS plugin speaks wire v2 now, so there is nothing left for
    // `shape` to warn about (that WARN existed only while the plugin still
    // sent the v1 shape core's normalizing deserializer accepted).
    assert!(!run.stdout.contains("WARN"), "{}", run.stdout);
}

/// The bundled Go plugin against its own fixture - a multi-file package, a
/// second package reached through an import, an external `_test` package and
/// a `go.work` naming two modules.
///
/// The two capability checks are both `SKIP`, for different reasons and both
/// correctly: the manifest declares `semantic_pass = true`, so
/// `semantic-pass-undeclared` does not apply, and the plugin has no semantic
/// engine to start yet (go/types is GM-281), so it writes no
/// semantic-engine marker and `semantic-engine-lazy` has no evidence either
/// way. When GM-281 lands that second one becomes a `PASS` and this test
/// should say so.
#[test]
fn the_go_plugin_passes_on_its_own_fixture() {
    let run = run_check(&go_plugin_dir(), &go_conformance_project(), &[]);
    assert!(run.success, "{}", run.stdout);
    for id in ALL_CHECKS {
        let expected = if id.starts_with("capabilities.") { "SKIP" } else { "PASS" };
        assert_eq!(run.outcome(id), expected, "{id}:\n{}", run.stdout);
    }
    assert!(!run.stdout.contains("WARN"), "{}", run.stdout);
}

// ============================================================================
// GM-277: `--expect <expect.toml>` - post-linking assertions against the
// same query code the MCP tools use. See `core/src/cli/plugin_check/
// expectations.rs`'s own module doc for the decisions these tests check.
// ============================================================================

/// Like [`run_check`], but for a run given `--expect`: the set of reported
/// check ids now includes a dynamic `"expectations"` section
/// (`expectations.callers[0]`, ...) that varies with the expectations file,
/// so this does not assert the fixed `ALL_CHECKS` set the way `run_check`
/// does - only that the command ran and captured output to inspect.
fn run_check_with_expect(plugin_dir: &Path, fixture: &Path, expect: &Path) -> Run {
    let output = Command::new(BIN)
        .args(["plugins", "check"])
        .arg(plugin_dir)
        .arg("--fixture")
        .arg(fixture)
        .arg("--expect")
        .arg(expect)
        .output()
        .expect("failed to run g-mesh plugins check --expect");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut outcomes = BTreeMap::new();
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix("  ") else { continue };
        for verdict in ["PASS", "FAIL", "SKIP"] {
            if let Some(id) = rest.strip_prefix(verdict).and_then(|r| r.strip_prefix("  ")) {
                let id = id.split_whitespace().next().unwrap_or_default().to_string();
                assert!(
                    outcomes.insert(id.clone(), verdict.to_string()).is_none(),
                    "{id} reported twice:\n{stdout}"
                );
            }
        }
    }
    Run { success: output.status.success(), stdout, outcomes }
}

/// Writes a small `.fk` fixture project (the toy language `install_fake`'s
/// plugin speaks - see this file's module doc) into a fresh temp directory,
/// one file per `(name, contents)` pair, and returns its path.
fn write_fk_fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create a temp fixture dir");
    for (name, contents) in files {
        fs::write(dir.path().join(name), contents).unwrap();
    }
    dir
}

fn write_expect_file(dir: &Path, contents: &str) -> PathBuf {
    let path = dir.join("expect.toml");
    fs::write(&path, contents).unwrap();
    path
}

/// The TS fixture's own `expect.toml` (`plugins/typescript/conformance/
/// expect.toml`) passes end to end through the real plugin, tsserver's
/// semantic pass included - the acceptance criterion "the TS fixture
/// passes".
#[test]
fn the_typescript_plugin_satisfies_its_own_expectations_file() {
    let run = run_check_with_expect(&ts_plugin_dir(), &ts_conformance_project(), &ts_conformance_expect());
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);
    let expectation_results: Vec<&String> = run
        .outcomes
        .keys()
        .filter(|id| id.starts_with("expectations.") && *id != "expectations.file")
        .collect();
    // Every kind in expect.toml is exercised, and none of them silently
    // vanished from the report.
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.callers")), "{}", run.stdout);
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.references")), "{}", run.stdout);
    assert!(
        expectation_results.iter().any(|id| id.starts_with("expectations.implementations")),
        "{}",
        run.stdout
    );
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.imports")), "{}", run.stdout);
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.definition")), "{}", run.stdout);
    for id in expectation_results {
        assert_eq!(run.outcome(id), "PASS", "{id}:\n{}", run.stdout);
    }
}

/// The Go plugin's own expectations, through the real linker and the real
/// MCP handlers - GM-280's end-to-end proof that a container-scoped
/// placeholder is an address core actually resolves.
///
/// Deliberately a smaller set than the TS plugin's (GM-282 owns the full Go
/// expectations file), so this asserts the two kinds that file does carry
/// rather than every kind the kit supports. `[[implementations]]` in
/// particular is absent on purpose: Go's interfaces are structural, so
/// `SUPERTYPE_OF` edges are go/types' answer and arrive with GM-281.
#[test]
fn the_go_plugin_satisfies_its_own_expectations_file() {
    let run = run_check_with_expect(&go_plugin_dir(), &go_conformance_project(), &go_conformance_expect());
    assert!(run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.file"), "PASS", "{}", run.stdout);
    let expectation_results: Vec<&String> = run
        .outcomes
        .keys()
        .filter(|id| id.starts_with("expectations.") && *id != "expectations.file")
        .collect();
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.callers")), "{}", run.stdout);
    assert!(expectation_results.iter().any(|id| id.starts_with("expectations.imports")), "{}", run.stdout);
    for id in expectation_results {
        assert_eq!(run.outcome(id), "PASS", "{id}:\n{}", run.stdout);
    }
}

/// A deliberately wrong `[[callers]]` entry - one expected caller that does
/// not exist ("ghost"), and the real caller ("user") left off `expect` -
/// fails with a diff naming both the missing and the extra entry, and
/// nothing else in the report is affected.
#[test]
fn a_wrong_expectation_reports_a_readable_diff() {
    let fixture = write_fk_fixture(&[("a.fk", "fn helper\nfn user\ncall helper\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[callers]]\nsymbol = \"helper\"\nfile = \"a.fk\"\nexpect = [\"a.fk:ghost\"]\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);
    assert!(!run.success, "a failing expectation must make the command exit non-zero:\n{}", run.stdout);
    assert_eq!(run.outcome("expectations.callers[0]"), "FAIL", "{}", run.stdout);
    assert!(run.stdout.contains("expected: {a.fk:ghost}"), "{}", run.stdout);
    assert!(run.stdout.contains("actual:   {a.fk:user}"), "{}", run.stdout);
    assert!(run.stdout.contains("missing (expected, not found): a.fk:ghost"), "{}", run.stdout);
    assert!(run.stdout.contains("extra (found, not expected): a.fk:user"), "{}", run.stdout);
}

/// Two same-named declarations across files make `symbol_name` resolution
/// ambiguous - the expectation fails with every candidate's id/
/// qualifiedName/filePath/kind printed, never a silent pick (decision 3).
///
/// The `[[definition]]` and `[[callers]]` kinds are both exercised here
/// because they disambiguate differently once `file` narrows the candidate
/// list to one (`expectations.rs`'s module doc, decision 3): the four
/// `symbol_id`-accepting tools (`callers` among them) re-call by that exact
/// id and always land on the one candidate meant. `find_definition` has no
/// `symbol_id` parameter, so its own disambiguated re-call is by
/// `qualifiedName` - which stays ambiguous here on purpose (both `shared`
/// declarations share the bare name `qualifiedName` too, the fake
/// language's convention), so `[[definition]]`'s `file` case is asserted to
/// still fail, with its own distinct message, rather than silently claimed
/// to work when it can't.
#[test]
fn an_ambiguous_symbol_fails_with_its_candidates() {
    let fixture = write_fk_fixture(&[("a.fk", "fn shared\n"), ("b.fk", "fn shared\nfn user\ncall shared\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[definition]]\nsymbol = \"shared\"\nexpect = [\"a.fk:shared\"]\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.definition[0]"), "FAIL", "{}", run.stdout);
    assert!(
        run.stdout.contains("did not resolve to one symbol (resolvedBy = nameAmbiguous)"),
        "{}",
        run.stdout
    );
    assert!(run.stdout.contains("no `file` was given to disambiguate"), "{}", run.stdout);
    assert!(run.stdout.contains("filePath=a.fk"), "{}", run.stdout);
    assert!(run.stdout.contains("filePath=b.fk"), "{}", run.stdout);

    // `find_definition` has no `symbol_id`: narrowing by `file` still leaves
    // its re-call ambiguous by `qualifiedName` alone, and that failure is
    // reported rather than silently guessed at.
    let expect_still_ambiguous = write_expect_file(
        fixture.path(),
        "[[definition]]\nsymbol = \"shared\"\nfile = \"a.fk\"\nexpect = [\"a.fk:shared\"]\n",
    );
    let run_still_ambiguous = run_check_with_expect(&fake.dir, fixture.path(), &expect_still_ambiguous);
    assert_eq!(
        run_still_ambiguous.outcome("expectations.definition[0]"),
        "FAIL",
        "{}",
        run_still_ambiguous.stdout
    );
    assert!(
        run_still_ambiguous.stdout.contains("has no symbol_id parameter to disambiguate further"),
        "{}",
        run_still_ambiguous.stdout
    );

    // `[[callers]]` disambiguates cleanly: it re-calls by the winning
    // candidate's own `symbol_id`, so `file` alone is enough.
    let expect_callers = write_expect_file(
        fixture.path(),
        "[[callers]]\nsymbol = \"shared\"\nfile = \"b.fk\"\nexpect = [\"b.fk:user\"]\n",
    );
    let run_callers = run_check_with_expect(&fake.dir, fixture.path(), &expect_callers);
    assert_eq!(run_callers.outcome("expectations.callers[0]"), "PASS", "{}", run_callers.stdout);
}

/// A typo'd key in `expect.toml` is a hard parse error - `expectations.file`
/// fails with the unknown field named, and no per-expectation check runs at
/// all (decision 5: typos must not pass silently).
#[test]
fn an_unknown_expectation_key_is_a_hard_parse_error() {
    let fixture = write_fk_fixture(&[("a.fk", "fn helper\n")]);
    let expect = write_expect_file(
        fixture.path(),
        "[[callers]]\nsymbol = \"helper\"\nexpect = []\nbogus_field = true\n",
    );
    let fake = install_fake("none", true);
    let run = run_check_with_expect(&fake.dir, fixture.path(), &expect);
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.file"), "FAIL", "{}", run.stdout);
    assert!(run.stdout.contains("bogus_field"), "{}", run.stdout);
    assert!(run.stdout.contains("unknown field"), "{}", run.stdout);
    assert!(
        !run.outcomes.keys().any(|id| id.starts_with("expectations.callers")),
        "an unparsed file must run no per-expectation check:\n{}",
        run.stdout
    );
}

/// The discrimination case: `[[callers]] symbol = "double"` in the TS
/// fixture's own `expect.toml` expects a caller reached only through a
/// namespace import (`import * as m from "./math"; m.double(4)` in
/// main.ts's `useNamespaceImport`), which the structural pass cannot see at
/// all - only the semantic pass (tsserver) upgrades it to a real edge. This
/// test proves that dependency is real, not assumed: it runs the identical
/// fixture and `expect.toml` against a copy of the TS plugin whose manifest
/// declares `semantic_pass = false`, so `session::run_session` never sends
/// `semanticPass` at all, and shows that this one expectation - and only
/// this one - now fails, missing exactly `useNamespaceImport`. Every other
/// `[[callers]]` entry (the same-file call, the cross-file import, both
/// barrel re-export forms - all structural) keeps passing, which is what
/// proves the failure is specific to the semantic-only case and not a
/// blanket breakage from disabling the capability.
#[test]
fn a_namespace_import_caller_needs_the_semantic_pass_to_resolve() {
    let plugin = ts_plugin_dir();
    let scratch = tempfile::tempdir().expect("failed to create a temp dir for the no-semantic-pass plugin");
    let dir = scratch.path().join("typescript");
    fs::create_dir_all(&dir).unwrap();
    // `dist`/`node_modules` are symlinked rather than copied: this plugin is
    // already built by `core/build.rs`, and copying `node_modules` (tens of
    // MB) on every test run would be pure waste - only `plugin.toml` needs
    // to differ.
    for shared in ["dist", "node_modules"] {
        #[cfg(unix)]
        std::os::unix::fs::symlink(plugin.join(shared), dir.join(shared)).unwrap();
        #[cfg(windows)]
        {
            let target = plugin.join(shared);
            if target.is_dir() {
                std::os::windows::fs::symlink_dir(&target, dir.join(shared)).unwrap();
            } else {
                std::os::windows::fs::symlink_file(&target, dir.join(shared)).unwrap();
            }
        }
    }
    let manifest = fs::read_to_string(plugin.join("plugin.toml")).unwrap();
    assert!(manifest.contains("semantic_pass = true"), "the real manifest must still declare it: {manifest}");
    fs::write(dir.join("plugin.toml"), manifest.replace("semantic_pass = true", "semantic_pass = false"))
        .unwrap();

    let run = run_check_with_expect(&dir, &ts_conformance_project(), &ts_conformance_expect());
    assert!(!run.success, "{}", run.stdout);
    assert_eq!(run.outcome("expectations.callers[1]"), "FAIL", "{}", run.stdout);
    assert!(
        run.stdout.contains("missing (expected, not found): src/main.ts:useNamespaceImport"),
        "{}",
        run.stdout
    );

    // Every other expectation - the structural ones - is unaffected.
    for id in [
        "expectations.callers[0]",
        "expectations.callers[2]",
        "expectations.references[0]",
        "expectations.implementations[0]",
        "expectations.imports[0]",
        "expectations.definition[0]",
        "expectations.definition[1]",
    ] {
        assert_eq!(run.outcome(id), "PASS", "{id}:\n{}", run.stdout);
    }
}
