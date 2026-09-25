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
    }
}

/// The two-file index this module's doc describes: `a.toy` declares `add`,
/// `b.toy` calls it through a receiver it cannot resolve.
fn fixture(scratch: &Scratch) -> (SdkIndex, String) {
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
    builder.open_site(OpenSite {
        from_id: caller.clone(),
        position: Position { line: 1, col: SITE_CHAR_COL },
        name: "add".to_string(),
        kind: OpenSiteKind::ReceiverCall,
        edge_kind: EdgeKind::Calls,
        from_container: Some("pkg".to_string()),
        replaces: None,
    });
    index.insert(b, B_TOY.to_string(), builder.finish());

    (index, caller)
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
    assert_eq!(semantic_edges(&answer).len(), 1, "the one answer that arrived is kept: {:#?}", answer.diff);

    // The process is still standing and still usable: a second pass starts a
    // new server, which crashes again after its own first answer - proving the
    // bridge tried rather than giving up.
    let second = pass(&mut bridge, &index);
    assert_eq!(semantic_edges(&second).len(), 1, "a fresh server answered again: {:#?}", second.diff);
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
