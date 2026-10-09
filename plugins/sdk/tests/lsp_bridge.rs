//! `lsp::LspBridge` against a real, scripted language server.
//!
//! # What these tests are for
//!
//! The bridge's failure modes are all about a peer it does not control: a
//! server that is still indexing, one that answers in a different column unit
//! than it said it would, one that never answers, one that dies with questions
//! outstanding. None of those is reachable through a mocked client - they are
//! facts about two processes and a pipe - so every test here spawns
//! `g-mesh-fake-lsp` (`plugins/sdk/fake-lsp/main.rs`) with a JSON script and
//! lets the real client talk to it over real pipes.
//!
//! # The fixture, and why it looks like that
//!
//! Two files of a language that does not exist, with deliberately awkward
//! text:
//!
//! ```text
//! src/a.toy   🦀🦀🦀🦀 add
//! src/b.toy   fn caller
//!               héllo🦀.add()
//! ```
//!
//! Both lines are chosen so that a character column and a UTF-16 column
//! *differ*, and so that the difference is load-bearing in both directions:
//!
//! - **Outbound.** The open site's name starts at character 9 of `b.toy`'s
//!   second line and at UTF-16 unit 10. The scripted server answers only at
//!   10, so a bridge that sent the wire's own column would be told "nothing
//!   here" and emit no edge.
//! - **Inbound.** The declaration `add` occupies characters 5 to 8 of `a.toy`,
//!   and the server answers with UTF-16 unit 9. Converted, that is character
//!   5, inside the node; unconverted, it is character 9, past the node's end,
//!   where the only thing containing it is the file itself - which is never an
//!   answer's target. So a bridge that skipped the conversion would again emit
//!   nothing.
//!
//! That is what makes [`a_definition_becomes_a_semantic_edge`] a test of the
//! mapping rather than of the plumbing: it fails, silently and completely, if
//! either conversion is dropped.

// Test diagnostics to the harness, not a daemon log line (GM-520).
#![allow(clippy::disallowed_macros)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use g_mesh_plugin_sdk::lsp::{Budgets, LspBridge, SemanticConfig, ServerReadiness};
use g_mesh_plugin_sdk::wire::{
    EdgeKind, NodeKind, Position, Range, SourceTier, TargetKey, TargetScope, WireEdge, WireNode,
};
use g_mesh_plugin_sdk::{
    FileGraphBuilder, NodeSpec, OpenSite, OpenSiteKind, RelPath, SdkIndex, SemanticAnswer, SemanticEngine,
};
use serde_json::{json, Value};

/// The declaring file: four crabs, a space, and a three-character name. See
/// this module's doc for why every one of those counts.
const A_TOY: &str = "🦀🦀🦀🦀 add\n";
/// The using file, whose open site sits after a non-ASCII prefix.
const B_TOY: &str = "fn caller\n  héllo🦀.add()\n";

/// The site's own column, in characters (the wire's unit) and in UTF-16 code
/// units (what the server is told, and what it matches on).
const SITE_CHAR_COL: u32 = 9;
const SITE_UTF16_COL: u32 = 10;
/// The declaration's column, in the same two units.
const DECL_CHAR_COL: u32 = 5;
const DECL_UTF16_COL: u32 = 9;

// --- the fixture ------------------------------------------------------------

/// A scratch project directory that removes itself.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("g-mesh-lsp-bridge-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).expect("the scratch project can be created");
        Scratch(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// The `file:` URI of a project-relative path, as the scripted server
    /// will be asked about it.
    fn uri(&self, relative: &str) -> String {
        file_uri(&self.0.join(relative))
    }

    /// The same, through the *canonical* root - `/private/var/…` on macOS
    /// where the scratch directory itself says `/var/…`, and the long-name
    /// `C:\Users\runneradmin\…` on Windows where `%TEMP%` is the 8.3
    /// `C:\Users\RUNNER~1\…`. A server reports the canonical spelling, so at
    /// least one test answers in it.
    fn real_uri(&self, relative: &str) -> String {
        let real = std::fs::canonicalize(&self.0).unwrap_or_else(|_| self.0.clone());
        file_uri(&real.join(relative))
    }

    fn write(&self, relative: &str, contents: &str) {
        std::fs::write(self.0.join(relative), contents).expect("the fixture file can be written");
    }

    /// Writes the script and returns the config that runs the fake server with
    /// it.
    fn server(&self, script: Value) -> SemanticConfig {
        let path = self.0.join("script.json");
        std::fs::write(&path, serde_json::to_vec_pretty(&script).unwrap()).unwrap();
        let mut config = SemanticConfig::new(env!("CARGO_BIN_EXE_g-mesh-fake-lsp"));
        config.args = vec!["--script".to_string(), path.to_string_lossy().into_owned()];
        config.engine = "fake-lsp".to_string();
        config
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `file:` URI naming an absolute path.
///
/// Spelled here rather than imported from the SDK, for the reason
/// `fake-lsp`'s module doc gives about framing: the scripted server matches
/// the request's URI as a *string*, so a fixture that built its expectation
/// with the bridge's own function would agree with the bridge by
/// construction and could never catch it spelling a URI no server accepts.
/// That is not hypothetical - GM-338 is exactly that failure, in this
/// direction: this helper used to be `format!("file://{path}")`, which is
/// right for a POSIX path by luck (the leading `/` supplies the third slash)
/// and wrong for every Windows one, which has no leading slash and uses the
/// other separator. Nineteen of the twenty-five bridge tests in this file
/// then asked about one URI, scripted an answer under another, and got the
/// server's ordinary "nothing here" for every question.
///
/// Three decisions, and two of them are invisible on a Unix host: forward
/// slashes, three of them before a drive letter, and percent-encoding for
/// everything outside the unreserved set (`:` and `/` excepted - a drive
/// spelled `C%3A` names no drive). The test below checks all three from any
/// host.
fn file_uri(path: &Path) -> String {
    let text = path.to_string_lossy();
    // `std::fs::canonicalize` answers in Windows' extended-length spelling,
    // which is not a URI path: `\\?\C:\p` is `C:\p`.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).replace('\\', "/");
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    // A POSIX path brings the third slash itself; a Windows one (`C:/p`) does
    // not.
    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    }
}

/// **The GM-338 control.** The one platform-dependent decision this fixture
/// makes, stated as literals so that its Windows arm runs on every host -
/// there is no Windows machine to reproduce the CI failure on, and a check
/// that only runs where the bug cannot happen is not a check.
///
/// The first case is the exact shape of a GitHub Actions runner's `%TEMP%`,
/// short name and all; the second is what `Scratch::real_uri` gets back from
/// `canonicalize` there. Before the fix both produced `file://C:\…` - two
/// slashes and the wrong separator - which is a URI the bridge never asks
/// about and the scripted server therefore never matches.
#[test]
fn a_windows_scratch_path_is_asked_about_by_an_ordinary_file_uri() {
    assert_eq!(
        file_uri(Path::new(r"C:\Users\RUNNER~1\AppData\Local\Temp\g-mesh-lsp-bridge-1-x\src\b.toy")),
        "file:///C:/Users/RUNNER~1/AppData/Local/Temp/g-mesh-lsp-bridge-1-x/src/b.toy"
    );
    assert_eq!(file_uri(Path::new(r"\\?\C:\p\src\b.toy")), "file:///C:/p/src/b.toy");
    // The POSIX arm, which is the only one CI was checking until now.
    assert_eq!(file_uri(Path::new("/private/var/p/src/b.toy")), "file:///private/var/p/src/b.toy");
    assert_eq!(file_uri(Path::new("/p/a b.toy")), "file:///p/a%20b.toy", "a URI carries no raw space");
}

/// Budgets small enough that a test finishes and large enough that a machine
/// under load does not fail one for being slow - every value here is an
/// order of magnitude above what the fake server takes to answer.
fn budgets() -> Budgets {
    Budgets {
        request: Duration::from_secs(5),
        max_sites: 1_000,
        concurrency: 4,
        project_floor: Duration::from_secs(30),
        per_file: Duration::from_secs(30),
        single_file: Duration::from_secs(30),
        readiness: Duration::from_secs(20),
        settle: Duration::from_millis(150),
        warm_up: None,
    }
}

/// The two-file index this module's doc describes: `a.toy` declares `add`,
/// `b.toy` calls it through a receiver it cannot resolve.
fn fixture(scratch: &Scratch) -> (SdkIndex, String) {
    let (index, caller, _) = fixture_with_structural_edge(scratch, false);
    (index, caller)
}

/// [`fixture`], where `b.toy` may also carry a structural `CALLS` edge for
/// the site, onto a placeholder addressed differently from the one an answer
/// produces, and the site names that edge in `replaces`. Returns the edge's
/// id when there is one.
fn fixture_with_structural_edge(scratch: &Scratch, structural: bool) -> (SdkIndex, String, Option<String>) {
    scratch.write("src/a.toy", A_TOY);
    scratch.write("src/b.toy", B_TOY);

    let mut index = SdkIndex::new();

    let a = RelPath::new("src/a.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &a);
    builder.file_node(range(0, 0, 1, 0));
    // The declaration's range covers its name and nothing else - see this
    // module's doc on the inbound conversion.
    builder.add_node(
        NodeSpec::new(NodeKind::Function, "add", "add", range(0, DECL_CHAR_COL, 0, DECL_CHAR_COL + 3))
            .native_kind("function")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(a, A_TOY.to_string(), builder.finish());

    let b = RelPath::new("src/b.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &b);
    builder.file_node(range(0, 0, 2, 0));
    let caller = builder.add_node(
        NodeSpec::new(NodeKind::Function, "caller", "caller", range(0, 0, 1, 15))
            .native_kind("function")
            .in_container("pkg", None)
            .public(),
    );
    let replaces = structural.then(|| {
        let placeholder = builder.add_placeholder(
            g_mesh_plugin_sdk::PlaceholderKind::PendingSymbol,
            "add",
            g_mesh_plugin_sdk::wire::PlaceholderTarget {
                scope: TargetScope::Container("pkg".to_string()),
                key: TargetKey::QualifiedName("Receiver::add".to_string()),
                from_container: Some("pkg".to_string()),
                key_path: None,
            },
            range(1, SITE_CHAR_COL, 1, SITE_CHAR_COL + 3),
        );
        builder.placeholder_edge(EdgeKind::Calls, &caller, &placeholder)
    });
    builder.open_site(OpenSite {
        from_id: caller.clone(),
        position: Position { line: 1, col: SITE_CHAR_COL },
        name: "add".to_string(),
        kind: OpenSiteKind::ReceiverCall,
        edge_kind: EdgeKind::Calls,
        from_container: Some("pkg".to_string()),
        replaces: replaces.clone(),
    });
    index.insert(b, B_TOY.to_string(), builder.finish());

    (index, caller, replaces)
}

fn range(start_line: u32, start_col: u32, end_line: u32, end_col: u32) -> Range {
    Range {
        start: Position { line: start_line, col: start_col },
        end: Position { line: end_line, col: end_col },
    }
}

/// The script entry that answers `b.toy`'s open site with `a.toy`'s
/// declaration, in the server's own column units.
fn answers_the_site(scratch: &Scratch) -> Value {
    json!([{
        "uri": scratch.uri("src/b.toy"),
        "line": 1,
        "character": SITE_UTF16_COL,
        "definition": {
            "uri": scratch.uri("src/a.toy"),
            "line": 0,
            "character": DECL_UTF16_COL,
        },
    }])
}

fn pass(bridge: &mut LspBridge, index: &SdkIndex) -> SemanticAnswer {
    bridge.answer(&[], index).expect("the bridge answers rather than failing")
}

/// The reason an incomplete pass gave, which core records per language.
fn reason(answer: &SemanticAnswer) -> &str {
    answer.reason.as_deref().expect("an incomplete pass says why")
}

fn semantic_edges(answer: &SemanticAnswer) -> Vec<&WireEdge> {
    answer.diff.upsert_edges.iter().filter(|edge| edge.source == SourceTier::Semantic).collect()
}

/// How many times the scripted server was asked `method`, from the log it
/// was told to keep. A question counted rather than assumed is the only way
/// to assert that a follow-up was *not* sent.
fn asked(log: &Path, method: &str) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter(|line| line.trim() == method).count()
}

fn placeholder<'a>(answer: &'a SemanticAnswer, edge: &WireEdge) -> &'a WireNode {
    answer
        .diff
        .upsert_nodes
        .iter()
        .find(|node| node.id == edge.to_id)
        .expect("every edge an answer emits lands on a placeholder it also emits")
}

// --- the tests --------------------------------------------------------------

/// The whole mapping, end to end: a position out, a location back, a
/// `qualifiedName`-keyed placeholder and a semantic edge in the asking file.
///
/// Both column conversions are load-bearing here - see this module's doc.
#[test]
fn a_definition_becomes_a_semantic_edge() {
    let scratch = Scratch::new("definition");
    let (index, caller) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 0 },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let engine = config.engine.clone();
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "every question was asked and answered");

    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 1, "one site, one answer: {:#?}", answer.diff);
    let edge = edges[0];
    assert_eq!(edge.from_id, caller, "the edge starts where the site said it does");
    assert_eq!(edge.kind, EdgeKind::Calls, "the site's own edge kind");
    assert_eq!(edge.engine, engine, "the engine label comes from the manifest");
    assert!(!edge.resolved, "an edge onto a placeholder is core's to confirm");

    let node = placeholder(&answer, edge);
    assert_eq!(node.file_path, "src/b.toy", "the placeholder lives in the asking file");
    assert_eq!(node.native_kind.as_deref(), Some("pending_symbol"));
    let target = node.target.as_ref().expect("a pending symbol always carries its address");
    assert_eq!(target.scope, TargetScope::Container("pkg".to_string()));
    assert_eq!(target.key, TargetKey::QualifiedName("add".to_string()), "exact, never a bare name");
    assert_eq!(target.from_container.as_deref(), Some("pkg"), "who is asking, for the visibility check");
}

/// A site whose structural edge reaches the answered declaration through a
/// differently addressed placeholder: the answer's own edge has another id,
/// so the structural edge is retracted, and the call is left with exactly
/// one `CALLS` edge rather than one per tier.
#[test]
fn an_answer_for_a_site_with_a_structural_edge_leaves_one_edge_for_the_call() {
    let scratch = Scratch::new("replaces");
    let (index, caller, replaces) = fixture_with_structural_edge(&scratch, true);
    let structural = replaces.expect("the fixture wrote a structural edge");
    let config = scratch.server(json!({
        "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 0 },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(answer.complete);
    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 1, "{:#?}", answer.diff);
    assert_eq!(edges[0].from_id, caller);
    assert_ne!(edges[0].id, structural);
    assert_eq!(answer.diff.delete_edge_ids, vec![structural], "the structural edge is retracted");
}

/// The other half of decision 3: a server that negotiates a *different* unit
/// is answered in that unit.
///
/// The same fixture, the same site, and a server that says `utf-32` - so the
/// columns it matches on and answers with are the wire's own, and both
/// conversions become the identity. A bridge that ignored the negotiated
/// encoding would send UTF-16 columns to a UTF-32 server and get nothing,
/// which is the same silent nothing as getting the conversion backwards.
#[test]
fn the_encoding_the_server_negotiates_is_the_one_it_is_answered_in() {
    let scratch = Scratch::new("utf32");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-32",
        "answers": [{
            "uri": scratch.uri("src/b.toy"),
            "line": 1,
            // Characters, not UTF-16 code units - this is the one difference
            // from every other test in this file.
            "character": SITE_CHAR_COL,
            "definition": {
                "uri": scratch.uri("src/a.toy"),
                "line": 0,
                "character": DECL_CHAR_COL,
            },
        }],
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);
    assert!(answer.complete);
}

/// The rule the design doc states outright: "an empty answer before readiness
/// is never recorded as 'no target'".
///
/// The scripted server answers `null` to everything until its indexing
/// progress ends, and ends it 400ms in. A bridge that waits gets the real
/// location; one that asks immediately gets nothing and - worse - records
/// nothing as the truth about that site.
#[test]
fn readiness_is_waited_for_before_anything_is_asked() {
    let scratch = Scratch::new("readiness");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 400 },
        "nullWhileIndexing": true,
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut budgets = budgets();
    // Comfortably longer than the server takes to say it has begun, so that
    // "no progress was reported" cannot win the race against "it began".
    budgets.settle = Duration::from_secs(2);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert_eq!(
        semantic_edges(&answer).len(),
        1,
        "the bridge asked after the server finished indexing: {:#?}",
        answer.diff
    );
    assert!(answer.complete);
}

/// A server that begins indexing and never finishes it. The pass asks nothing,
/// records nothing, and says so - which is what keeps `semanticPassAt` unset
/// and the language's receiver gap listed.
#[test]
fn a_server_that_never_becomes_ready_reports_an_incomplete_pass() {
    let scratch = Scratch::new("never-ready");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "never", "beginAfterMs": 0 },
        "answers": answers_the_site(&scratch),
    }));
    let mut budgets = budgets();
    budgets.readiness = Duration::from_millis(600);
    budgets.settle = Duration::from_secs(2);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "a pass that asked nothing has not completed");
    assert!(reason(&answer).contains("still indexing"), "{}", reason(&answer));
    assert!(answer.diff.upsert_edges.is_empty());
    assert!(answer.diff.delete_edge_ids.is_empty(), "nothing is retracted on the strength of no answers");
}

/// `$/progress` is optional, and a server that sends none must not be made to
/// wait out the whole readiness budget for it.
#[test]
fn a_server_that_reports_no_progress_is_ready_once_it_has_settled() {
    let scratch = Scratch::new("no-progress");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut budgets = budgets();
    // Long enough that waiting it out would be obvious, so the test only
    // passes if the settle rule is what let the pass proceed.
    budgets.readiness = Duration::from_secs(20);
    budgets.settle = Duration::from_millis(100);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let started = std::time::Instant::now();
    let answer = pass(&mut bridge, &index);
    assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);
    assert!(answer.complete);
    assert!(started.elapsed() < Duration::from_secs(10), "it settled rather than waiting out readiness");
}

/// A server whose startup is a *sequence* of progress tokens is not ready in
/// the gaps between them - GM-290's correction to GM-289's rule 1.
///
/// The scripted server runs two phases with a 150ms gap and answers `null` to
/// everything until the second one ends. Under the rule this replaces - "once
/// a progress has begun, ready when all of them have ended" - the bridge asks
/// in that gap, is told nothing, and records nothing while reporting the pass
/// complete. The 300ms quiet period cannot be satisfied by a 150ms gap, so
/// the question waits for the real end and gets the real answer.
///
/// This is the fixture shape a real rust-analyzer produces: `Fetching` ends,
/// `Building CrateGraph` begins 0.28s later, and the whole startup is not
/// over for another eight seconds.
#[test]
fn a_gap_between_two_progress_phases_is_not_readiness() {
    let scratch = Scratch::new("phases");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "phases": [
            { "token": "fetching", "beginAfterMs": 0, "holdMs": 50 },
            { "token": "indexing", "beginAfterMs": 50, "holdMs": 50 },
        ]},
        "nullWhileIndexing": true,
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut budgets = budgets();
    // Forty times the scripted gap. The margin is not fussiness: the gap is a
    // `sleep` on a machine that may be loaded, and a settle merely twice as
    // long lets a stretched 50ms sleep satisfy it - which is the bug this
    // test exists to catch, passing itself off as the fix. Measured: at load
    // average 500 this failed with a 150ms gap against a 300ms settle.
    budgets.settle = Duration::from_millis(2_000);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert_eq!(
        semantic_edges(&answer).len(),
        1,
        "the gap between two phases is not the end of indexing: {:#?}",
        answer.diff
    );
    assert!(answer.complete);
}

/// The quiet period is a per-server cost, not a per-pass one.
///
/// A server that reports no progress at all becomes ready purely by the
/// clock, so the first pass pays the whole settle; the second must pay none
/// of it, or every per-file pass after an edit would spend it again.
#[test]
fn the_settle_is_paid_once_per_server_rather_than_once_per_pass() {
    let scratch = Scratch::new("settle-once");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut budgets = budgets();
    budgets.settle = Duration::from_millis(900);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let first = std::time::Instant::now();
    assert_eq!(semantic_edges(&pass(&mut bridge, &index)).len(), 1);
    let first = first.elapsed();
    let second = std::time::Instant::now();
    assert_eq!(semantic_edges(&pass(&mut bridge, &index)).len(), 1);
    let second = second.elapsed();

    assert!(first >= Duration::from_millis(800), "the first pass waits out the settle: {first:?}");
    assert!(second < Duration::from_millis(400), "the second pass does not: {second:?}");
}

/// Readiness is not only a startup condition. The server begins a second
/// progress just before answering the first question, answers `null` while it
/// runs, and ends it 300ms later. The empty answer must be re-asked rather
/// than believed.
#[test]
fn an_empty_answer_while_the_server_reindexes_is_asked_again() {
    let scratch = Scratch::new("reindex");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 0 },
        "reindex": { "atRequest": 1, "holdMs": 300 },
        "nullWhileIndexing": true,
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert_eq!(
        semantic_edges(&answer).len(),
        1,
        "the empty answer was re-asked after the reindex ended: {:#?}",
        answer.diff
    );
    assert!(answer.complete);
}

/// This machine's load average, read fresh for every timing-sensitive
/// assertion - see the house rule that a timing measurement without it is
/// worse than none. `uptime`'s exact column layout is not parsed; the whole
/// line is enough to tell a quiet run from one sharing the box with
/// something else.
fn uptime() -> String {
    std::process::Command::new("uptime")
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_else(|| "uptime unavailable".to_string())
}

/// GM-309: `LspClient::settle` latches once per server (GM-290's correction,
/// kept - see `the_settle_is_paid_once_per_server_rather_than_once_per_pass`
/// above), so from a server's second pass on, `LspBridge`'s readiness gate asks only
/// "is anything in flight *right now*", not "has it been quiet for a whole
/// settle". That is correct once the server has actually caught up with
/// whatever the pass just sent it - and false in the gap right after a
/// `didChange`, before the server has emitted *any* progress for that edit.
/// GM-299 measured this gap for pyright at ~0.6s and reasoned that it was
/// safe (a missing edge, never a wrong one) without constructing a case that
/// forces it. This test is that case.
///
/// The fake server's `reindexOnChange` answers `null` to everything from the
/// moment `didChange` arrives until a scripted delay after it, then a hold,
/// then it reveals the real answer - modelling a server that has not yet
/// noticed the edit, not one that is merely slow to answer. The first
/// question, sent immediately after the change, is answered by a local pipe
/// in low single-digit milliseconds even on a machine this loaded (measured:
/// under 2ms end to end before this fix existed to defer it at all - see the
/// "before" evidence this test's own history carries), so `beginAfterMs` of
/// 200 is not a coin flip against it, it is the race forced by construction.
///
/// `budgets.settle` is set well *above* `beginAfterMs`, and that ordering is
/// load-bearing rather than incidental: `run_pass` requeues a deferred
/// question the moment the client has been continuously quiet for one whole
/// settle, with no idea a server is about to speak. If settle were shorter
/// than `beginAfterMs`, the retry would itself fire before the server's
/// progress begins and land back in the same unrevealed gap - re-proving the
/// bug on the second try instead of testing the fix. With settle longer, the
/// server's own `$/progress` begin necessarily interrupts the quiet period
/// first (clearing it, since a client mid-progress is never "quiet"), so the
/// retry cannot fire until a full settle *after* progress has ended - by
/// which point `revealed` is already true.
///
/// Unfixed, `run_pass`'s deferral test is `!client.quiet_for(budgets.settle)`,
/// and a client whose server settled minutes ago (during pass one, in this
/// test's setup) is already quiet for far longer than one settle the instant
/// `didChange` is sent - so the early `null` is believed, no edge is emitted,
/// and the pass reports itself complete. Fixed, the same `didChange` resets
/// how long the client has been quiet *for the purposes of that judgement*,
/// so the early `null` is deferred and the site is asked again once the
/// server is quiet for real - which does not happen until after the
/// `reindexOnChange` cycle has ended and revealed the true answer.
#[test]
fn a_didchange_race_is_not_recorded_as_no_target() {
    let scratch = Scratch::new("didchange-race");
    let mut index = fixture(&scratch).0;
    let log = scratch.path().join("asked.log");
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "reindexOnChange": { "beginAfterMs": 200, "holdMs": 150 },
        "log": log.to_string_lossy(),
    }));
    let mut budgets = budgets();
    // Well above `beginAfterMs` above (5x) - see this test's own doc on why
    // that ordering, not just the margin, is what makes the retry land after
    // `revealed` rather than in the same unrevealed gap as the first ask.
    budgets.settle = Duration::from_millis(1_000);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    // Pass one: the server has no progress to report at all, so it becomes
    // ready purely by the clock, and `settled` latches - the state every
    // per-file pass after the first edit actually starts from.
    let first = pass(&mut bridge, &index);
    assert_eq!(semantic_edges(&first).len(), 1, "the baseline pass resolves the site: {:#?}", first.diff);

    // Edit b.toy - same open site, same position, different bytes - so
    // `sync_documents` sends `didChange`, not `didOpen`. A per-file pass over
    // just that file is what core runs after one reparse.
    let b = RelPath::new("src/b.toy");
    let graph = index.entry(&b).expect("the fixture has it").graph.clone();
    index.insert(b.clone(), format!("{B_TOY}// edited\n"), graph);

    let before = asked(&log, "textDocument/definition");
    let uptime_before = uptime();
    let started = std::time::Instant::now();
    let second = bridge.answer(&[b], &index).expect("the bridge answers");
    let elapsed = started.elapsed();
    let uptime_after = uptime();
    let after = asked(&log, "textDocument/definition");
    eprintln!(
        "a_didchange_race_is_not_recorded_as_no_target: pass two took {elapsed:?}; \
         uptime before {uptime_before:?}, after {uptime_after:?}"
    );

    assert_eq!(
        semantic_edges(&second).len(),
        1,
        "the early null must not be believed over the real answer the server gives once it has \
         caught up with the edit: {:#?}",
        second.diff
    );
    assert!(second.complete, "the site was eventually answered, not left outstanding");
    assert_eq!(
        after - before,
        2,
        "the site was asked once, deferred on the early null, and asked again - not answered \
         once and trusted"
    );
}

/// GM-310, the half that must not regress: a manifest saying `on-demand`
/// about a server that is in fact a rust-analyzer does not cost an edge.
///
/// This is the test the whole mechanism is chosen to pass. `on-demand` skips
/// the start-up quiet period, so the first question of the first pass goes out
/// at once - and the scripted server here is the traced rust-analyzer shape:
/// a *sequence* of work-done tokens, answering `null` to everything from
/// `initialized` until the last of them ends. Under the rejected alternative -
/// "an on-demand server has a settle of zero" - that first `null` is measured
/// against a zero-length quiet period, passes trivially, and is recorded as
/// "there is no such symbol": one missing edge bought with two seconds, which
/// this design refuses to trade.
///
/// What makes it safe instead is the rule that was already there and is
/// deliberately left alone. `run_pass` defers an empty answer that arrives
/// while the client has *not* been continuously quiet for a whole
/// `budgets.settle`, and a deferred question returns to the queue only when
/// that same continuous quiet arrives. A server mid-sequence cannot supply it:
/// the gaps between its phases are shorter than the settle (traced on
/// rust-analyzer 1.97.1 at 157-182ms against a 2s settle), and its own next
/// `begin` clears the clock. So the early `null` is not believed, the site is
/// re-asked after the sequence really ends, and the edge is the one the
/// server's truthful answer produces.
///
/// The phase sequence begins after a real delay rather than at `initialized`,
/// and that is load-bearing: rust-analyzer's own first token begins ~313ms
/// after `didOpen` (measured), and it is *that* window - ready-looking, no
/// progress yet, nothing truthful to say - the first question has to land in
/// for this test to be about anything. With `beginAfterMs: 0` the bridge would
/// simply wait for the in-flight progress in `wait_ready` and never exercise
/// the deferral at all.
///
/// Shown capable of failing: with `LspClient::quiet_for` reduced to "is
/// anything in flight right now" - GM-289's rule 1, the bug GM-290 found
/// twice - this test fails with zero semantic edges while reporting the pass
/// complete, which is the worse half of that bug and exactly what it is here
/// to catch.
#[test]
fn an_indexing_server_is_not_believed_early_even_when_the_manifest_says_on_demand() {
    let scratch = Scratch::new("on-demand-wrong");
    let (index, _) = fixture(&scratch);
    let mut config = scratch.server(json!({
        // The rust-analyzer shape: a sequence, and a pre-progress window in
        // front of it that looks exactly like a server with nothing to do.
        "readiness": { "phases": [
            { "token": "fetching", "beginAfterMs": 300, "holdMs": 60 },
            { "token": "cachePriming", "beginAfterMs": 60, "holdMs": 60 },
        ]},
        "nullWhileIndexing": true,
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    // The manifest's claim, and it is the wrong one about this server.
    config.readiness = ServerReadiness::OnDemand;

    let mut budgets = budgets();
    // 2000ms against a 60ms scripted gap - a 33x margin, the same order
    // `a_gap_between_two_progress_phases_is_not_readiness` argues for and for
    // the same reason: the gap is a `sleep` on a machine that may be loaded,
    // and a settle merely twice as long lets a stretched gap satisfy it,
    // which is the bug passing itself off as the fix.
    budgets.settle = Duration::from_millis(2_000);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let load = uptime();
    let answer = pass(&mut bridge, &index);
    assert_eq!(
        semantic_edges(&answer).len(),
        1,
        "an `on-demand` manifest must not make an indexing server's early null final ({load}): {:#?}",
        answer.diff
    );
    assert!(answer.complete, "{load}");
}

/// GM-310, the half the saving comes from: a server whose manifest says
/// `on-demand` is not made to prove, by waiting, a shape its plugin author
/// already traced.
///
/// One fixture, one scripted server, one variable: the same server that
/// reports no progress at all and answers the site correctly from its first
/// millisecond, run once under each readiness claim. The `indexed` arm is
/// today's behaviour - the first pass waits out a whole settle before asking
/// anything - and the `on-demand` arm asks at once. Both must produce the
/// same one edge, or the measurement is of a bridge that got faster by
/// answering less.
///
/// The settle is 900ms, and the second assertion is deliberately **relative**
/// rather than a second absolute bound. The claim is "this arm did not pay the
/// settle", and the honest test of it is the gap between the two arms: an
/// absolute ceiling on the on-demand arm would also be measuring how long this
/// machine takes to fork a process and complete a handshake, which is a
/// different quantity and one that has been between load average 6 and 995 on
/// the machine this was written on. Both arms absorb a slow machine together,
/// so the difference survives what a ceiling would not - while still failing
/// loudly if `on-demand` ever started waiting, since the two arms would then
/// come back within noise of each other.
///
/// This is the cheap observation that shows the arms are genuinely different
/// before anything is concluded from the difference. It is the same shape
/// `the_settle_is_paid_once_per_server_rather_than_once_per_pass` uses for the
/// first-versus-second-pass split, one variable over.
#[test]
fn an_on_demand_server_does_not_wait_out_a_settle_its_manifest_says_it_does_not_need() {
    let scratch = Scratch::new("on-demand-saving");
    let (index, _) = fixture(&scratch);
    let script = json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    });

    let mut elapsed = Vec::new();
    for readiness in [ServerReadiness::Indexed, ServerReadiness::OnDemand] {
        let mut config = scratch.server(script.clone());
        config.readiness = readiness;
        let mut budgets = budgets();
        budgets.settle = Duration::from_millis(900);
        let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

        let started = std::time::Instant::now();
        let answer = pass(&mut bridge, &index);
        elapsed.push(started.elapsed());
        assert_eq!(
            semantic_edges(&answer).len(),
            1,
            "{readiness:?} must answer the same site: {:#?}",
            answer.diff
        );
        assert!(answer.complete, "{readiness:?}");
    }

    let load = uptime();
    assert!(
        elapsed[0] >= Duration::from_millis(800),
        "the indexed arm waits out the settle: {:?} ({load})",
        elapsed[0]
    );
    let saved = elapsed[0].saturating_sub(elapsed[1]);
    assert!(
        saved >= Duration::from_millis(500),
        "the on-demand arm must skip most of the 900ms settle: indexed {:?}, on-demand {:?}, \
         saved only {saved:?} ({load})",
        elapsed[0],
        elapsed[1]
    );
}

/// `textDocument/implementation` on a node whose `nativeKind` the manifest
/// lists, and the `SUPERTYPE_OF` edge it produces - from the implementor to
/// the thing implemented, which is the direction `find_implementations` walks.
///
/// The answer is spelled with the *canonical* root, which on macOS differs
/// from the one the bridge was given: a server reports resolved paths, and a
/// bridge that only knew one spelling would map none of them.
#[test]
fn an_implementation_becomes_a_supertype_edge_from_the_implementor() {
    let scratch = Scratch::new("implementation");
    scratch.write("src/t.toy", "trait Greeter\n");
    scratch.write("src/r.toy", "type Robot\n");

    let mut index = SdkIndex::new();
    let t = RelPath::new("src/t.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &t);
    builder.file_node(range(0, 0, 1, 0));
    builder.add_node(
        NodeSpec::new(NodeKind::Type, "Greeter", "Greeter", range(0, 0, 0, 13))
            .native_kind("trait")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(t, "trait Greeter\n".to_string(), builder.finish());

    let r = RelPath::new("src/r.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &r);
    builder.file_node(range(0, 0, 1, 0));
    let robot = builder.add_node(
        NodeSpec::new(NodeKind::Type, "Robot", "Robot", range(0, 0, 0, 10))
            .native_kind("struct")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(r, "type Robot\n".to_string(), builder.finish());

    let log = scratch.path().join("asked.log");
    let mut config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "log": log.to_string_lossy(),
        "answers": [{
            // The request lands on the *name*, not on the `trait` keyword the
            // node's range starts at.
            "uri": scratch.uri("src/t.toy"),
            "line": 0,
            "character": 6,
            "implementation": [{ "uri": scratch.real_uri("src/r.toy"), "line": 0, "character": 5 }],
        }],
    }));
    config.implementation_kinds = vec!["trait".to_string()];
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 1, "{:#?}", answer.diff);
    assert_eq!(edges[0].kind, EdgeKind::SupertypeOf);
    assert_eq!(edges[0].from_id, robot, "subtype -> supertype");

    let node = placeholder(&answer, edges[0]);
    assert_eq!(node.file_path, "src/r.toy", "the placeholder lives with the edge that needs it");
    let target = node.target.as_ref().expect("a pending symbol always carries its address");
    assert_eq!(target.key, TargetKey::QualifiedName("Greeter".to_string()));
    assert!(answer.complete);

    // The other half of GM-290's second hop: a location that already names a
    // declaration costs no extra question at all.
    assert_eq!(
        asked(&log, "textDocument/definition"),
        0,
        "an implementation answer that resolved needs no follow-up"
    );
}

/// The second hop (GM-290): an implementation answer that lands on no
/// declaration is resolved with one `textDocument/definition` at that very
/// position.
///
/// This is rust-analyzer's real shape, in miniature. `r.toy`'s second line is
/// the implementing construct - an `impl` header, which no plugin emits a
/// node for - and the server answers `implementation` with a position inside
/// it. Without the follow-up, [`SdkIndex::node_at`] finds only the file, a
/// file is never an answer's target, and the whole sweep produces nothing;
/// with it, the declaration on line 0 is found and the edge starts there.
#[test]
fn an_implementation_answer_on_no_declaration_is_resolved_with_one_more_question() {
    let scratch = Scratch::new("implementation-site");
    const R_TOY: &str = "type Robot\nimpl Greeter for Robot\n";
    scratch.write("src/t.toy", "trait Greeter\n");
    scratch.write("src/r.toy", R_TOY);

    let mut index = SdkIndex::new();
    let t = RelPath::new("src/t.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &t);
    builder.file_node(range(0, 0, 1, 0));
    builder.add_node(
        NodeSpec::new(NodeKind::Type, "Greeter", "Greeter", range(0, 0, 0, 13))
            .native_kind("trait")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(t, "trait Greeter\n".to_string(), builder.finish());

    let r = RelPath::new("src/r.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &r);
    builder.file_node(range(0, 0, 2, 0));
    // The declaration covers line 0 only. Line 1 - the `impl` header the
    // server points at - is deliberately inside no node but the file's.
    let robot = builder.add_node(
        NodeSpec::new(NodeKind::Type, "Robot", "Robot", range(0, 0, 0, 10))
            .native_kind("struct")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(r, R_TOY.to_string(), builder.finish());

    let log = scratch.path().join("asked.log");
    let mut config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "log": log.to_string_lossy(),
        "answers": [
            {
                "uri": scratch.uri("src/t.toy"),
                "line": 0,
                "character": 6,
                // `Robot` in `impl Greeter for Robot`, not the declaration.
                "implementation": [{ "uri": scratch.real_uri("src/r.toy"), "line": 1, "character": 17 }],
            },
            {
                // The follow-up, answered the way a server answers "what is
                // this name": with the declaration itself.
                "uri": scratch.uri("src/r.toy"),
                "line": 1,
                "character": 17,
                "definition": { "uri": scratch.real_uri("src/r.toy"), "line": 0, "character": 5 },
            },
        ],
    }));
    config.implementation_kinds = vec!["trait".to_string()];
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 1, "the second hop found the declaration: {:#?}", answer.diff);
    assert_eq!(edges[0].kind, EdgeKind::SupertypeOf);
    assert_eq!(edges[0].from_id, robot, "the edge starts at the declaration, not at the impl");

    let node = placeholder(&answer, edges[0]);
    assert_eq!(node.file_path, "src/r.toy");
    assert_eq!(
        node.target.as_ref().map(|target| &target.key),
        Some(&TargetKey::QualifiedName("Greeter".to_string()))
    );
    assert!(answer.complete);

    assert_eq!(asked(&log, "textDocument/implementation"), 1, "one sweep question");
    assert_eq!(asked(&log, "textDocument/definition"), 1, "and exactly one follow-up, never a third");
}

/// GM-361: an `impl` header written *inside* an inline module is contained
/// by that module's node, so `node_at` answers with the module - which
/// implements nothing.
///
/// This is the sole difference from
/// [`an_implementation_answer_on_no_declaration_is_resolved_with_one_more_question`]
/// above: there the header sat at the file's top level and `node_at` found
/// only the `File` node, which was already refused. Here a `Module` node
/// covers lines 1-3, so the position the server answers with *does* land on
/// a node - and taking it produced the row measured on ripgrep,
/// `sink::sinks @ crates/searcher/src/sink.rs:516`, which is
/// `pub mod sinks { … }`. Refusing it sends the answer to the same second
/// hop the file-level case takes, which finds `Robot`.
#[test]
fn an_implementation_answer_landing_on_a_module_is_refused_and_asked_again() {
    let scratch = Scratch::new("implementation-in-module");
    const R_TOY: &str = "mod inner\n  type Robot\n  impl Greeter for Robot\n";
    scratch.write("src/t.toy", "trait Greeter\n");
    scratch.write("src/r.toy", R_TOY);

    let mut index = SdkIndex::new();
    let t = RelPath::new("src/t.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &t);
    builder.file_node(range(0, 0, 1, 0));
    builder.add_node(
        NodeSpec::new(NodeKind::Type, "Greeter", "Greeter", range(0, 0, 0, 13))
            .native_kind("trait")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(t, "trait Greeter\n".to_string(), builder.finish());

    let r = RelPath::new("src/r.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &r);
    builder.file_node(range(0, 0, 3, 0));
    // The module covers the whole of the file's body, the `impl` header
    // included - so it, not the `File` node, is what `node_at` reaches for.
    builder.add_node(
        NodeSpec::new(NodeKind::Module, "inner", "inner", range(0, 0, 2, 24))
            .native_kind("module")
            .in_container("pkg", None)
            .public(),
    );
    let robot = builder.add_node(
        NodeSpec::new(NodeKind::Type, "Robot", "inner::Robot", range(1, 2, 1, 12))
            .native_kind("struct")
            .in_container("pkg::inner", Some("pkg".to_string()))
            .public(),
    );
    index.insert(r, R_TOY.to_string(), builder.finish());

    let log = scratch.path().join("asked.log");
    let mut config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "log": log.to_string_lossy(),
        "answers": [
            {
                "uri": scratch.uri("src/t.toy"),
                "line": 0,
                "character": 6,
                // `Robot` in `  impl Greeter for Robot`, inside `mod inner`.
                "implementation": [{ "uri": scratch.real_uri("src/r.toy"), "line": 2, "character": 19 }],
            },
            {
                "uri": scratch.uri("src/r.toy"),
                "line": 2,
                "character": 19,
                "definition": { "uri": scratch.real_uri("src/r.toy"), "line": 1, "character": 7 },
            },
        ],
    }));
    config.implementation_kinds = vec!["trait".to_string()];
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 1, "one implementor, not the module too: {:#?}", answer.diff);
    assert_eq!(edges[0].kind, EdgeKind::SupertypeOf);
    assert_eq!(edges[0].from_id, robot, "the edge starts at the type, never at the module around it");
    assert_eq!(asked(&log, "textDocument/definition"), 1, "the refusal is what asks the second hop");
}

/// A question the server simply never answers costs its own budget and
/// nothing more, and the pass says it did not cover everything.
#[test]
fn a_question_that_is_never_answered_makes_the_pass_incomplete() {
    let scratch = Scratch::new("timeout");
    let (index, _) = fixture(&scratch);
    let mut answers = answers_the_site(&scratch);
    answers[0]["silent"] = json!(true);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
    }));
    let mut budgets = budgets();
    budgets.request = Duration::from_millis(400);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let started = std::time::Instant::now();
    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "an unanswered question is not an answer of 'nothing'");
    assert!(reason(&answer).contains("did not answer a question about src/b.toy"), "{}", reason(&answer));
    assert!(answer.diff.upsert_edges.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the per-request budget bounded the wait, not the pass budget"
    );
}

/// A pass that runs out of its whole budget with a question still waiting is
/// incomplete and says so, even though no single request reached its own
/// timeout.
///
/// The server is started and made ready by a per-file pass first, under its
/// own generous budget, so the short whole-project budget is spent on asking
/// alone: however slow the machine is to start a process, the pass that runs
/// out is the one asking a question that is never answered.
#[test]
fn a_pass_that_runs_out_of_its_budget_is_incomplete_and_says_why() {
    let scratch = Scratch::new("pass-budget");
    let (mut index, _) = fixture(&scratch);
    crowd_file(&scratch, &mut index, "src/c.toy", 1);
    let mut answers = answers_the_site(&scratch);
    answers.as_array_mut().expect("a list").push(json!({
        "uri": scratch.uri("src/c.toy"),
        "line": 0,
        "character": 5,
        "silent": true,
    }));
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
    }));
    let mut budgets = budgets();
    // The pass budget ends long before the request's own would.
    budgets.request = Duration::from_secs(60);
    budgets.project_floor = Duration::from_millis(300);
    budgets.per_file = Duration::from_millis(1);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let warm = bridge.answer(&[RelPath::new("src/b.toy")], &index).expect("the bridge answers");
    assert!(warm.complete, "the per-file pass was answered: {:?}", warm.reason);

    let started = std::time::Instant::now();
    let answer = pass(&mut bridge, &index);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "the pass budget ended the wait, not the request's: {:?}",
        started.elapsed()
    );
    assert!(!answer.complete, "a question cut off by the pass budget is not an answer of 'nothing'");
    assert!(reason(&answer).contains("ran out of its budget"), "{}", reason(&answer));
}

/// A server that can no longer be written to cannot be asked anything: the
/// pass is incomplete and its reason says the question could not be sent.
#[test]
fn a_server_that_cannot_be_asked_makes_the_pass_incomplete_and_says_why() {
    let scratch = Scratch::new("unwritable");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "closeInputAfterInitialized": true,
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "a question that was never sent is not an answer of 'nothing'");
    assert!(reason(&answer).contains("could not ask the language server"), "{}", reason(&answer));
    assert!(answer.diff.upsert_edges.is_empty());
}

/// A server that answers a question with an error has not answered it: the
/// pass is incomplete, and its reason carries the server's own message.
#[test]
fn a_question_the_server_answers_with_an_error_makes_the_pass_incomplete_and_says_why() {
    let scratch = Scratch::new("error");
    let (index, _) = fixture(&scratch);
    let mut answers = answers_the_site(&scratch);
    answers[0]["error"] = json!("pyright: internal error resolving the import");
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "a refused question is not an answer of 'nothing'");
    assert!(reason(&answer).contains("pyright: internal error resolving the import"), "{}", reason(&answer));
    assert!(answer.diff.upsert_edges.is_empty());
}

/// The acceptance case: the server dies mid-pass. The plugin survives, the
/// answers that did arrive are kept, the pass reports incomplete - and the
/// next pass starts a fresh server rather than staying broken.
#[test]
fn a_server_that_crashes_mid_pass_keeps_its_answers_and_the_bridge_recovers() {
    let scratch = Scratch::new("crash");
    let (mut index, caller) = fixture(&scratch);
    // A second site, which the script does not answer and the server does not
    // live to be asked about: the pass has more to do than the server has left.
    let b = RelPath::new("src/b.toy");
    let (source, mut graph) = {
        let entry = index.entry(&b).expect("the fixture has it");
        (entry.source.clone(), entry.graph.clone())
    };
    graph.open_sites.push(OpenSite {
        from_id: caller,
        position: Position { line: 1, col: 12 },
        name: "add".to_string(),
        kind: OpenSiteKind::ReceiverCall,
        edge_kind: EdgeKind::Calls,
        from_container: Some("pkg".to_string()),
        replaces: None,
    });
    index.insert(b, source, graph);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "crashAfterRequests": 1,
    }));
    let mut budgets = budgets();
    // One at a time, so "the first question is answered and then the server
    // dies" is an ordering rather than a race.
    budgets.concurrency = 1;
    budgets.request = Duration::from_secs(2);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "questions were left unasked when the server went away");
    assert!(reason(&answer).contains("exited during the pass"), "{}", reason(&answer));
    assert!(
        reason(&answer).contains("101"),
        "how the server ended is part of the reason: {}",
        reason(&answer)
    );
    assert_eq!(semantic_edges(&answer).len(), 1, "the one answer that arrived is kept: {:#?}", answer.diff);

    // The process is still standing and still usable: a second pass starts a
    // new server, which crashes again after its own first answer - proving the
    // bridge tried rather than giving up.
    let second = pass(&mut bridge, &index);
    assert_eq!(semantic_edges(&second).len(), 1, "a fresh server answered again: {:#?}", second.diff);
}

/// A server that dies between answering one question and being asked the
/// next is reported as having exited, with how it ended - not as a broken
/// pipe, which is only how its death first showed.
#[test]
fn a_server_that_dies_before_the_next_question_is_reported_as_exited_not_as_a_broken_pipe() {
    let scratch = Scratch::new("crash-epipe");
    let (mut index, caller) = fixture(&scratch);
    let b = RelPath::new("src/b.toy");
    let (source, mut graph) = {
        let entry = index.entry(&b).expect("the fixture has it");
        (entry.source.clone(), entry.graph.clone())
    };
    graph.open_sites.push(OpenSite {
        from_id: caller,
        position: Position { line: 1, col: 12 },
        name: "add".to_string(),
        kind: OpenSiteKind::ReceiverCall,
        edge_kind: EdgeKind::Calls,
        from_container: Some("pkg".to_string()),
        replaces: None,
    });
    index.insert(b, source, graph);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "crashAfterRequests": 1,
        "closeInputBeforeCrash": true,
    }));
    let mut budgets = budgets();
    budgets.concurrency = 1;
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete);
    assert!(reason(&answer).contains("exited during the pass"), "{}", reason(&answer));
    assert!(
        reason(&answer).contains("101"),
        "how the server ended is part of the reason: {}",
        reason(&answer)
    );
    assert_eq!(semantic_edges(&answer).len(), 1, "the one answer that arrived is kept: {:#?}", answer.diff);
}

/// An answer pointing at something this index does not have - another
/// repository, a generated file, the standard library - is no answer. It is
/// still an *answer*, so the pass is complete; there is simply nothing to say
/// about that site.
#[test]
fn an_answer_outside_the_index_produces_no_edge_and_no_incompleteness() {
    let scratch = Scratch::new("outside");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": [{
            "uri": scratch.uri("src/b.toy"),
            "line": 1,
            "character": SITE_UTF16_COL,
            "definition": { "uri": "file:///elsewhere/vendor/lib.toy", "line": 0, "character": 0 },
        }],
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(answer.diff.upsert_edges.is_empty(), "{:#?}", answer.diff);
    assert!(answer.complete, "the question was asked and answered");
}

/// The retraction rule: what this bridge said last time about a file it has
/// now finished again, and did not say again, is withdrawn.
#[test]
fn an_answer_that_is_no_longer_produced_is_retracted() {
    let scratch = Scratch::new("retract");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let first = pass(&mut bridge, &index);
    let edge = semantic_edges(&first).first().map(|edge| edge.id.clone()).expect("one edge");

    // The site is gone - the call was deleted - so the next pass over the same
    // file produces nothing for it.
    let mut without = SdkIndex::new();
    for (path, entry) in index.files() {
        let mut graph = entry.graph.clone();
        graph.open_sites.clear();
        without.insert(path.clone(), entry.source.clone(), graph);
    }
    let second = bridge.answer(&[], &without).expect("the bridge answers");
    assert_eq!(second.diff.delete_edge_ids, vec![edge], "the stale answer is withdrawn");
    assert!(second.complete);
}

/// The design doc's "semantic engine missing" mode, from the bridge's side:
/// no such binary, no edges, and a pass that does not claim to have finished.
#[test]
fn a_missing_server_binary_degrades_to_structural_and_reports_incomplete() {
    let scratch = Scratch::new("missing");
    let (index, _) = fixture(&scratch);
    let config = SemanticConfig::new(scratch.path().join("there-is-no-such-server"));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    for _ in 0..3 {
        let answer = pass(&mut bridge, &index);
        assert!(!answer.complete);
        assert!(reason(&answer).contains("could not be started"), "{}", reason(&answer));
        assert!(answer.diff.upsert_edges.is_empty());
    }
}

/// The site budget is a cap, and a pass that hits it says so rather than
/// reporting the sites it skipped as having no target.
#[test]
fn the_site_budget_cuts_a_pass_short_and_reports_it() {
    let scratch = Scratch::new("budget");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut budgets = budgets();
    budgets.max_sites = 0;
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "a budget that asked nothing has covered nothing");
    assert!(reason(&answer).contains("max_sites"), "{}", reason(&answer));
    assert!(answer.diff.upsert_edges.is_empty());
}

/// A per-file pass - core's `semanticPass` after one reparse - asks about that
/// file alone.
#[test]
fn a_per_file_pass_asks_only_about_that_file() {
    let scratch = Scratch::new("per-file");
    let (index, _) = fixture(&scratch);
    let log = scratch.path().join("asked.log");
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "log": log.to_string_lossy(),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = bridge.answer(&[RelPath::new("src/b.toy")], &index).expect("the bridge answers");
    assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);

    let asked = std::fs::read_to_string(&log).unwrap_or_default();
    let opened = asked.lines().filter(|line| *line == "textDocument/didOpen").count();
    assert_eq!(opened, 1, "only the file in scope was mirrored to the server:\n{asked}");
    let definitions = asked.lines().filter(|line| *line == "textDocument/definition").count();
    assert_eq!(definitions, 1);
}

/// Nothing in the bridge is allowed to name a language. This is the mechanical
/// half of that promise (the grep in the task's report is the other half): the
/// same bridge, told about a different language and a different `nativeKind`,
/// answers the same way.
#[test]
fn the_bridge_carries_no_language_of_its_own() {
    let scratch = Scratch::new("generic");
    let (index, caller) = fixture(&scratch);
    let mut produced: BTreeMap<&str, String> = BTreeMap::new();
    for language in ["toy", "banana", "ml"] {
        let config = scratch.server(json!({
            "readiness": { "kind": "none" },
            "positionEncoding": "utf-16",
            "answers": answers_the_site(&scratch),
        }));
        let mut bridge = LspBridge::with_budgets(language, scratch.path(), config, budgets());
        let answer = pass(&mut bridge, &index);
        let edges = semantic_edges(&answer);
        assert_eq!(edges.len(), 1, "{language}: {:#?}", answer.diff);
        assert_eq!(edges[0].from_id, caller);
        produced.insert(language, edges[0].id.clone());
    }
    let ids: Vec<&String> = produced.values().collect();
    assert!(
        ids.windows(2).all(|pair| pair[0] == pair[1]),
        "the answer does not depend on the name: {produced:?}"
    );
}

/// A document is opened under the `languageId` its extension maps to in
/// `LspBridge::language_ids`, and under the bridge's language without one -
/// read from what the server received.
#[test]
fn a_document_is_opened_under_the_language_id_its_extension_maps_to() {
    let scratch = Scratch::new("language-ids");
    let (index, _) = fixture(&scratch);
    let opened_as = |by_extension: &'static [(&'static str, &'static str)], tag: &str| {
        let log = scratch.path().join(format!("opened-{tag}.log"));
        let config = scratch.server(json!({
            "readiness": { "kind": "none" },
            "positionEncoding": "utf-16",
            "answers": answers_the_site(&scratch),
            "openedLog": log.to_string_lossy(),
        }));
        let mut bridge =
            LspBridge::with_budgets("toy", scratch.path(), config, budgets()).language_ids(by_extension);
        let answer = bridge.answer(&[RelPath::new("src/b.toy")], &index).expect("the bridge answers");
        assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);
        std::fs::read_to_string(&log).unwrap_or_default()
    };
    let b = scratch.uri("src/b.toy");
    assert_eq!(opened_as(&[(".other", "other"), ("b.toy", "toy-b")], "mapped"), format!("{b} toy-b\n"));
    assert_eq!(opened_as(&[(".other", "other")], "unmapped"), format!("{b} toy\n"));
}

/// The second settings channel (GM-299): a server that *asks* for its
/// settings gets the ones the config carries, positionally, and `null` for a
/// section nobody configured.
///
/// This is the one direction the rest of this file cannot exercise - the
/// server making a request of the client - and it is the only channel pyright
/// reads at all (`lsp::config`'s module doc has that measurement). The
/// assertion is on what the *server received*, written back out to a file,
/// rather than on what the client believed it sent: a reply that never leaves
/// the client is indistinguishable from a correct one at this end.
#[test]
fn a_server_that_asks_for_its_settings_is_answered_from_the_config() {
    let scratch = Scratch::new("settings");
    let (index, _caller) = fixture(&scratch);
    let received = scratch.path().join("configuration.json");
    let mut config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "askConfiguration": ["toy", "nobody-configured-this"],
        "configurationOut": received.to_string_lossy(),
    }));
    config.settings.insert("toy".to_string(), json!({ "analysis": { "mode": "basic" } }));

    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());
    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "the settings exchange must not disturb the pass itself");
    assert_eq!(semantic_edges(&answer).len(), 1, "and the pass still answers: {:#?}", answer.diff);

    // Dropping the bridge runs `shutdown`/`exit` and waits for the child, and
    // that is what makes this read deterministic rather than a race: one pipe
    // preserves order, so a reply the client sent during the pass is a frame
    // the server necessarily read before the `exit` it has now acted on.
    drop(bridge);
    let text = std::fs::read_to_string(&received).expect("the server wrote down what it was answered");
    let sent: Value = serde_json::from_str(&text).expect("and it is the JSON the client sent");
    assert_eq!(
        sent,
        json!([{ "analysis": { "mode": "basic" } }, null]),
        "one value per item, in the order asked, with an unconfigured section null: {text}"
    );
}

/// The discrimination for the test above: the same server asking the same
/// question of a bridge whose config carries no settings is answered `null` -
/// so the test above is measuring the settings and not the request.
#[test]
fn a_server_that_asks_with_nothing_configured_is_answered_null() {
    let scratch = Scratch::new("settings-none");
    let (index, _caller) = fixture(&scratch);
    let received = scratch.path().join("configuration.json");
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "askConfiguration": ["toy", "nobody-configured-this"],
        "configurationOut": received.to_string_lossy(),
    }));
    assert!(config.settings.is_empty(), "the arm's whole difference");

    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());
    assert!(pass(&mut bridge, &index).complete);

    drop(bridge);
    let text = std::fs::read_to_string(&received).expect("the server wrote down what it was answered");
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json!([null, null]), "{text}");
}

// --- the site ceiling (GM-319) ----------------------------------------------

/// Adds a file of `sites` unresolved receiver calls, one per line, each with
/// its own calling declaration.
///
/// One caller per site rather than one per file, deliberately: two sites that
/// share a `from_id` and resolve to the same target are the *same* edge, and
/// `Answers` deduplicates it - so a fixture built the other way would count
/// half the edges it thinks it has, and a test about retracting them would be
/// measuring the deduplication instead.
fn crowd_file(scratch: &Scratch, index: &mut SdkIndex, relative: &str, sites: usize) {
    let source: String = (0..sites).map(|line| format!("  r{line}.m()\n")).collect();
    scratch.write(relative, &source);

    let path = RelPath::new(relative);
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &path);
    builder.file_node(range(0, 0, sites as u32, 0));
    for line in 0..sites as u32 {
        let name = format!("{}#{line}", relative.replace(['/', '.'], "_"));
        let caller = builder.add_node(
            NodeSpec::new(NodeKind::Function, &name, &name, range(line, 0, line, 10))
                .native_kind("function")
                .in_container("pkg", None)
                .public(),
        );
        builder.open_site(OpenSite {
            from_id: caller,
            position: Position { line, col: 5 },
            name: "m".to_string(),
            kind: OpenSiteKind::ReceiverCall,
            edge_kind: EdgeKind::Calls,
            from_container: Some("pkg".to_string()),
            replaces: None,
        });
    }
    index.insert(path, source, builder.finish());
}

/// **The GM-319 regression.** A repository whose open sites outnumber the
/// pre-GM-319 ceiling of 20,000 must still record a *completed* whole-project
/// pass. An incomplete one leaves `language_state.semanticPassAt` unset - and
/// leaves it unset forever, because nothing about the project changes to make
/// the next pass smaller: the receiver-call gap stays in every session's MCP
/// instructions and the whole pass is redone on every daemon start. GM-314
/// measured 87,832 questions for Django, 27,750 for tokio and 21,995 for
/// g-mesh itself (GM-319's re-count), so this is not a hypothetical size.
///
/// Everything but `max_sites` is loosened here so that a loaded machine
/// cannot fail this for a reason the test is not about; `max_sites` is read
/// from `Budgets::default()` rather than written down, which is the whole
/// point. On the pre-GM-319 default this fails at `complete`.
#[test]
fn a_project_larger_than_the_old_ceiling_still_completes_its_pass() {
    const SITES: usize = 21_000;
    const OLD_CEILING: usize = 20_000;
    const FILES: usize = 40;

    const { assert!(SITES > OLD_CEILING, "this fixture must be over the ceiling that used to cut it") };
    let shipped = Budgets::default().max_sites;

    let scratch = Scratch::new("ceiling");
    let mut index = SdkIndex::new();
    for file in 0..FILES {
        crowd_file(&scratch, &mut index, &format!("src/crowd{file}.toy"), SITES / FILES);
    }

    let log = scratch.path().join("asked.log");
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": [],
        "log": log.to_string_lossy(),
    }));
    let budgets = Budgets {
        request: Duration::from_secs(120),
        max_sites: shipped,
        concurrency: 8,
        project_floor: Duration::from_secs(600),
        per_file: Duration::from_secs(600),
        single_file: Duration::from_secs(600),
        readiness: Duration::from_secs(60),
        settle: Duration::from_millis(150),
        warm_up: None,
    };
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(
        answer.complete,
        "a {SITES}-site project must record a completed pass, not one reported cut short - the \
         shipped ceiling is {shipped}"
    );
    // A server that answers nothing still *answers* every question - "nothing
    // there" is an answer. Counting the requests is what proves the ceiling
    // let the list through, rather than that the questions were cheap.
    assert_eq!(
        asked(&log, "textDocument/definition"),
        SITES,
        "every site must have been put to the server, not merely counted"
    );
}

/// A pass the ceiling cuts short must not retract an earlier pass's answers
/// about a file it never reached.
///
/// Before GM-319 the cut fell wherever the *n*th question happened to be,
/// which is usually the middle of a file. Every question that file did
/// contribute was asked and answered, so `run_pass` reported it covered, and
/// `retract_stale` reads covered as "this pass is now the whole truth about
/// that file" - withdrawing the edges an earlier, complete pass emitted for
/// the sites this one was cut before reaching. Correct edges deleted to
/// account for questions nobody asked, which is the direction the bridge's
/// retraction rules exist to forbid.
///
/// Two files of two sites each and a ceiling of three. Three is the whole
/// arithmetic: it can only be spent by cutting the second file in half.
#[test]
fn a_ceiling_that_cuts_a_pass_short_retracts_nothing_it_did_not_ask_about() {
    let scratch = Scratch::new("ceiling-retract");
    let mut index = SdkIndex::new();

    // The declaration every site resolves to, in a file of its own so that no
    // answer lands inside the file that asked - and with no sites, so it
    // costs the ceiling nothing.
    let decl = RelPath::new("src/decl.toy");
    let decl_source = "fn target\n";
    scratch.write("src/decl.toy", decl_source);
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &decl);
    builder.file_node(range(0, 0, 1, 0));
    builder.add_node(
        NodeSpec::new(NodeKind::Function, "target", "target", range(0, 3, 0, 9))
            .native_kind("function")
            .in_container("pkg", None)
            .public(),
    );
    index.insert(decl, decl_source.to_string(), builder.finish());

    crowd_file(&scratch, &mut index, "src/x.toy", 2);
    crowd_file(&scratch, &mut index, "src/y.toy", 2);

    let mut answers = Vec::new();
    for file in ["src/x.toy", "src/y.toy"] {
        for line in 0..2u32 {
            answers.push(json!({
                "uri": scratch.uri(file),
                "line": line,
                "character": 5,
                "definition": { "uri": scratch.uri("src/decl.toy"), "line": 0, "character": 4 },
            }));
        }
    }
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
    }));

    let mut budgets = budgets();
    budgets.max_sites = 3;
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    // A per-file pass over `y.toy` alone fits under the same ceiling, and is
    // what gives the whole-project pass below something to be tempted to
    // retract. (Core sends exactly this after every settled reparse.)
    let first = bridge.answer(&[RelPath::new("src/y.toy")], &index).expect("the bridge answers");
    assert!(first.complete, "two questions fit under a ceiling of three");
    assert_eq!(semantic_edges(&first).len(), 2, "y.toy's own sites: {:#?}", first.diff);

    // Now the whole project, at a ceiling that fits `decl.toy` (nothing) and
    // `x.toy` (two) but not `y.toy`'s two as well.
    let second = pass(&mut bridge, &index);
    assert!(!second.complete, "a pass the ceiling cut short is not a completed pass");
    assert!(
        second.diff.delete_edge_ids.is_empty(),
        "a file the ceiling stopped this pass from reaching keeps the answers it already has - \
         retracted {:?}",
        second.diff.delete_edge_ids
    );
    assert_eq!(
        semantic_edges(&second).len(),
        2,
        "and the file that did fit is asked about in full: {:#?}",
        second.diff
    );
}

/// GM-433 (GM-429 finding 1): after a `workspaceChanged`, the next pass waits
/// out the server's whole reload rather than the first gap in it.
///
/// The scripted server is rust-analyzer after a version bump: it notices the
/// manifest change itself (here, a trigger file), then reports two phases
/// with a gap between them - "Building compile-time-deps", ~13ms of silence,
/// "Building CrateGraph" and the rest - answering `null` throughout. Pass one
/// has already latched the settle, which is the state a warm server is in
/// when the bump arrives. The test waits until the reload's first `begin` is
/// on the wire before it tells the bridge about the change and runs the
/// pass, so the pass starts mid-reload by construction, not by a race.
///
/// Unfixed, the latched client reads the end of phase one as ready, asks into
/// the gap, is answered `null`, defers, and re-asks after phase two: two
/// `definition` requests for one site - the "asks every question twice" of
/// the finding. Fixed, `workspace_changed` unlatches the settle and the one
/// question goes out once the server has been quiet for a whole settle.
/// Settle is 1s against a 30ms gap, a margin that survives a loaded machine
/// (see `a_gap_between_two_progress_phases_is_not_readiness`).
#[test]
fn after_a_workspace_change_each_question_is_asked_once_after_the_reload() {
    let scratch = Scratch::new("workspace-reload");
    let (index, _) = fixture(&scratch);
    let log = scratch.path().join("asked.log");
    let trigger = scratch.path().join("manifest.bumped");
    let started = scratch.path().join("reload.started");
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
        "log": log.to_string_lossy(),
        "reloadOnFile": {
            "trigger": trigger.to_string_lossy(),
            "started": started.to_string_lossy(),
            "phases": [
                { "token": "compile-time-deps", "holdMs": 100 },
                { "token": "crate-graph", "beginAfterMs": 30, "holdMs": 600 },
            ],
        },
    }));
    let mut budgets = budgets();
    budgets.settle = Duration::from_millis(1_000);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    // Pass one latches the settle, as on any warm server.
    let first = pass(&mut bridge, &index);
    assert_eq!(semantic_edges(&first).len(), 1, "the baseline pass resolves the site: {:#?}", first.diff);

    // The bump: the server starts reloading, and core says so.
    std::fs::write(&trigger, "").expect("write the trigger");
    let waiting = std::time::Instant::now();
    while !started.exists() {
        assert!(waiting.elapsed() < Duration::from_secs(10), "the scripted reload never began");
        std::thread::sleep(Duration::from_millis(5));
    }
    bridge.workspace_changed();

    let before = asked(&log, "textDocument/definition");
    let uptime_before = uptime();
    let clock = std::time::Instant::now();
    let second = pass(&mut bridge, &index);
    let elapsed = clock.elapsed();
    let after = asked(&log, "textDocument/definition");
    eprintln!(
        "after_a_workspace_change_each_question_is_asked_once_after_the_reload: pass two took \
         {elapsed:?}; uptime before {uptime_before:?}, after {:?}",
        uptime()
    );

    assert_eq!(semantic_edges(&second).len(), 1, "the site is resolved: {:#?}", second.diff);
    assert!(second.complete, "{:?}", second.reason);
    assert_eq!(
        after - before,
        1,
        "the question is asked once, after the reload - not into the gap between its phases and \
         then again"
    );
}

/// GM-433, the half that must not regress: `workspaceChanged` makes an
/// indexed server's next pass pay the settle again, and leaves an on-demand
/// one alone (see `LspClient::unsettle` for that decision).
///
/// Both arms run one pass (latching), then a change, then a second pass
/// timed. A server with no progress becomes ready purely by the clock, so
/// the indexed arm's second pass waits out the 900ms settle only if the
/// change unlatched it; the on-demand arm's must not. Relative, like
/// `an_on_demand_server_does_not_wait_out_a_settle_its_manifest_says_it_does_not_need`.
#[test]
fn a_workspace_change_costs_an_indexed_server_a_settle_and_an_on_demand_one_nothing() {
    let scratch = Scratch::new("workspace-settle");
    let (index, _) = fixture(&scratch);
    let script = json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    });

    let mut elapsed = Vec::new();
    for readiness in [ServerReadiness::Indexed, ServerReadiness::OnDemand] {
        let mut config = scratch.server(script.clone());
        config.readiness = readiness;
        let mut budgets = budgets();
        budgets.settle = Duration::from_millis(900);
        let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

        assert_eq!(semantic_edges(&pass(&mut bridge, &index)).len(), 1, "{readiness:?}");
        bridge.workspace_changed();
        let started = std::time::Instant::now();
        let answer = pass(&mut bridge, &index);
        elapsed.push(started.elapsed());
        assert_eq!(semantic_edges(&answer).len(), 1, "{readiness:?}: {:#?}", answer.diff);
        assert!(answer.complete, "{readiness:?}");
    }

    let load = uptime();
    assert!(
        elapsed[0] >= Duration::from_millis(800),
        "the indexed arm waits out a settle after the change: {:?} ({load})",
        elapsed[0]
    );
    let saved = elapsed[0].saturating_sub(elapsed[1]);
    assert!(
        saved >= Duration::from_millis(500),
        "the on-demand arm must not: indexed {:?}, on-demand {:?}, saved only {saved:?} ({load})",
        elapsed[0],
        elapsed[1]
    );
}

/// GM-433: LSP `ContentModified` (-32801) is "ask again", not a refusal.
/// rust-analyzer answers it to every request in flight when it switches crate
/// graphs; before this, one of them made the whole pass incomplete and the
/// next daemon start reran it.
#[test]
fn a_content_modified_answer_is_asked_again_and_the_pass_is_complete() {
    let scratch = Scratch::new("content-modified");
    let (index, _) = fixture(&scratch);
    let log = scratch.path().join("asked.log");
    let mut answers = answers_the_site(&scratch);
    answers[0]["error"] = json!("content modified");
    answers[0]["errorCode"] = json!(-32801);
    answers[0]["errorTimes"] = json!(1);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
        "log": log.to_string_lossy(),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "a ContentModified is not a refusal: {:?}", answer.reason);
    assert_eq!(semantic_edges(&answer).len(), 1, "the re-ask is answered: {:#?}", answer.diff);
    assert_eq!(asked(&log, "textDocument/definition"), 2, "asked, modified, asked again");
}

/// GM-433, the bound on the above: "again" is once, under the same
/// `re_asked` rule as an empty answer. A server that answers
/// `ContentModified` every time is refused on the second, and the pass says
/// so - rather than re-asking until its budget runs out.
#[test]
fn a_question_modified_twice_is_refused_rather_than_asked_forever() {
    let scratch = Scratch::new("content-modified-twice");
    let (index, _) = fixture(&scratch);
    let log = scratch.path().join("asked.log");
    let mut answers = answers_the_site(&scratch);
    answers[0]["error"] = json!("content modified");
    answers[0]["errorCode"] = json!(-32801);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
        "log": log.to_string_lossy(),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete);
    assert!(reason(&answer).contains("content modified"), "{}", reason(&answer));
    assert_eq!(asked(&log, "textDocument/definition"), 2, "asked twice, not more");
}

/// `prepare` (core's `prepareSemanticPass`) starts the server before any
/// pass is asked, the pass then uses that server instead of a second one,
/// and the pass answers exactly what a pass on an unprepared bridge answers.
/// Without `prepare`, constructing the bridge starts nothing.
///
/// Control: make `LspBridge::prepare` a no-op (the trait default) and the
/// "started before the pass" assertion fails.
#[test]
fn a_prepared_server_starts_before_the_pass_and_answers_the_same() {
    let scratch = Scratch::new("prepare");
    let (index, _) = fixture(&scratch);
    let script = |log: &Path| {
        json!({
            "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 0 },
            "positionEncoding": "utf-16",
            "answers": answers_the_site(&scratch),
            "log": log.to_string_lossy(),
        })
    };

    let unprepared_log = scratch.path().join("unprepared.log");
    let mut unprepared =
        LspBridge::with_budgets("toy", scratch.path(), scratch.server(script(&unprepared_log)), budgets());
    assert_eq!(asked(&unprepared_log, "initialize"), 0, "constructing a bridge starts no server");
    let baseline = pass(&mut unprepared, &index);
    drop(unprepared);

    let prepared_log = scratch.path().join("prepared.log");
    let mut prepared =
        LspBridge::with_budgets("toy", scratch.path(), scratch.server(script(&prepared_log)), budgets());
    prepared.prepare();
    assert_eq!(asked(&prepared_log, "initialize"), 1, "prepare starts and initializes the server");
    assert_eq!(asked(&prepared_log, "textDocument/definition"), 0, "and asks it nothing yet");

    let answer = pass(&mut prepared, &index);
    assert_eq!(asked(&prepared_log, "initialize"), 1, "the pass uses the prepared server");
    assert!(answer.complete && baseline.complete);
    assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);
    assert_eq!(answer.diff, baseline.diff, "preparing changes when the server starts, not what it answers");
}

/// A server started by `prepare` is still made to prove its readiness inside
/// the pass: one that indexes, answering `null` meanwhile, is waited for.
///
/// Control: have `prepare` mark the client settled (or skip `wait_ready` for
/// a prepared server) and the pass asks during indexing and finds no edge.
#[test]
fn a_prepared_server_is_still_waited_for_by_the_pass() {
    let scratch = Scratch::new("prepare-ready");
    let (index, _) = fixture(&scratch);
    let config = scratch.server(json!({
        "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 600 },
        "nullWhileIndexing": true,
        "positionEncoding": "utf-16",
        "answers": answers_the_site(&scratch),
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    bridge.prepare();
    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(semantic_edges(&answer).len(), 1, "asked only once the server had finished indexing");
}

/// A missing server binary found by `prepare` turns the tier off exactly as
/// a pass's own start would: the pass reports it, starting nothing.
#[test]
fn a_missing_server_found_by_prepare_is_reported_by_the_pass() {
    let scratch = Scratch::new("prepare-missing");
    let (index, _) = fixture(&scratch);
    let config = SemanticConfig::new(scratch.path().join("there-is-no-such-server"));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    bridge.prepare();
    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete);
    assert!(reason(&answer).contains("could not be started"), "{}", reason(&answer));
}

// --- GM-486: untyped receiver calls answered outside the index ---------------

/// A file whose one function, `id`, calls each of `sites` - `(name, line)`,
/// column 4 - through a receiver the structural tier cannot type, so each
/// name is on its `untypedCalls`.
fn untyped_caller(
    scratch: &Scratch,
    index: &mut SdkIndex,
    relative: &str,
    id: &str,
    sites: &[(&str, u32)],
) -> String {
    let mut source = format!("fn {id}() {{\n");
    for (name, _) in sites {
        source.push_str(&format!("  x.{name}();\n"));
    }
    source.push_str("}\n");
    scratch.write(relative, &source);

    let path = RelPath::new(relative);
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &path);
    builder.record_untyped_receiver_calls();
    let lines = sites.len() as u32 + 2;
    builder.file_node(range(0, 0, lines, 0));
    let caller = builder.add_node(
        NodeSpec::new(NodeKind::Function, id, id, range(0, 0, lines - 1, 1))
            .native_kind("function")
            .in_container("pkg", None)
            .public(),
    );
    for (name, line) in sites {
        builder.open_site(OpenSite {
            from_id: caller.clone(),
            position: Position { line: *line, col: 4 },
            name: name.to_string(),
            kind: OpenSiteKind::ReceiverCall,
            edge_kind: EdgeKind::Calls,
            from_container: Some("pkg".to_string()),
            replaces: None,
        });
    }
    index.insert(path, source, builder.finish());
    caller
}

/// A script entry answering the site at `(line, 4)` of `relative` with a std
/// declaration, a file this index does not hold.
fn answered_by_std(scratch: &Scratch, relative: &str, line: u32) -> Value {
    json!({
        "uri": scratch.uri(relative),
        "line": line,
        "character": 4,
        "definition": { "uri": "file:///rustlib/src/rust/library/alloc/src/vec/mod.rs", "line": 9, "character": 11 },
    })
}

/// The `untypedCalls` each re-sent node of `answer` carries, by id.
fn re_sent(answer: &SemanticAnswer) -> BTreeMap<String, Vec<String>> {
    answer
        .diff
        .upsert_nodes
        .iter()
        .filter(|node| node.kind == NodeKind::Function)
        .map(|node| (node.id.clone(), node.untyped_calls.clone()))
        .collect()
}

/// **GM-486.** Only files the pass both asked about and finished are
/// trimmed. `c.toy` had its `len` answered by std too, but its `hang`
/// question never came back, so the pass did not finish that file and its
/// caller keeps its list - core is not sent it at all.
///
/// Control: in `LspBridge::answer`, pass `&asked_about` to
/// `trim_untyped_calls` instead of `&finished` (`c_caller` is re-sent with
/// `len` dropped).
#[test]
fn a_file_the_pass_did_not_finish_keeps_its_untyped_calls() {
    let scratch = Scratch::new("untyped-partial");
    let mut index = SdkIndex::new();
    let b_caller = untyped_caller(&scratch, &mut index, "src/b.toy", "b_caller", &[("len", 1)]);
    let c_caller = untyped_caller(&scratch, &mut index, "src/c.toy", "c_caller", &[("len", 1), ("hang", 2)]);
    let mut silent = answered_by_std(&scratch, "src/c.toy", 2);
    silent["silent"] = json!(true);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": [answered_by_std(&scratch, "src/b.toy", 1), answered_by_std(&scratch, "src/c.toy", 1), silent],
    }));
    let mut budgets = budgets();
    budgets.request = Duration::from_millis(400);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "the `hang` question was never answered");
    assert_eq!(
        re_sent(&answer),
        BTreeMap::from([(b_caller, Vec::new())]),
        "b.toy was finished and its one call answered by std; {c_caller} is in an unfinished file"
    );
}

/// **GM-486, across passes.** A call std answered leaves its caller's list
/// on every pass that answers it - each starts from the structural list, so
/// a structural reparse that re-sent the full list in between is undone -
/// and comes back on the first pass whose answer stops arriving. After
/// that, with nothing trimmed, the caller is not re-sent.
///
/// The answer "stops" by moving the site to a column the scripted server
/// has no answer for, the same text and the same name.
///
/// Controls, in `trim_untyped_calls`: drop the `else if
/// previously.contains(..)` branch (pass 3 sends nothing, so `len` never
/// comes back); or forget `LspBridge::trimmed` between passes - pass a
/// fresh map instead of `&mut self.trimmed` (likewise).
#[test]
fn a_trimmed_name_comes_back_when_its_answer_stops() {
    let scratch = Scratch::new("untyped-back");
    let mut index = SdkIndex::new();
    let caller = untyped_caller(&scratch, &mut index, "src/b.toy", "b_caller", &[("len", 1)]);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": [answered_by_std(&scratch, "src/b.toy", 1)],
    }));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets());

    let mut unanswered = SdkIndex::new();
    for (path, entry) in index.files() {
        let mut graph = entry.graph.clone();
        for site in &mut graph.open_sites {
            site.position.col = 2;
        }
        unanswered.insert(path.clone(), entry.source.clone(), graph);
    }

    let mut passes = Vec::new();
    for index in [&index, &index, &unanswered, &unanswered] {
        let answer = pass(&mut bridge, index);
        assert!(answer.complete, "{:?}", answer.reason);
        assert!(answer.diff.upsert_edges.is_empty(), "std is never an edge: {:#?}", answer.diff);
        passes.push(re_sent(&answer));
    }
    let sent =
        |names: &[&str]| BTreeMap::from([(caller.clone(), names.iter().map(|n| n.to_string()).collect())]);
    assert_eq!(passes[0], sent(&[]), "pass 1: answered by std, `len` drops");
    assert_eq!(passes[1], sent(&[]), "pass 2: trimmed again from the structural list");
    assert_eq!(passes[2], sent(&["len"]), "pass 3: no answer, `len` comes back");
    assert_eq!(passes[3], BTreeMap::new(), "pass 4: nothing trimmed now or before");
}

// --- GM-489: one edge per typed receiver call -----------------------------

/// Where a GM-489 fixture's structural edge `X` lands.
#[derive(Clone, Copy)]
enum Bound {
    /// On the declaration itself, in the caller's own file (`X.to_id == D.id`).
    Here,
    /// On a placeholder addressed exactly as an answer would address `D` in
    /// another file, so an agreeing answer would get `X`'s own id.
    There,
    /// On a placeholder addressed at a re-exporting container (`reexp`), as a
    /// call through a `pub use` re-export is: an answer on `D` gets an id of
    /// its own, never `X`'s, so only core's link result shows agreement.
    ReExport,
}

/// `src/d.toy` declares `add` and `sub`, the cross-file targets.
const D_TOY: &str = "fn add\nfn sub\n";
/// `src/f.toy` declares its own `add` and `sub` and a `caller` whose
/// receiver calls sit at column 4 of lines 3 to 7. Which line a site is on
/// decides what the scripted server answers for it.
const F_TOY: &str = "fn add\nfn sub\nfn caller\n  x.add()\n  x.add()\n  x.add()\n  x.add()\n  x.add()\n";
/// Answered with `add`, the structural edge's own target.
const AGREE: u32 = 3;
/// Answered with `sub`, somewhere else.
const CONTRA: u32 = 4;
/// Never scripted: the server answers `null`.
const EMPTY: u32 = 5;
/// Answered with `add`, for an untyped site.
const UNTYPED: u32 = 6;
/// Never answered at all, which leaves `f.toy` unfinished.
const SILENT: u32 = 7;

struct Receiver {
    index: SdkIndex,
    caller: String,
    /// The structural edge every typed site names in `replaces`.
    x: String,
}

/// The GM-489 index: `caller` in `f.toy` with one structural `CALLS` edge
/// `X` onto `add` (where `bound` says), typed sites on `typed` that name `X`
/// in `replaces`, and untyped sites on `untyped`. `X` is in the graph
/// whatever the sites are, as it is after any reparse that keeps the call.
fn receiver_fixture(scratch: &Scratch, bound: Bound, typed: &[u32], untyped: &[u32]) -> Receiver {
    typed_site_fixture(scratch, bound, OpenSiteKind::ReceiverCall, typed, untyped)
}

/// The GM-497 index: [`receiver_fixture`]'s shape for a typed field read.
/// `add` and `sub` are fields (`Variable`, native `field`), `X` is a
/// `REFERENCES` edge, and every site is a `ReceiverField` - what
/// `plugins/rust`'s `Bodies::field_access` emits for `x.add`.
fn field_fixture(scratch: &Scratch, bound: Bound, typed: &[u32], untyped: &[u32]) -> Receiver {
    typed_site_fixture(scratch, bound, OpenSiteKind::ReceiverField, typed, untyped)
}

/// [`receiver_fixture`] and [`field_fixture`]: `site_kind` decides the
/// declarations' kind, `X`'s edge kind and the sites' kind.
fn typed_site_fixture(
    scratch: &Scratch,
    bound: Bound,
    site_kind: OpenSiteKind,
    typed: &[u32],
    untyped: &[u32],
) -> Receiver {
    let (node_kind, native_kind, edge_kind) = match site_kind {
        OpenSiteKind::ReceiverField => (NodeKind::Variable, "field", EdgeKind::References),
        _ => (NodeKind::Function, "function", EdgeKind::Calls),
    };
    scratch.write("src/d.toy", D_TOY);
    scratch.write("src/f.toy", F_TOY);
    let declare = |builder: &mut FileGraphBuilder| -> [String; 2] {
        ["add", "sub"].iter().enumerate().fold([String::new(), String::new()], |mut ids, (line, name)| {
            ids[line] = builder.add_node(
                NodeSpec::new(node_kind, *name, *name, range(line as u32, 3, line as u32, 6))
                    .native_kind(native_kind)
                    .in_container("pkg", None)
                    .public(),
            );
            ids
        })
    };

    let mut index = SdkIndex::new();
    let d = RelPath::new("src/d.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &d);
    builder.file_node(range(0, 0, 2, 0));
    declare(&mut builder);
    let d_graph = builder.finish();
    let d_add = d_graph.nodes.iter().find(|node| node.name == "add").unwrap().clone();
    index.insert(d, D_TOY.to_string(), d_graph);

    let f = RelPath::new("src/f.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &f);
    builder.record_untyped_receiver_calls();
    builder.file_node(range(0, 0, 8, 0));
    let [local_add, _] = declare(&mut builder);
    let caller = builder.add_node(
        NodeSpec::new(NodeKind::Function, "caller", "caller", range(2, 0, 7, 9))
            .native_kind("function")
            .in_container("pkg", None)
            .public(),
    );
    let x = match bound {
        Bound::Here => builder.resolved_edge(edge_kind, &caller, &local_add),
        Bound::There => {
            let placeholder = builder.add_placeholder(
                g_mesh_plugin_sdk::PlaceholderKind::PendingSymbol,
                "add",
                g_mesh_plugin_sdk::wire::PlaceholderTarget {
                    scope: TargetScope::Container("pkg".to_string()),
                    key: TargetKey::QualifiedName(d_add.qualified_name.clone()),
                    from_container: Some("pkg".to_string()),
                    key_path: d_add.qualified_path.clone(),
                },
                range(AGREE, 4, AGREE, 7),
            );
            builder.placeholder_edge(edge_kind, &caller, &placeholder)
        }
        Bound::ReExport => {
            let placeholder = builder.add_placeholder(
                g_mesh_plugin_sdk::PlaceholderKind::PendingSymbol,
                "add",
                g_mesh_plugin_sdk::wire::PlaceholderTarget {
                    scope: TargetScope::Container("reexp".to_string()),
                    key: TargetKey::QualifiedName("reexp::add".to_string()),
                    from_container: Some("pkg".to_string()),
                    key_path: d_add.qualified_path.clone(),
                },
                range(AGREE, 4, AGREE, 7),
            );
            builder.placeholder_edge(edge_kind, &caller, &placeholder)
        }
    };
    let sites =
        typed.iter().map(|line| (*line, Some(x.clone()))).chain(untyped.iter().map(|line| (*line, None)));
    for (line, replaces) in sites {
        builder.open_site(OpenSite {
            from_id: caller.clone(),
            position: Position { line, col: 4 },
            name: "add".to_string(),
            kind: site_kind,
            edge_kind,
            from_container: Some("pkg".to_string()),
            replaces,
        });
    }
    index.insert(f, F_TOY.to_string(), builder.finish());
    Receiver { index, caller, x }
}

/// A bridge whose server answers `f.toy`'s sites by line: `AGREE` and
/// `UNTYPED` with `add`, `CONTRA` with `sub` - in `f.toy` for [`Bound::Here`]
/// and in `d.toy` for [`Bound::There`] - `EMPTY` with `null`, and `SILENT`
/// never.
fn receiver_bridge(scratch: &Scratch, bound: Bound) -> LspBridge {
    let target = match bound {
        Bound::Here => "src/f.toy",
        Bound::There | Bound::ReExport => "src/d.toy",
    };
    let at = |line: u32, declaration: u32| {
        json!({
            "uri": scratch.uri("src/f.toy"),
            "line": line,
            "character": 4,
            "definition": { "uri": scratch.uri(target), "line": declaration, "character": 3 },
        })
    };
    let mut silent = at(SILENT, 0);
    silent["silent"] = json!(true);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": [at(AGREE, 0), at(CONTRA, 1), at(UNTYPED, 0), silent],
    }));
    let mut budgets = budgets();
    budgets.request = Duration::from_millis(400);
    LspBridge::with_budgets("toy", scratch.path(), config, budgets)
}

/// Every upsert of edge `id` in `answer`.
fn upserts<'a>(answer: &'a SemanticAnswer, id: &str) -> Vec<&'a WireEdge> {
    answer.diff.upsert_edges.iter().filter(|edge| edge.id == id).collect()
}

/// `X` exactly as the index holds it, the one edge `answer` sends under that
/// id, and not among the deletes: the call's row stands, still syntactic.
fn x_re_sent_unchanged(answer: &SemanticAnswer, fixture: &Receiver, pass: &str) {
    let held = fixture
        .index
        .graph(&RelPath::new("src/f.toy"))
        .and_then(|graph| graph.edges.iter().find(|edge| edge.id == fixture.x))
        .expect("the fixture holds X");
    assert_eq!(
        upserts(answer, &fixture.x),
        vec![held],
        "{pass}: X re-sent once, unchanged: {:#?}",
        answer.diff
    );
    assert_eq!(held.source, SourceTier::Syntactic);
    assert!(
        !answer.diff.delete_edge_ids.contains(&fixture.x),
        "{pass}: X is not retracted: {:#?}",
        answer.diff
    );
}

/// **GM-489, T1 (R2, `Bound::Here`).** A typed call whose answer lands on
/// the structural edge's own target records no semantic edge and retracts
/// nothing: `X` is re-sent as it is, so the call keeps one row.
///
/// Control: in `record_answer`, drop `lands_on_it ||` from the agreement test
/// (the answer becomes a semantic edge and `X` is retracted).
#[test]
fn a_typed_call_its_server_confirms_in_the_same_file_stays_one_structural_edge() {
    let scratch = Scratch::new("gm489-here-agree");
    let fixture = receiver_fixture(&scratch, Bound::Here, &[AGREE], &[]);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "agreement adds nothing: {:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
}

/// **GM-489, T2 (R2 `Bound::There`, then R1).** A cross-file typed call the
/// server confirms, whose answer would get `X`'s own id, adds nothing; a
/// later pass whose answer is empty (the site moved to a position the server
/// answers `null` for) neither retracts `X` nor loses it - it re-sends it.
///
/// Controls: pass 1 - in `record_answer`, drop `|| &prospective == replaced`
/// (a semantic edge under `X`'s id is sent and `X` is retracted). Pass 2 -
/// in `Answers::settle`, skip `self.resend.push(edge.clone())` (`X` is not
/// re-sent).
#[test]
fn a_cross_file_call_the_server_confirms_survives_a_later_empty_pass() {
    let scratch = Scratch::new("gm489-there-agree-empty");
    let agree = receiver_fixture(&scratch, Bound::There, &[AGREE], &[]);
    let empty = receiver_fixture(&scratch, Bound::There, &[EMPTY], &[]);
    assert_eq!(agree.x, empty.x, "an edit that moves the call keeps X's id");
    let mut bridge = receiver_bridge(&scratch, Bound::There);

    let first = pass(&mut bridge, &agree.index);
    assert!(first.complete, "{:?}", first.reason);
    assert!(semantic_edges(&first).is_empty(), "agreement adds nothing: {:#?}", first.diff);
    x_re_sent_unchanged(&first, &agree, "pass 1 (agrees)");

    let second = pass(&mut bridge, &empty.index);
    assert!(second.complete, "{:?}", second.reason);
    assert!(semantic_edges(&second).is_empty(), "{:#?}", second.diff);
    x_re_sent_unchanged(&second, &empty, "pass 2 (empty)");
}

/// **GM-489, T3 (contradiction, then R1).** An answer that lands elsewhere
/// retracts `X` and emits its own edge `E'`; a later pass whose answer is
/// empty retracts `E'` and restores `X` in the same diff, so the call has
/// one row before and after.
///
/// Control: in `Answers::settle`, skip `self.resend.push(edge.clone())`
/// (pass 2 retracts `E'` and sends no `X`: the call has no row).
#[test]
fn a_contradicted_call_is_restored_by_a_later_empty_pass() {
    let scratch = Scratch::new("gm489-contra-empty");
    let contra = receiver_fixture(&scratch, Bound::Here, &[CONTRA], &[]);
    let empty = receiver_fixture(&scratch, Bound::Here, &[EMPTY], &[]);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let first = pass(&mut bridge, &contra.index);
    assert!(first.complete, "{:?}", first.reason);
    let emitted = semantic_edges(&first);
    assert_eq!(emitted.len(), 1, "{:#?}", first.diff);
    assert_eq!(emitted[0].from_id, contra.caller);
    let e_prime = emitted[0].id.clone();
    assert!(upserts(&first, &contra.x).is_empty(), "a contradicted X is not re-sent");
    assert_eq!(first.diff.delete_edge_ids, vec![contra.x.clone()], "X is retracted");

    let second = pass(&mut bridge, &empty.index);
    assert!(second.complete, "{:?}", second.reason);
    assert!(semantic_edges(&second).is_empty(), "{:#?}", second.diff);
    assert_eq!(second.diff.delete_edge_ids, vec![e_prime], "E' is retracted, X is not");
    x_re_sent_unchanged(&second, &empty, "pass 2 (empty)");
}

/// **GM-489, T4 (contradiction, then agreement, file unchanged but for the
/// answer).** Pass 2 retracts `E'`, re-sends `X`, and emits no semantic edge.
/// The cross-file shape, so the agreement is by id.
///
/// Controls: in `Answers::settle`, skip `self.resend.push(edge.clone())`
/// (pass 2 sends no `X`); in `record_answer`, drop `|| &prospective ==
/// replaced` (pass 2 sends a semantic edge under `X`'s id and retracts `X`).
#[test]
fn a_contradicted_call_is_restored_by_a_later_agreeing_pass() {
    let scratch = Scratch::new("gm489-contra-agree");
    let contra = receiver_fixture(&scratch, Bound::There, &[CONTRA], &[]);
    let agree = receiver_fixture(&scratch, Bound::There, &[AGREE], &[]);
    let mut bridge = receiver_bridge(&scratch, Bound::There);

    let first = pass(&mut bridge, &contra.index);
    assert!(first.complete, "{:?}", first.reason);
    let emitted = semantic_edges(&first);
    assert_eq!(emitted.len(), 1, "{:#?}", first.diff);
    assert_ne!(emitted[0].id, contra.x, "E' lands elsewhere, so its id is its own");
    let e_prime = emitted[0].id.clone();
    assert_eq!(first.diff.delete_edge_ids, vec![contra.x.clone()], "X is retracted");

    let second = pass(&mut bridge, &agree.index);
    assert!(second.complete, "{:?}", second.reason);
    assert!(semantic_edges(&second).is_empty(), "{:#?}", second.diff);
    assert_eq!(second.diff.delete_edge_ids, vec![e_prime], "E' is retracted, X is not");
    x_re_sent_unchanged(&second, &agree, "pass 2 (agrees)");
}

/// **GM-489, T5 (R3, `Bound::Here`).** A caller with a typed call the server
/// confirms and an untyped call it answers with the same declaration: the
/// untyped answer's semantic edge `(caller, CALLS, add)` is covered by `X`,
/// so the diff holds `X` and no semantic edge or placeholder - and the
/// untyped call still counts as answered (its name leaves `untypedCalls`).
///
/// Control: in `Answers::settle`'s `covered`, drop the `|| lands.get(..)`
/// arm (an `E_sem(caller, add)` and its placeholder are sent beside `X`).
#[test]
fn a_typed_call_covers_an_untyped_call_that_reaches_the_same_declaration() {
    let scratch = Scratch::new("gm489-mixed-here");
    let fixture = receiver_fixture(&scratch, Bound::Here, &[AGREE], &[UNTYPED]);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "X already says caller calls add: {:#?}", answer.diff);
    assert!(
        answer.diff.upsert_nodes.iter().all(|node| node.native_kind.as_deref() != Some("pending_symbol")),
        "no edge, so no placeholder: {:#?}",
        answer.diff.upsert_nodes
    );
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
    assert_eq!(
        re_sent(&answer),
        BTreeMap::from([(fixture.caller.clone(), Vec::new())]),
        "the untyped call was answered, so `add` leaves the caller's list"
    );
}

/// **GM-489, T5b (R3 by id, `Bound::There`).** The typed site's answer is
/// empty; the untyped site's answer lands on `add` in `d.toy`, which is
/// exactly `X`'s address, so the semantic edge it records *is* `X`'s id.
/// The diff must still carry one edge under that id, the syntactic one.
///
/// Control: in `Answers::settle`'s `covered`, drop `ids.contains(..) ||` (a
/// second, semantic upsert under `X`'s id is sent beside the re-sent `X`).
#[test]
fn an_untyped_answer_with_the_structural_edges_id_is_dropped_for_it() {
    let scratch = Scratch::new("gm489-mixed-there");
    let fixture = receiver_fixture(&scratch, Bound::There, &[EMPTY], &[UNTYPED]);
    let mut bridge = receiver_bridge(&scratch, Bound::There);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
}

/// **GM-489, T6.** Two typed sites name one `X`; one answer agrees and one
/// lands elsewhere. `X` is not retracted, because not every answer about it
/// contradicted it - it is re-sent - while the contradicting site keeps its
/// own edge `E'`.
///
/// Control: in `Answers::settle`, drop `&& !self.upheld.contains(replaced)`
/// (retract on any contradiction: `X` is deleted and not re-sent).
#[test]
fn a_structural_edge_one_site_upholds_is_kept_though_another_contradicts_it() {
    let scratch = Scratch::new("gm489-split");
    let fixture = receiver_fixture(&scratch, Bound::Here, &[AGREE, CONTRA], &[]);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(semantic_edges(&answer).len(), 1, "the contradicting site's own edge: {:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
}

/// **GM-489, the unfinished file.** `f.toy` has a typed call the server
/// confirms, an untyped call whose answer has `X`'s own id (`Bound::There`),
/// and a question that is never answered, so the pass does not finish
/// `f.toy` and settles nothing there. What this records:
///
/// - Pass 1 (unfinished) sends the untyped answer's edge under `X`'s id
///   with `source = semantic`, and retracts nothing. In core that is an
///   upsert of `X`'s own row: still one row for the call, but labelled
///   semantic until the next pass that finishes the file.
/// - Pass 2 (finished, the silent site gone) sends `X` back unchanged and
///   syntactic, and does *not* retract it, although pass 1 remembered that
///   id as emitted for `f.toy`.
///
/// Control (pass 2): in `Answers::finish`, drop `&& !resent.contains(id)`
/// from the delete filter (`X` is both re-sent and retracted).
#[test]
fn an_unfinished_file_relabels_the_structural_edge_and_the_next_finished_pass_restores_it() {
    let scratch = Scratch::new("gm489-unfinished");
    let unfinished = receiver_fixture(&scratch, Bound::There, &[AGREE], &[UNTYPED, SILENT]);
    let finished = receiver_fixture(&scratch, Bound::There, &[AGREE], &[UNTYPED]);
    let mut bridge = receiver_bridge(&scratch, Bound::There);

    let first = pass(&mut bridge, &unfinished.index);
    assert!(!first.complete, "the SILENT question was never answered");
    let under_x = upserts(&first, &unfinished.x);
    assert_eq!(under_x.len(), 1, "one edge under X's id: {:#?}", first.diff);
    assert_eq!(under_x[0].source, SourceTier::Semantic, "the untyped answer's copy, not X itself");
    assert_eq!(under_x[0].from_id, unfinished.caller);
    assert!(first.diff.delete_edge_ids.is_empty(), "nothing is retracted in an unfinished file");

    let second = pass(&mut bridge, &finished.index);
    assert!(second.complete, "{:?}", second.reason);
    assert!(semantic_edges(&second).is_empty(), "{:#?}", second.diff);
    x_re_sent_unchanged(&second, &finished, "pass 2 (finished)");
}

// --- GM-497: typed field reads, asked like typed calls ----------------------
//
// The GM-489 fixtures with the sites turned into `ReceiverField` and `X` into
// a `REFERENCES` edge onto a field. `record_answer` and `Answers::settle` do
// not look at the site's kind, so these pin that a field read reaches them
// at all - `questions` asks it - and that the rules then hold for it too.
// Control for every test here: in `questions`, route `ReceiverField` to
// `continue` (nothing is asked: `X` is not re-sent, and no edge is emitted).

/// What the semantic `edge` of `answer` lands on: the name of the placeholder
/// the answer became, which addresses the declaration the server named.
fn lands_on<'a>(answer: &'a SemanticAnswer, edge: &WireEdge) -> &'a str {
    let node = answer.diff.upsert_nodes.iter().find(|node| node.id == edge.to_id);
    &node.unwrap_or_else(|| panic!("the answer sends its target: {:#?}", answer.diff)).name
}

/// **GM-497, item 12 (R2, `Bound::Here`).** A typed field read whose answer
/// lands on the structural edge's own target records nothing new: `X` is
/// re-sent as it is, one row, syntactic.
#[test]
fn a_typed_field_read_its_server_confirms_in_the_same_file_stays_one_structural_edge() {
    let scratch = Scratch::new("gm497-here-agree");
    let fixture = field_fixture(&scratch, Bound::Here, &[AGREE], &[]);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "agreement adds nothing: {:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
    assert_eq!(upserts(&answer, &fixture.x)[0].kind, EdgeKind::References);
}

/// **GM-497, item 13 (R2 `Bound::There`, then R1).** A cross-file field read
/// the server confirms, whose answer would get `X`'s own id, records
/// nothing; a later empty pass keeps `X`.
#[test]
fn a_cross_file_field_read_the_server_confirms_survives_a_later_empty_pass() {
    let scratch = Scratch::new("gm497-there-agree-empty");
    let agree = field_fixture(&scratch, Bound::There, &[AGREE], &[]);
    let empty = field_fixture(&scratch, Bound::There, &[EMPTY], &[]);
    assert_eq!(agree.x, empty.x, "an edit that moves the read keeps X's id");
    let mut bridge = receiver_bridge(&scratch, Bound::There);

    let first = pass(&mut bridge, &agree.index);
    assert!(first.complete, "{:?}", first.reason);
    assert!(semantic_edges(&first).is_empty(), "agreement adds nothing: {:#?}", first.diff);
    x_re_sent_unchanged(&first, &agree, "pass 1 (agrees)");

    let second = pass(&mut bridge, &empty.index);
    assert!(second.complete, "{:?}", second.reason);
    assert!(semantic_edges(&second).is_empty(), "{:#?}", second.diff);
    x_re_sent_unchanged(&second, &empty, "pass 2 (empty)");
}

/// **GM-497, item 14 (contradiction, then R1).** An answer on another field
/// emits a semantic `REFERENCES` edge onto that field and retracts `X`; a
/// later empty pass retracts it and restores `X`.
#[test]
fn a_contradicted_field_read_is_restored_by_a_later_empty_pass() {
    let scratch = Scratch::new("gm497-contra-empty");
    let contra = field_fixture(&scratch, Bound::Here, &[CONTRA], &[]);
    let empty = field_fixture(&scratch, Bound::Here, &[EMPTY], &[]);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let first = pass(&mut bridge, &contra.index);
    assert!(first.complete, "{:?}", first.reason);
    let emitted = semantic_edges(&first);
    assert_eq!(emitted.len(), 1, "{:#?}", first.diff);
    assert_eq!(
        (emitted[0].from_id.as_str(), emitted[0].kind, lands_on(&first, emitted[0])),
        (contra.caller.as_str(), EdgeKind::References, "sub"),
        "the read references the field the server named"
    );
    let e_prime = emitted[0].id.clone();
    assert!(upserts(&first, &contra.x).is_empty(), "a contradicted X is not re-sent");
    assert_eq!(first.diff.delete_edge_ids, vec![contra.x.clone()], "X is retracted");

    let second = pass(&mut bridge, &empty.index);
    assert!(second.complete, "{:?}", second.reason);
    assert!(semantic_edges(&second).is_empty(), "{:#?}", second.diff);
    assert_eq!(second.diff.delete_edge_ids, vec![e_prime], "E' is retracted, X is not");
    x_re_sent_unchanged(&second, &empty, "pass 2 (empty)");
}

/// **GM-497, item 15 (R1).** An empty answer and an ambiguous one (two
/// fields) both uphold `X`: no semantic edge, nothing retracted, `X`
/// re-sent. Control, besides the section's: in `Answers::settle`, skip
/// `self.resend.push(edge.clone())` (`X` is not re-sent).
#[test]
fn an_empty_or_ambiguous_answer_upholds_a_typed_field_read() {
    let scratch = Scratch::new("gm497-uphold");
    let fixture = field_fixture(&scratch, Bound::Here, &[EMPTY, UNTYPED], &[]);
    let config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": [{
            "uri": scratch.uri("src/f.toy"),
            "line": UNTYPED,
            "character": 4,
            "definitions": [
                { "uri": scratch.uri("src/f.toy"), "line": 0, "character": 3 },
                { "uri": scratch.uri("src/f.toy"), "line": 1, "character": 3 },
            ],
        }],
    }));
    let mut budgets = budgets();
    budgets.request = Duration::from_millis(400);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
}

/// **GM-497, items 11 and 17.** An untyped field read (no `replaces`) is
/// asked, and its answer is an ordinary semantic `REFERENCES` edge; it never
/// becomes an untyped *call*: the caller's `untypedCalls` is empty and no
/// caller is re-sent. Controls: route `ReceiverField` to `continue` in
/// `questions` (no edge); count `ReceiverField` in `fold_untyped_calls`
/// (the caller lists `add`).
#[test]
fn an_untyped_field_read_is_asked_and_is_never_an_untyped_call() {
    let scratch = Scratch::new("gm497-untyped");
    let fixture = field_fixture(&scratch, Bound::Here, &[], &[UNTYPED]);
    let graph = fixture.index.graph(&RelPath::new("src/f.toy")).unwrap();
    let caller = graph.nodes.iter().find(|node| node.id == fixture.caller).unwrap();
    assert!(caller.untyped_calls.is_empty(), "a field read is not a call: {:?}", caller.untyped_calls);
    let mut bridge = receiver_bridge(&scratch, Bound::Here);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    let emitted = semantic_edges(&answer);
    assert_eq!(emitted.len(), 1, "{:#?}", answer.diff);
    assert_eq!(
        (emitted[0].from_id.as_str(), emitted[0].kind, lands_on(&answer, emitted[0])),
        (fixture.caller.as_str(), EdgeKind::References, "add")
    );
    assert!(re_sent(&answer).is_empty(), "no untypedCalls to trim: {:#?}", answer.diff.upsert_nodes);
}

// --- Re-exports: core's link result decides agreement -----------------------
//
// `X` points at a placeholder addressed at the re-exporting container
// (`Bound::ReExport`), so neither R2 test that looks at `X` alone matches an
// answer on `D` in `d.toy`. What core's linker did with `X` comes with the
// pass (`SdkIndex::set_linked`), and the bridge decides agreement with it.

/// The fixture of `site_kind` with `X` through a re-export, and core's link
/// result saying it linked `X` onto `d.toy`'s `linked_to`.
fn reexport_fixture(scratch: &Scratch, site_kind: OpenSiteKind, typed: &[u32], linked_to: &str) -> Receiver {
    let mut fixture = typed_site_fixture(scratch, Bound::ReExport, site_kind, typed, &[]);
    let target = fixture
        .index
        .graph(&RelPath::new("src/d.toy"))
        .and_then(|graph| graph.nodes.iter().find(|node| node.name == linked_to))
        .map(|node| node.id.clone())
        .expect("d.toy declares the linked target");
    fixture.index.set_linked([(fixture.x.clone(), target)]);
    fixture
}

/// A typed call through a re-export, which core linked onto `D` and the
/// server answers with `D`: agreement. No semantic edge, `X` not retracted
/// and re-sent as it is, so the call keeps one row.
#[test]
fn a_typed_call_through_a_re_export_core_linked_onto_the_answer_stays_one_structural_edge() {
    let scratch = Scratch::new("reexport-call-agree");
    let fixture = reexport_fixture(&scratch, OpenSiteKind::ReceiverCall, &[AGREE], "add");
    let mut bridge = receiver_bridge(&scratch, Bound::ReExport);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "agreement adds nothing: {:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
}

/// The same for a typed field read (`REFERENCES` onto a field).
#[test]
fn a_typed_field_read_through_a_re_export_core_linked_onto_the_answer_stays_one_structural_edge() {
    let scratch = Scratch::new("reexport-field-agree");
    let fixture = reexport_fixture(&scratch, OpenSiteKind::ReceiverField, &[AGREE], "add");
    let mut bridge = receiver_bridge(&scratch, Bound::ReExport);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "agreement adds nothing: {:#?}", answer.diff);
    x_re_sent_unchanged(&answer, &fixture, "the only pass");
    assert_eq!(upserts(&answer, &fixture.x)[0].kind, EdgeKind::References);
}

/// Core linked `X` onto `sub` and the server answers `add`: a contradiction
/// as for any other edge. The answer becomes a semantic edge onto `add` and
/// `X` is retracted - the link result agrees only on the very declaration.
#[test]
fn a_call_through_a_re_export_core_linked_elsewhere_is_still_a_contradiction() {
    let scratch = Scratch::new("reexport-contra");
    let fixture = reexport_fixture(&scratch, OpenSiteKind::ReceiverCall, &[AGREE], "sub");
    let mut bridge = receiver_bridge(&scratch, Bound::ReExport);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    let emitted = semantic_edges(&answer);
    assert_eq!(emitted.len(), 1, "{:#?}", answer.diff);
    assert_eq!(
        (emitted[0].from_id.as_str(), lands_on(&answer, emitted[0])),
        (fixture.caller.as_str(), "add"),
        "the call lands where the server said"
    );
    assert!(upserts(&answer, &fixture.x).is_empty(), "a contradicted X is not re-sent");
    assert_eq!(answer.diff.delete_edge_ids, vec![fixture.x.clone()], "X is retracted");
}

/// After an edit the follow-up pass does not finish (one site is never
/// answered). Pass 1 agreed through core's link, so there is no semantic
/// edge to clean up: neither pass records one or retracts `X`, and the call
/// has one row at every point.
#[test]
fn a_failed_pass_after_an_agreeing_re_export_pass_leaves_one_row() {
    let scratch = Scratch::new("reexport-failed-follow-up");
    let agree = reexport_fixture(&scratch, OpenSiteKind::ReceiverCall, &[AGREE], "add");
    let unfinished = reexport_fixture(&scratch, OpenSiteKind::ReceiverCall, &[AGREE, SILENT], "add");
    assert_eq!(agree.x, unfinished.x, "the edit keeps X's id");
    let mut bridge = receiver_bridge(&scratch, Bound::ReExport);

    let first = pass(&mut bridge, &agree.index);
    assert!(first.complete, "{:?}", first.reason);
    assert!(semantic_edges(&first).is_empty(), "pass 1 agrees: {:#?}", first.diff);
    x_re_sent_unchanged(&first, &agree, "pass 1 (agrees)");

    let second = pass(&mut bridge, &unfinished.index);
    assert!(!second.complete, "the SILENT question was never answered");
    assert!(semantic_edges(&second).is_empty(), "pass 2 records no edge: {:#?}", second.diff);
    assert!(
        !second.diff.delete_edge_ids.contains(&unfinished.x),
        "pass 2 does not retract X: {:#?}",
        second.diff
    );
}

/// A per-file pass over `files`.
fn pass_over(bridge: &mut LspBridge, index: &SdkIndex, files: &[&str]) -> SemanticAnswer {
    let files: Vec<RelPath> = files.iter().map(|file| RelPath::new(*file)).collect();
    bridge.answer(&files, index).expect("the bridge answers rather than failing")
}

// --- GM-348: overload binding -----------------------------------------------
//
// `src/o.toy` declares two overload sets, each one node carrying three
// `declarations` (two bodiless stubs, then the implementation, one per line),
// and a plain `g`:
//
// ```text
// 0  over f int      f, ordinal 0      (the node's own range is this `f`)
// 1  over f str      f, ordinal 1
// 2  body f any      f, ordinal 2, has_body
// 3  fn g            g, no declarations
// 4  over m int      m, ordinal 0
// 5  over m str      m, ordinal 1
// 6  body m any      m, ordinal 2, has_body
// ```
//
// Qualified names are `o::f` and so on, so that a `Name("f")` placeholder
// and one addressed as an answer addresses `f` are two placeholders.
//
// `src/c.toy`'s `caller` calls into it; which sites the index records is
// each test's choice ([`OSite`]). Every `f` call's structural edge is `E_f`,
// onto a `Name`-keyed placeholder (`Bound::There`, the plugin cannot know the
// target is overloaded), so every test here also exercises the filter's
// placeholder arm (B10b): a site onto `E_f` is asked only because `f`'s key
// names an overload set elsewhere in the index.

const O_TOY: &str = "over f int\nover f str\nbody f any\nfn g\nover m int\nover m str\nbody m any\n";
const C_TOY: &str = "fn caller\n  f(1)\n  f(s)\n  g()\n  x.m(s)\n  x.f()\n";
/// Where every declaration's name is written in `o.toy`.
const NAME_COL: u32 = 5;

/// The sites [`overload_fixture`] can record in `c.toy`.
#[derive(Clone, Copy, PartialEq)]
enum OSite {
    /// `f(1)`, line 1: `OverloadCall` naming `E_f`.
    F1,
    /// `f(s)`, line 2: `OverloadCall` naming `E_f`.
    Fs,
    /// `g()`, line 3: `OverloadCall` naming `E_g`, onto a set-less target.
    G,
    /// `g()`, line 3: `OverloadCall` with no `replaces`.
    GBare,
    /// `x.m(s)`, line 4: an untyped `ReceiverCall`.
    M,
    /// `x.f()`, line 5: a typed `ReceiverCall` naming `E_x`, a second
    /// structural edge from `caller` addressed exactly as an answer
    /// addresses `f`, so an agreeing answer confirms it.
    Xf,
}

impl OSite {
    fn at(self) -> (u32, u32) {
        match self {
            OSite::F1 => (1, 2),
            OSite::Fs => (2, 2),
            OSite::G | OSite::GBare => (3, 2),
            OSite::M => (4, 4),
            OSite::Xf => (5, 4),
        }
    }
}

struct Overloads {
    index: SdkIndex,
    caller: String,
    e_f: String,
    e_g: String,
    e_x: String,
}

fn stubs(first_line: u32) -> Vec<g_mesh_plugin_sdk::wire::WireDeclaration> {
    (0..3)
        .map(|ordinal| g_mesh_plugin_sdk::wire::WireDeclaration {
            ordinal,
            start_line: first_line + ordinal,
            start_col: 0,
            end_line: first_line + ordinal,
            end_col: 10,
            signature: None,
            has_body: ordinal == 2,
        })
        .collect()
}

fn overload_fixture(scratch: &Scratch, sites: &[OSite]) -> Overloads {
    scratch.write("src/o.toy", O_TOY);
    scratch.write("src/c.toy", C_TOY);
    let mut index = SdkIndex::new();

    let o = RelPath::new("src/o.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &o);
    builder.file_node(range(0, 0, 7, 0));
    for (name, line, declarations) in [("f", 0, stubs(0)), ("g", 3, Vec::new()), ("m", 4, stubs(4))] {
        let col = if name == "g" { 3 } else { NAME_COL };
        builder.add_node(
            NodeSpec::new(NodeKind::Function, name, format!("o::{name}"), range(line, col, line, col + 1))
                .native_kind("function")
                .in_container("pkg", None)
                .public()
                .declarations(declarations),
        );
    }
    let o_graph = builder.finish();
    let f = o_graph.nodes.iter().find(|node| node.name == "f").unwrap().clone();
    index.insert(o, O_TOY.to_string(), o_graph);

    let c = RelPath::new("src/c.toy");
    let mut builder = FileGraphBuilder::new("toy", "toy-parser", &c);
    builder.file_node(range(0, 0, 6, 0));
    let caller = builder.add_node(
        NodeSpec::new(NodeKind::Function, "caller", "caller", range(0, 0, 5, 7))
            .native_kind("function")
            .in_container("pkg", None)
            .public(),
    );
    let edge_onto = |builder: &mut FileGraphBuilder, name: &str, key: TargetKey, key_path, line: u32| {
        let placeholder = builder.add_placeholder(
            g_mesh_plugin_sdk::PlaceholderKind::PendingSymbol,
            name,
            g_mesh_plugin_sdk::wire::PlaceholderTarget {
                scope: TargetScope::Container("pkg".to_string()),
                key,
                from_container: Some("pkg".to_string()),
                key_path,
            },
            range(line, 2, line, 3),
        );
        builder.placeholder_edge(EdgeKind::Calls, &caller, &placeholder)
    };
    let e_f = edge_onto(&mut builder, "f", TargetKey::Name("f".to_string()), None, 1);
    let e_g = edge_onto(&mut builder, "g", TargetKey::Name("g".to_string()), None, 3);
    let e_x =
        edge_onto(&mut builder, "f", TargetKey::QualifiedName(f.qualified_name.clone()), f.qualified_path, 5);
    for site in sites {
        let (kind, replaces, name) = match site {
            OSite::F1 | OSite::Fs => (OpenSiteKind::OverloadCall, Some(e_f.clone()), "f"),
            OSite::G => (OpenSiteKind::OverloadCall, Some(e_g.clone()), "g"),
            OSite::GBare => (OpenSiteKind::OverloadCall, None, "g"),
            OSite::M => (OpenSiteKind::ReceiverCall, None, "m"),
            OSite::Xf => (OpenSiteKind::ReceiverCall, Some(e_x.clone()), "f"),
        };
        let (line, col) = site.at();
        builder.open_site(OpenSite {
            from_id: caller.clone(),
            position: Position { line, col },
            name: name.to_string(),
            kind,
            edge_kind: EdgeKind::Calls,
            from_container: Some("pkg".to_string()),
            replaces,
        });
    }
    index.insert(c, C_TOY.to_string(), builder.finish());
    assert_ne!(e_f, e_x, "a Name key and a QualifiedName key are two placeholders");
    Overloads { index, caller, e_f, e_g, e_x }
}

/// A scripted answer at `site`: `definitions` are `o.toy` lines (each at the
/// declaration's name), `hover` the call's markdown.
fn at_site(scratch: &Scratch, site: OSite, definitions: &[u32], hover: Option<&str>) -> Value {
    let (line, character) = site.at();
    let definitions: Vec<Value> = definitions
        .iter()
        .map(|line| json!({ "uri": scratch.uri("src/o.toy"), "line": line, "character": NAME_COL }))
        .collect();
    let mut answer = json!({
        "uri": scratch.uri("src/c.toy"), "line": line, "character": character, "definitions": definitions,
    });
    if let Some(hover) = hover {
        answer["hover"] = json!(hover);
    }
    answer
}

/// The hover a candidate declaration on `o.toy` line `line` answers with.
fn at_declaration(scratch: &Scratch, line: u32, hover: &str) -> Value {
    json!({ "uri": scratch.uri("src/o.toy"), "line": line, "character": NAME_COL, "hover": hover })
}

/// pyright's hover markdown: the signature in a fenced block, then docs.
fn fenced(signature: &str) -> String {
    format!("```python\n{signature}\n```\n---\nSome documentation.")
}

fn overload_bridge(
    scratch: &Scratch,
    answers: Vec<Value>,
    disambiguation: g_mesh_plugin_sdk::lsp::OverloadDisambiguation,
    concurrency: usize,
) -> (LspBridge, PathBuf) {
    let log = scratch.path().join("asked.log");
    let mut config = scratch.server(json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
        "log": log.to_string_lossy(),
    }));
    config.overload_disambiguation = disambiguation;
    let mut budgets = budgets();
    budgets.concurrency = concurrency;
    (LspBridge::with_budgets("toy", scratch.path(), config, budgets), log)
}

use g_mesh_plugin_sdk::lsp::OverloadDisambiguation::{Hover as ByHover, None as NoHover};

/// The semantic edges `answer` binds to an ordinal, as `(from, ordinal)`.
fn bound(answer: &SemanticAnswer) -> Vec<(String, u32)> {
    let mut bound: Vec<(String, u32)> = semantic_edges(answer)
        .into_iter()
        .filter_map(|edge| edge.to_declaration.map(|ordinal| (edge.from_id.clone(), ordinal)))
        .collect();
    bound.sort();
    bound
}

/// The structural edge `id` is re-sent and not retracted: the call keeps its
/// one structural row, unrefined.
fn kept(answer: &SemanticAnswer, id: &str, what: &str) {
    assert_eq!(upserts(answer, id).len(), 1, "{what}: re-sent: {:#?}", answer.diff);
    assert!(!answer.diff.delete_edge_ids.iter().any(|deleted| deleted == id), "{what}: not retracted");
}

/// The structural edge `id` is retracted and not re-sent.
fn retracted(answer: &SemanticAnswer, id: &str, what: &str) {
    assert!(answer.diff.delete_edge_ids.iter().any(|deleted| deleted == id), "{what}: {:#?}", answer.diff);
    assert!(upserts(answer, id).is_empty(), "{what}: not also re-sent");
}

/// **GM-348 B3.** A server that answers an overloaded call with one location
/// (tsserver's shape) binds it by containment in the set's `declarations` -
/// here on the *second* stub, which lies outside the node's own range (the
/// first stub's name), so node containment alone would land on nothing.
/// The bound edge replaces the structural one.
///
/// Control: in `declaration_at`, test the node's own `range` instead of each
/// declaration's (or make `overload_landing` use `node_at` only) - no
/// ordinal, the call stays unbound, `E_f` is re-sent.
#[test]
fn a_single_location_binds_the_declaration_that_contains_it() {
    let scratch = Scratch::new("gm348-b3");
    let fixture = overload_fixture(&scratch, &[OSite::Fs]);
    let (mut bridge, _) =
        overload_bridge(&scratch, vec![at_site(&scratch, OSite::Fs, &[1], None)], NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(bound(&answer), vec![(fixture.caller.clone(), 1)], "{:#?}", answer.diff);
    retracted(&answer, &fixture.e_f, "every site of E_f bound");
}

/// **GM-348 B7 (and B5's retraction).** Two calls from one caller binding two
/// overloads are two edges with distinct ids onto one placeholder, each
/// carrying its ordinal as `toDeclaration` on the wire; their structural edge
/// is retracted.
///
/// Controls: pass `None` to `edge_id` in `Answers::record` (one edge, not
/// two); write `to_declaration: None` in `Answers::finish` (the wire field is
/// absent).
#[test]
fn two_overloads_called_from_one_caller_are_two_edges_onto_one_placeholder() {
    let scratch = Scratch::new("gm348-b7");
    let fixture = overload_fixture(&scratch, &[OSite::F1, OSite::Fs]);
    let answers = vec![at_site(&scratch, OSite::F1, &[0], None), at_site(&scratch, OSite::Fs, &[1], None)];
    let (mut bridge, _) = overload_bridge(&scratch, answers, NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 2, "{:#?}", answer.diff);
    assert_ne!(edges[0].id, edges[1].id);
    assert_eq!(edges[0].to_id, edges[1].to_id, "one placeholder");
    let placeholders = answer.diff.upsert_nodes.iter().filter(|node| node.target.is_some()).count();
    assert_eq!(placeholders, 1, "{:#?}", answer.diff.upsert_nodes);
    let mut on_the_wire: Vec<Value> = edges
        .iter()
        .map(|edge| serde_json::to_value(edge).expect("an edge serializes")["toDeclaration"].clone())
        .collect();
    on_the_wire.sort_by_key(|value| value.as_u64());
    assert_eq!(on_the_wire, vec![json!(0), json!(1)]);
    retracted(&answer, &fixture.e_f, "both sites bound");
}

/// **GM-348 B4.** All or nothing per structural edge: one site of `E_f`
/// binds, the other gets no answer, so `E_f` is re-sent unchanged and the one
/// binding is dropped rather than shipped beside it.
///
/// Control: in `Answers::settle_overloads`, use `.any(..)` instead of
/// `.all(..)` over the edge's sites (`E_f` is retracted and one bound edge
/// sent).
#[test]
fn one_unbound_site_keeps_the_structural_edge_and_drops_every_binding() {
    let scratch = Scratch::new("gm348-b4");
    let fixture = overload_fixture(&scratch, &[OSite::F1, OSite::Fs]);
    let (mut bridge, _) =
        overload_bridge(&scratch, vec![at_site(&scratch, OSite::F1, &[0], None)], NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    kept(&answer, &fixture.e_f, "f(s) did not bind");
}

/// **GM-348 B5 (R3).** A fully bound edge's bindings survive settle even
/// though another structural edge from the same caller (`E_x`, confirmed by
/// an agreeing answer and therefore re-sent) lands on the same declaration:
/// a bound edge says more than an unbound one and is never covered by it.
///
/// Control: in `Answers::settle`'s `covered`, drop the
/// `edge.to_declaration.is_none() &&` conjunct (both bindings are dropped).
#[test]
fn a_binding_survives_a_re_sent_structural_edge_onto_the_same_declaration() {
    let scratch = Scratch::new("gm348-b5");
    let fixture = overload_fixture(&scratch, &[OSite::F1, OSite::Fs, OSite::Xf]);
    let mut xf = at_site(&scratch, OSite::Xf, &[], None);
    xf["definition"] = json!({ "uri": scratch.uri("src/o.toy"), "line": 0, "character": NAME_COL });
    let answers =
        vec![at_site(&scratch, OSite::F1, &[0], None), at_site(&scratch, OSite::Fs, &[1], None), xf];
    let (mut bridge, _) = overload_bridge(&scratch, answers, NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    kept(&answer, &fixture.e_x, "x.f() agrees with E_x");
    assert_eq!(
        bound(&answer),
        vec![(fixture.caller.clone(), 0), (fixture.caller.clone(), 1)],
        "{:#?}",
        answer.diff
    );
    retracted(&answer, &fixture.e_f, "both sites of E_f bound");
}

/// **GM-348 B6.** A location on the implementation of a set with stubs binds
/// nothing: no call binds an implementation, so the server is answering "the
/// function", and the structural edge stands.
///
/// Control: make `choose_overload`'s `bindable` true for every existing
/// ordinal (ordinal 2 is recorded and `E_f` retracted).
#[test]
fn a_location_on_the_implementation_binds_nothing() {
    let scratch = Scratch::new("gm348-b6");
    let fixture = overload_fixture(&scratch, &[OSite::Fs]);
    let (mut bridge, _) =
        overload_bridge(&scratch, vec![at_site(&scratch, OSite::Fs, &[2], None)], NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    kept(&answer, &fixture.e_f, "the implementation is not an overload");
}

/// **GM-348, refining never moves a call.** An answer for an `f` call that
/// lands in another overload set (`m`'s first stub) binds nothing and
/// retracts nothing: refining a call never moves it to another target.
///
/// Control: make `refines` return `true` (the call binds `m`'s ordinal 0 and
/// `E_f` is retracted).
#[test]
fn an_answer_on_another_function_leaves_the_structural_edge_alone() {
    let scratch = Scratch::new("gm348-moves");
    let fixture = overload_fixture(&scratch, &[OSite::F1]);
    let mut f1 = at_site(&scratch, OSite::F1, &[], None);
    f1["definitions"] = json!([{ "uri": scratch.uri("src/o.toy"), "line": 4, "character": NAME_COL }]);
    let (mut bridge, _) = overload_bridge(&scratch, vec![f1], NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    kept(&answer, &fixture.e_f, "the answer named m, not f");
}

/// **GM-348 B8 (method), plus the receiver-call path.** An untyped receiver
/// call onto a method set, answered with the whole set (pyright's shape):
/// the call's hover is the bound stub with `self` dropped and on one line,
/// each candidate's hover has `self` and is wrapped over several lines. It
/// binds ordinal 1 once the first parameter is dropped and whitespace is
/// normalised; the implementation is never hovered.
///
/// Controls: `hover_matches` exact-only (`declared == call`) - unbound, a
/// plain edge with no ordinal; `normalise_hover` returning `text.to_string()`
/// - unbound likewise; delete the `ReceiverCall` prelude in `record_answer` -
///   a three-location answer never agrees, no edge at all.
#[test]
fn a_method_call_binds_by_hover_with_its_receiver_dropped() {
    let scratch = Scratch::new("gm348-b8-method");
    let fixture = overload_fixture(&scratch, &[OSite::M]);
    let answers = vec![
        at_site(&scratch, OSite::M, &[4, 5, 6], Some(&fenced("(method) def m(x: str) -> str"))),
        at_declaration(&scratch, 4, &fenced("(method) def m(\n    self: Self@C,\n    x: int\n) -> int")),
        at_declaration(&scratch, 5, &fenced("(method) def m(\n    self: Self@C,\n    x: str\n) -> str")),
    ];
    let (mut bridge, log) = overload_bridge(&scratch, answers, ByHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(bound(&answer), vec![(fixture.caller.clone(), 1)], "{:#?}", answer.diff);
    assert_eq!(asked(&log, "textDocument/hover"), 3, "the call and two stubs, never the implementation");
}

/// **GM-348, receiver call unbound.** The same call without hover: three
/// declarations and nothing to tell them apart, so the receiver call keeps
/// the plain edge it always had, with no ordinal.
///
/// Control: in `Answers::conclude`, return early for every unbound site, not
/// only an `OverloadCall` (no edge at all).
#[test]
fn an_unbound_receiver_call_keeps_its_plain_edge() {
    let scratch = Scratch::new("gm348-receiver-unbound");
    let fixture = overload_fixture(&scratch, &[OSite::M]);
    let (mut bridge, log) =
        overload_bridge(&scratch, vec![at_site(&scratch, OSite::M, &[4, 5, 6], None)], NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    let edges = semantic_edges(&answer);
    assert_eq!(edges.len(), 1, "{:#?}", answer.diff);
    assert_eq!((edges[0].from_id.as_str(), edges[0].to_declaration), (fixture.caller.as_str(), None));
    assert_eq!(placeholder(&answer, edges[0]).name, "m");
    assert_eq!(asked(&log, "textDocument/hover"), 0);
}

/// **GM-348 B8 (function) and B8d.** Two calls of one set answered with the
/// whole set, told apart by hover: ordinals 0 and 1, `E_f` retracted. The two
/// stubs are hovered once each for the pass, not once per call, and the
/// implementation never: four hovers in all.
///
/// Concurrency 1 on purpose. The cache is filled when a candidate's hover
/// *arrives*, so two definitions in flight at once both miss it and the
/// stubs are hovered twice (6 hovers) - see the controls file.
///
/// Controls: remove the `hovers.contains_key` check in `bind_overload` (6
/// hovers); make `bindable` ignore `has_body` (the implementation is hovered:
/// 5).
#[test]
fn calls_told_apart_by_hover_bind_and_each_stub_is_hovered_once() {
    let scratch = Scratch::new("gm348-b8-function");
    let fixture = overload_fixture(&scratch, &[OSite::F1, OSite::Fs]);
    let answers = vec![
        at_site(&scratch, OSite::F1, &[0, 1, 2], Some(&fenced("(function) def f(x: int) -> int"))),
        at_site(&scratch, OSite::Fs, &[0, 1, 2], Some(&fenced("(function) def f(x: str) -> str"))),
        at_declaration(&scratch, 0, &fenced("(function) def f(x: int) -> int")),
        at_declaration(&scratch, 1, &fenced("(function) def f(x: str) -> str")),
        at_declaration(&scratch, 2, &fenced("(function) def f(x: int | str) -> int | str")),
    ];
    let (mut bridge, log) = overload_bridge(&scratch, answers, ByHover, 1);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(
        bound(&answer),
        vec![(fixture.caller.clone(), 0), (fixture.caller.clone(), 1)],
        "{:#?}",
        answer.diff
    );
    retracted(&answer, &fixture.e_f, "both calls bound by hover");
    assert_eq!(asked(&log, "textDocument/hover"), 4, "two calls, two stubs, once each");
}

/// **GM-348 B8b.** The hover path fails closed: a call hover no stub matches,
/// and one that two stubs match, both leave `E_f` as it was.
///
/// Control: in `Answers::hovered`, bind the first match
/// (`matching.first().copied()`) - the two-match case binds ordinal 0.
#[test]
fn hover_binds_only_on_exactly_one_match() {
    for (case, stub_1, call) in [
        ("no match", "(function) def f(x: str) -> str", "(function) def f(x: bytes) -> bytes"),
        ("two matches", "(function) def f(x: int) -> int", "(function) def f(x: int) -> int"),
    ] {
        let scratch = Scratch::new(&format!("gm348-b8b-{}", case.replace(' ', "-")));
        let fixture = overload_fixture(&scratch, &[OSite::F1]);
        let answers = vec![
            at_site(&scratch, OSite::F1, &[0, 1, 2], Some(&fenced(call))),
            at_declaration(&scratch, 0, &fenced("(function) def f(x: int) -> int")),
            at_declaration(&scratch, 1, &fenced(stub_1)),
        ];
        let (mut bridge, log) = overload_bridge(&scratch, answers, ByHover, 4);

        let answer = pass(&mut bridge, &fixture.index);
        assert!(answer.complete, "{case}: {:?}", answer.reason);
        assert_eq!(asked(&log, "textDocument/hover"), 3, "{case}: the hover path ran");
        assert!(semantic_edges(&answer).is_empty(), "{case}: {:#?}", answer.diff);
        kept(&answer, &fixture.e_f, case);
    }
}

/// **GM-348 B8c.** Hover is opt-in: with the default disambiguation a
/// whole-set answer binds nothing and `textDocument/hover` is never sent,
/// though every position has a hover scripted.
///
/// Control: drop the `disambiguation == Hover` guard in `choose_overload`
/// (hovers are sent and both calls bind).
#[test]
fn without_the_manifest_key_hover_is_never_asked() {
    let scratch = Scratch::new("gm348-b8c");
    let fixture = overload_fixture(&scratch, &[OSite::F1, OSite::Fs]);
    let answers = vec![
        at_site(&scratch, OSite::F1, &[0, 1, 2], Some(&fenced("(function) def f(x: int) -> int"))),
        at_site(&scratch, OSite::Fs, &[0, 1, 2], Some(&fenced("(function) def f(x: str) -> str"))),
        at_declaration(&scratch, 0, &fenced("(function) def f(x: int) -> int")),
        at_declaration(&scratch, 1, &fenced("(function) def f(x: str) -> str")),
    ];
    let (mut bridge, log) = overload_bridge(&scratch, answers, NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(asked(&log, "textDocument/definition"), 2);
    assert_eq!(asked(&log, "textDocument/hover"), 0);
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    kept(&answer, &fixture.e_f, "nothing narrowed the set");
}

/// **GM-348, hover accounting.** A candidate's hover is asked in `o.toy` but
/// belongs to the call in `c.toy`: when it is refused, `c.toy` is the file
/// the pass did not finish, so `E_f` is neither retracted nor re-sent - the
/// file is simply not judged this pass.
///
/// Control: make `Question::accounted_to` return `&self.file` always
/// (`o.toy` is charged instead, `c.toy` counts as finished and `E_f` is
/// re-sent by R1).
#[test]
fn a_refused_candidate_hover_leaves_the_calls_file_unfinished() {
    let scratch = Scratch::new("gm348-accounting");
    let fixture = overload_fixture(&scratch, &[OSite::F1]);
    let mut refused = at_declaration(&scratch, 0, "unused");
    refused["hoverError"] = json!("no hover for you");
    let answers = vec![
        at_site(&scratch, OSite::F1, &[0, 1, 2], Some(&fenced("(function) def f(x: int) -> int"))),
        refused,
        at_declaration(&scratch, 1, &fenced("(function) def f(x: str) -> str")),
    ];
    let (mut bridge, _) = overload_bridge(&scratch, answers, ByHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(!answer.complete, "a refused hover makes the pass incomplete");
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    assert!(upserts(&answer, &fixture.e_f).is_empty(), "c.toy unfinished: E_f is not judged");
    assert!(!answer.diff.delete_edge_ids.contains(&fixture.e_f));
}

/// **GM-348 B10.** The filter: an `OverloadCall` whose structural edge lands
/// on a set-less target, and one without the `replaces` the kind requires,
/// cost the server nothing and leave the pass complete (not unanswerable).
///
/// Control: remove the `!overloaded.targets(..)` `continue` in `questions`
/// (one `definition` is sent).
#[test]
fn a_call_onto_a_function_with_no_overloads_asks_nothing() {
    let scratch = Scratch::new("gm348-b10");
    let fixture = overload_fixture(&scratch, &[OSite::G, OSite::GBare]);
    let mut g = at_site(&scratch, OSite::G, &[], None);
    g["definition"] = json!({ "uri": scratch.uri("src/o.toy"), "line": 3, "character": 3 });
    let (mut bridge, log) = overload_bridge(&scratch, vec![g], NoHover, 4);

    let answer = pass(&mut bridge, &fixture.index);
    assert!(answer.complete, "{:?}", answer.reason);
    assert_eq!(asked(&log, "textDocument/definition"), 0, "nothing was worth asking");
    assert!(semantic_edges(&answer).is_empty(), "{:#?}", answer.diff);
    assert!(!answer.diff.delete_edge_ids.contains(&fixture.e_g));
}

/// **GM-348 B11.** A binding whose call is deleted is retracted by the next
/// pass over that file, while the binding that is still there is not.
///
/// Control: in `Answers::settle_overloads` (or `record`), keep bound edges
/// out of `by_file` - pass 2 does not retract the `f(s)` binding.
#[test]
fn a_deleted_bound_call_is_retracted_by_the_next_pass() {
    let scratch = Scratch::new("gm348-b11");
    let both = overload_fixture(&scratch, &[OSite::F1, OSite::Fs]);
    let one = overload_fixture(&scratch, &[OSite::F1]);
    assert_eq!(both.e_f, one.e_f);
    let answers = vec![at_site(&scratch, OSite::F1, &[0], None), at_site(&scratch, OSite::Fs, &[1], None)];
    let (mut bridge, _) = overload_bridge(&scratch, answers, NoHover, 4);

    let first = pass(&mut bridge, &both.index);
    assert!(first.complete, "{:?}", first.reason);
    let id_of = |answer: &SemanticAnswer, ordinal: u32| {
        semantic_edges(answer)
            .into_iter()
            .find(|edge| edge.to_declaration == Some(ordinal))
            .map(|edge| edge.id.clone())
            .unwrap_or_else(|| panic!("ordinal {ordinal} bound: {:#?}", answer.diff))
    };
    let (kept_id, gone_id) = (id_of(&first, 0), id_of(&first, 1));

    let second = pass(&mut bridge, &one.index);
    assert!(second.complete, "{:?}", second.reason);
    assert_eq!(id_of(&second, 0), kept_id);
    assert!(second.diff.delete_edge_ids.contains(&gone_id), "f(s) is gone: {:#?}", second.diff);
    assert!(!second.diff.delete_edge_ids.contains(&kept_id));
    retracted(&second, &one.e_f, "f(1) still bound");
}

// --- the warm-up ------------------------------------------------------------
//
// `Budgets::warm_up`: a server's first question may take the warm-up budget
// instead of `request`, alone in the pipeline, once per server process. The
// scripted server holds chosen answers (`holdMs`, by arrival) while it goes
// on reading, and writes a `timeline` of what it was asked and answered, so
// how many questions were in flight at once is read from the server's side.
//
// The margins are wide on purpose: a held first answer sits at three times
// `request` and a third of the warm-up.

/// The ordinary budget for every question after the first.
const WARM_REQUEST: Duration = Duration::from_millis(500);
/// The warm-up budget for the first.
const WARM_UP: Duration = Duration::from_secs(5);
/// How long the scripted server holds a cold first answer: past
/// `WARM_REQUEST`, inside `WARM_UP`.
const COLD_MS: u64 = 1_500;

fn warm_budgets(request: Duration, warm_up: Option<Duration>) -> Budgets {
    Budgets { request, warm_up, ..budgets() }
}

/// The most questions the server had outstanding at once, per interval
/// between its answers: `[0]` before its first answer, `[1]` between its
/// first and second, and so on. A question it never answered stays
/// outstanding.
fn in_flight_between_answers(timeline: &Path) -> Vec<usize> {
    let text = std::fs::read_to_string(timeline).unwrap_or_default();
    let mut outstanding = 0usize;
    let mut most = 0usize;
    let mut intervals = Vec::new();
    for event in text.lines() {
        if event.starts_with("asked ") {
            outstanding += 1;
            most = most.max(outstanding);
        } else if event.starts_with("answered ") {
            intervals.push(most);
            outstanding = outstanding.saturating_sub(1);
            most = outstanding;
        }
    }
    intervals.push(most);
    intervals
}

/// How many definition/implementation questions reached the server.
fn arrived(timeline: &Path) -> usize {
    std::fs::read_to_string(timeline)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("asked "))
        .count()
}

/// The script every warm-up test runs: no progress, UTF-16, `answers`, the
/// given holds and refusals, and a timeline.
fn held_server(
    scratch: &Scratch,
    answers: Value,
    hold_ms: &[u64],
    refuse: &[u32],
    extra: Value,
) -> (SemanticConfig, PathBuf) {
    let timeline = scratch.path().join("timeline.log");
    let mut script = json!({
        "readiness": { "kind": "none" },
        "positionEncoding": "utf-16",
        "answers": answers,
        "holdMs": hold_ms,
        "refuse": refuse,
        "timeline": timeline.to_string_lossy(),
    });
    if let Value::Object(more) = extra {
        script.as_object_mut().expect("an object").extend(more);
    }
    (scratch.server(script), timeline)
}

/// The fixture with a second site in `b.toy` the script does not answer,
/// asked after the first.
fn fixture_with_a_second_site(scratch: &Scratch) -> SdkIndex {
    let (mut index, caller) = fixture(scratch);
    let b = RelPath::new("src/b.toy");
    let (source, mut graph) = {
        let entry = index.entry(&b).expect("the fixture has it");
        (entry.source.clone(), entry.graph.clone())
    };
    graph.open_sites.push(OpenSite {
        from_id: caller,
        position: Position { line: 1, col: 12 },
        name: "add".to_string(),
        kind: OpenSiteKind::ReceiverCall,
        edge_kind: EdgeKind::Calls,
        from_container: Some("pkg".to_string()),
        replaces: None,
    });
    index.insert(b, source, graph);
    index
}

/// A first answer that takes longer than `request` but less than the
/// warm-up is an answer: the pass is complete and its edge emitted.
///
/// Control: `let request_budget = budgets.request;` in `run_pass`; the first
/// question times out at `request`. Reverting the expiry filter alone is not
/// a control here: `run_pass` runs that filter only when a poll comes back
/// idle, and while the warm-up question is in flight that happens at its
/// warm-up or at the pass deadline, never in between - a notification is
/// `Poll::Noise` and goes straight back to the poll. The filter alone is
/// pinned at the deadline, by `the_warm_up_sits_inside_the_pass_budget`.
#[test]
fn a_servers_first_question_may_take_its_warm_up_budget() {
    let scratch = Scratch::new("warm-up-first");
    let (index, _) = fixture(&scratch);
    let (config, _) = held_server(&scratch, answers_the_site(&scratch), &[COLD_MS], &[], json!({}));
    let mut bridge =
        LspBridge::with_budgets("toy", scratch.path(), config, warm_budgets(WARM_REQUEST, Some(WARM_UP)));

    let answer = pass_over(&mut bridge, &index, &["src/b.toy"]);
    assert!(answer.complete, "the held first answer arrived inside the warm-up: {:?}", answer.reason);
    assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);
}

/// While the warm-up is owed one question is in flight; once the server has
/// answered, the pipeline fills to `concurrency`. Counted by the server.
///
/// Control: `let width = budgets.concurrency.max(1);` in `run_pass`; four
/// questions are outstanding before the first answer.
#[test]
fn while_the_warm_up_is_owed_one_question_is_in_flight_and_then_the_pipeline_fills() {
    let scratch = Scratch::new("warm-up-width");
    let mut index = SdkIndex::new();
    crowd_file(&scratch, &mut index, "src/c.toy", 8);
    let (config, timeline) =
        held_server(&scratch, json!([]), &[600, 300, 300, 300, 300, 300, 300, 300], &[], json!({}));
    let budgets = warm_budgets(Duration::from_secs(2), Some(Duration::from_secs(6)));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "{:?}", answer.reason);
    let in_flight = in_flight_between_answers(&timeline);
    assert_eq!(in_flight[0], 1, "one question while warming: {in_flight:?}");
    assert!(in_flight[1] > 1, "the pipeline fills once the server has answered: {in_flight:?}");
}

/// Only the first question gets the warm-up: a later one the server holds
/// past `request` times out under `request`.
///
/// Control: `let request_budget = warm_up.unwrap_or(budgets.request);` in
/// `run_pass`; the second question is answered and the pass is complete.
#[test]
fn questions_after_the_first_get_the_ordinary_request_budget() {
    let scratch = Scratch::new("warm-up-rest");
    let mut index = SdkIndex::new();
    crowd_file(&scratch, &mut index, "src/c.toy", 2);
    let (config, _) = held_server(&scratch, json!([]), &[COLD_MS, COLD_MS], &[], json!({}));
    let mut bridge =
        LspBridge::with_budgets("toy", scratch.path(), config, warm_budgets(WARM_REQUEST, Some(WARM_UP)));

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "the second question had only `request`");
    assert!(
        reason(&answer).contains("did not answer a question about src/c.toy within 500ms"),
        "{}",
        reason(&answer)
    );
}

/// The warm-up is spent once per server process: a second pass on the same
/// running server gives its first question only `request`.
///
/// Control: remove `client.mark_warmed_up()` from `run_pass`'s `Answered`
/// branch; the second pass waits out the warm-up again and is complete.
#[test]
fn a_warm_up_is_spent_once_per_server_and_not_once_per_pass() {
    let scratch = Scratch::new("warm-up-once");
    let (index, _) = fixture(&scratch);
    let (config, _) = held_server(&scratch, answers_the_site(&scratch), &[COLD_MS, COLD_MS], &[], json!({}));
    let mut bridge =
        LspBridge::with_budgets("toy", scratch.path(), config, warm_budgets(WARM_REQUEST, Some(WARM_UP)));

    let first = pass(&mut bridge, &index);
    assert!(first.complete, "the first pass had the warm-up: {:?}", first.reason);
    let second = pass(&mut bridge, &index);
    assert!(!second.complete, "the same server owes no second warm-up");
    assert!(reason(&second).contains("within 500ms"), "{}", reason(&second));
}

/// A restarted server is cold again and owes a new warm-up. The server
/// crashes after its first answer, so each pass meets a new one.
///
/// Control: keep the latch in a process-wide static instead of on
/// `LspClient`; the second server's first question times out at `request`
/// and the second pass emits no edge.
#[test]
fn a_restarted_server_owes_a_new_warm_up() {
    let scratch = Scratch::new("warm-up-restart");
    let index = fixture_with_a_second_site(&scratch);
    // `request` is long enough for the crashed server's exit to reach the
    // bridge while its second question waits, so the next pass starts anew.
    let (config, _) =
        held_server(&scratch, answers_the_site(&scratch), &[3_000], &[], json!({ "crashAfterRequests": 1 }));
    let budgets = warm_budgets(Duration::from_secs(1), Some(Duration::from_secs(8)));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let first = pass(&mut bridge, &index);
    assert_eq!(
        semantic_edges(&first).len(),
        1,
        "the first server answered under its warm-up: {:#?}",
        first.diff
    );

    let second = pass(&mut bridge, &index);
    assert_eq!(
        semantic_edges(&second).len(),
        1,
        "the new server's first question had a warm-up of its own: {:?}",
        second.reason
    );
}

/// A warm-up that times out fails its question as `request` would, and is
/// spent: the rest go out together under `request`, so all six are asked
/// well inside a pass budget that six warm-ups in a row would overrun.
///
/// Control: remove the `client.mark_warmed_up()` under
/// `if !expired.is_empty()` in `run_pass`; each question is sent alone under
/// the warm-up and the pass budget ends before all six are asked.
#[test]
fn a_warm_up_that_times_out_fails_its_question_and_is_spent() {
    let scratch = Scratch::new("warm-up-timeout");
    let mut index = SdkIndex::new();
    crowd_file(&scratch, &mut index, "src/c.toy", 6);
    // Never answered within the test.
    let (config, timeline) = held_server(&scratch, json!([]), &[60_000; 6], &[], json!({}));
    let mut budgets = warm_budgets(Duration::from_millis(300), Some(Duration::from_secs(1)));
    budgets.project_floor = Duration::from_secs(4);
    budgets.per_file = Duration::from_millis(1);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);
    // Started under its own budget, so the pass budget is spent on asking.
    bridge.prepare();

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "an unanswered warm-up is not an answer of 'nothing'");
    assert!(
        reason(&answer).contains("did not answer a question about src/c.toy within 1s"),
        "the warm-up's own timeout is the reason: {}",
        reason(&answer)
    );
    assert!(answer.diff.upsert_edges.is_empty());
    assert_eq!(arrived(&timeline), 6, "every question was asked after the warm-up was spent");
}

/// A refusal of the first question spends the warm-up too: the server is
/// answering, so the pipeline fills right after it.
///
/// Control: remove `client.mark_warmed_up()` from `run_pass`'s `Failed`
/// branch; the second question still goes out alone.
#[test]
fn a_refused_first_question_spends_the_warm_up() {
    let scratch = Scratch::new("warm-up-refused");
    let mut index = SdkIndex::new();
    crowd_file(&scratch, &mut index, "src/c.toy", 6);
    let (config, timeline) = held_server(&scratch, json!([]), &[0, 300, 300, 300, 300, 300], &[1], json!({}));
    let budgets = warm_budgets(Duration::from_secs(2), Some(Duration::from_secs(6)));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "a refused question is not an answer");
    assert!(reason(&answer).contains("refused by the script"), "{}", reason(&answer));
    let in_flight = in_flight_between_answers(&timeline);
    assert_eq!(in_flight[0], 1, "{in_flight:?}");
    assert!(in_flight[1] > 1, "the pipeline fills right after the refusal: {in_flight:?}");
}

/// The warm-up is off unless a plugin asks for it: the first question gets
/// `request` and the pipeline fills to `concurrency` at once.
///
/// Control: `warm_up: Some(..)` in `Budgets::default()`; one question goes
/// out alone and is answered. Control: `let width = if warming ||
/// warm_up.is_none() { 1 } else { .. };` in `run_pass`; the questions go out
/// one at a time and at most two are outstanding before the first answer.
#[test]
fn without_a_warm_up_the_first_question_gets_the_request_budget_and_the_pipeline_fills_at_once() {
    assert_eq!(Budgets::default().warm_up, None);

    let scratch = Scratch::new("warm-up-off");
    let mut index = SdkIndex::new();
    crowd_file(&scratch, &mut index, "src/c.toy", 6);
    let (config, timeline) =
        held_server(&scratch, json!([]), &[COLD_MS, 300, 300, 300, 300, 300], &[], json!({}));
    let defaults = budgets();
    let budgets = Budgets {
        request: WARM_REQUEST,
        concurrency: 4,
        project_floor: defaults.project_floor,
        per_file: defaults.per_file,
        single_file: defaults.single_file,
        readiness: defaults.readiness,
        settle: defaults.settle,
        ..Budgets::default()
    };
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(!answer.complete, "the held first answer had only `request`");
    assert!(reason(&answer).contains("within 500ms"), "{}", reason(&answer));
    // Every question asked before the server's first answer: `concurrency`
    // of them, at once. A bridge that sent one question at a time would have
    // at most two outstanding here - the held first, which the server goes
    // on holding after the bridge gives up on it, and the one after it.
    let in_flight = in_flight_between_answers(&timeline);
    assert!(in_flight[0] >= 4, "the pipeline fills to `concurrency` before any answer: {in_flight:?}");
}

/// How many questions reached the server before it answered its `nth`
/// (1-based, by arrival) - the `nth` itself included. A question never
/// answered counts every arrival.
fn asked_before_answer(timeline: &Path, nth: usize) -> usize {
    let answer = format!("answered {nth}");
    std::fs::read_to_string(timeline)
        .unwrap_or_default()
        .lines()
        .take_while(|line| *line != answer)
        .filter(|line| line.starts_with("asked "))
        .count()
}

/// With no warm-up configured, the first question is not sent alone: while
/// the server holds its first answer - well inside `request`, so the bridge
/// is still waiting for it rather than giving up on it - the rest of the
/// pipeline is already asked. Counted by the server: arrivals before its
/// first answer is written.
///
/// Control: `let warming = !client.warmed_up();` in `run_pass` (the
/// `warm_up.is_some()` condition dropped); the first question goes out alone
/// and is answered before any other arrives. The test above misses that
/// mutant: its held first question times out at `request`, which spends the
/// warm-up, and the pipeline fills before the server's first answer anyway.
#[test]
fn without_a_warm_up_the_first_question_is_not_asked_alone() {
    let scratch = Scratch::new("warm-up-off-held");
    let mut index = SdkIndex::new();
    crowd_file(&scratch, &mut index, "src/c.toy", 6);
    // The first answer is held for 1.5s of a 5s `request`; the rest at once.
    let (config, timeline) = held_server(&scratch, json!([]), &[COLD_MS], &[], json!({}));
    let budgets = budgets();
    assert_eq!(budgets.warm_up, None);
    assert!(Duration::from_millis(COLD_MS) * 3 <= budgets.request);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "the held first answer came inside `request`: {:?}", answer.reason);
    let before_first = asked_before_answer(&timeline, 1);
    assert!(
        before_first > 1,
        "more than one question was outstanding while the first was held: {} arrivals before `answered 1`\n{}",
        before_first,
        std::fs::read_to_string(&timeline).unwrap_or_default()
    );
}

/// A warm-up shorter than `request` never shortens the first question's
/// budget.
///
/// Control: drop `.max(budgets.request)` from `run_pass`'s `warm_up`; the
/// first question times out at the warm-up.
#[test]
fn a_warm_up_shorter_than_request_never_shortens_the_first_question() {
    let scratch = Scratch::new("warm-up-short");
    let (index, _) = fixture(&scratch);
    let (config, _) = held_server(&scratch, answers_the_site(&scratch), &[1_000], &[], json!({}));
    let budgets = warm_budgets(Duration::from_secs(3), Some(Duration::from_millis(300)));
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);

    let answer = pass(&mut bridge, &index);
    assert!(answer.complete, "the first question had `request`: {:?}", answer.reason);
    assert_eq!(semantic_edges(&answer).len(), 1, "{:#?}", answer.diff);
}

/// The warm-up sits inside the pass budget: a first question still
/// unanswered when the pass runs out ends the pass then, not at the end of
/// the warm-up.
///
/// Control: `let wake = oldest + request_budget;` in `run_pass` (no
/// `.min(deadline)`); the pass waits out the warm-up. Control: use
/// `budgets.request` instead of `request_budget` in `run_pass`'s expiry
/// filter; the idle poll at the deadline retires the first question at
/// `request`, and the pass reports that instead of running out of its budget.
#[test]
fn the_warm_up_sits_inside_the_pass_budget() {
    let scratch = Scratch::new("warm-up-deadline");
    let (index, _) = fixture(&scratch);
    let (config, _) = held_server(&scratch, answers_the_site(&scratch), &[60_000], &[], json!({}));
    let mut budgets = warm_budgets(Duration::from_millis(300), Some(Duration::from_secs(8)));
    budgets.project_floor = Duration::from_millis(2_500);
    budgets.per_file = Duration::from_millis(1);
    let mut bridge = LspBridge::with_budgets("toy", scratch.path(), config, budgets);
    // Started under its own budget, so the pass budget is spent on asking.
    bridge.prepare();

    let started = std::time::Instant::now();
    let answer = pass(&mut bridge, &index);
    let took = started.elapsed();
    assert!(!answer.complete);
    assert!(reason(&answer).contains("ran out of its budget"), "{}", reason(&answer));
    assert!(took < Duration::from_secs(6), "the pass budget ended the wait, not the warm-up: {took:?}");
}
