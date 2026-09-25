//! A language server that does exactly what a test script tells it to.
//!
//! # Why a real process and not a mock
//!
//! Everything the bridge can get wrong lives *between* two processes: framing,
//! request/response correlation, a server that answers out of order, one that
//! never answers, one that dies with questions outstanding, one that counts
//! columns in a different unit than the client assumed. A mock
//! `SemanticEngine`, or a fake `LspClient`, tests the code that is easy to get
//! right and skips the code that is not - and the crash case cannot be faked
//! at all: "the plugin survives its server dying" is a statement about a
//! process.
//!
//! So this is a real binary, spawned by the real client over real pipes, and
//! its whole behaviour is one JSON file the test writes:
//!
//! ```json
//! {
//!   "readiness": { "kind": "progress", "beginAfterMs": 0, "endAfterMs": 40 },
//!   "//": "or a sequence, which is what a real server's startup looks like:",
//!   "//readiness": { "phases": [{ "token": "fetch", "holdMs": 40 },
//!                               { "token": "index", "beginAfterMs": 20, "holdMs": 40 }] },
//!   "positionEncoding": "utf-16",
//!   "answers": [
//!     { "uri": "…/b.toy", "line": 1, "character": 3,
//!       "definition": { "uri": "…/a.toy", "line": 0, "character": 3 } }
//!   ],
//!   "delayMs": 0,
//!   "crashAfterRequests": 2,
//!   "silentFrom": 3
//! }
//! ```
//!
//! Every field is optional. `answers` is matched on the *request's* position,
//! so a script can make one position answer and another answer nothing, which
//! is what "an empty answer is not an error" needs to be tested with.
//!
//! # What it deliberately does not do
//!
//! Anything a real server does: parse, resolve, or know a language. It is a
//! programmable mouth for the protocol, and the moment it grows an opinion
//! about source code it stops being a test fixture and starts being a second
//! implementation of the thing under test.

use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

/// One scripted answer, matched on the position the request asked about.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScriptedAnswer {
    /// The document the question is asked in.
    uri: String,
    line: u32,
    character: u32,
    /// What `textDocument/definition` answers there - omitted means `null`,
    /// the ordinary "nothing here".
    #[serde(default)]
    definition: Option<Location>,
    /// What `textDocument/implementation` answers there.
    #[serde(default)]
    implementation: Vec<Location>,
    /// Answer this one with a JSON-RPC error instead.
    #[serde(default)]
    error: Option<String>,
    /// Never answer this one at all - what a per-request timeout is measured
    /// against.
    #[serde(default)]
    silent: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    uri: String,
    line: u32,
    character: u32,
}

impl Location {
    fn to_json(&self) -> Value {
        json!({
            "uri": self.uri,
            "range": {
                "start": { "line": self.line, "character": self.character },
                "end": { "line": self.line, "character": self.character + 1 },
            },
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Readiness {
    /// `"progress"` (the default when this table is present), `"none"` - never
    /// report progress at all - or `"never"`, which begins one and never ends
    /// it.
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    begin_after_ms: u64,
    #[serde(default)]
    end_after_ms: u64,
    /// A *sequence* of work-done tokens, each begun after the previous one
    /// ended - what a real rust-analyzer's startup looks like (`Fetching`,
    /// then `Building CrateGraph`, then `Roots Scanned`, …) and the shape
    /// that makes "no progress is in flight" a false reading of "the server
    /// has finished". When this is non-empty it replaces the single token
    /// above, and the server counts itself indexing until the last phase
    /// ends - so the gaps *between* phases are answerable-looking moments
    /// where the honest answer is still nothing.
    #[serde(default)]
    phases: Vec<Phase>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Phase {
    /// The token, so that a client tracking them by name sees genuinely
    /// different ones rather than one token restarted.
    token: String,
    /// How long after the previous phase ended this one begins - the gap.
    #[serde(default)]
    begin_after_ms: u64,
    /// How long this phase runs before it ends.
    #[serde(default)]
    hold_ms: u64,
}

/// A second burst of indexing, begun while the client is mid-pass - what a
/// real server does after a `didChange`, and the reason readiness is not only
/// a startup condition.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reindex {
    /// Begin the progress just before answering this request (1-based).
    at_request: u32,
    /// End it this long afterwards.
    hold_ms: u64,
}

/// The GM-309 shape, and the reason it is a separate table from [`Reindex`]:
/// that one begins its progress *deterministically*, tied to a request count,
/// so a test built on it can never land in the gap this one exists to reach.
///
/// A real server does not begin reporting progress for an edit the instant it
/// receives `didChange` - it notices the edit, decides to re-analyse, and
/// only then opens a work-done token. Measured for pyright
/// (`docs/architecture/multi-language-plugins.md`, "Readiness, measured, and
/// deliberately not changed"): the token arrives ~0.63s *after* `didOpen`.
/// This is the same shape after `didChange`, scripted with real milliseconds
/// rather than a request count, so a client that asks in the meantime is
/// genuinely racing the server's own recognition of the edit - not a fixture
/// rigged to make one side win.
///
/// Until the cycle this starts has *ended*, every question is answered
/// `null` regardless of `answers` - not only while the progress is in
/// flight, but in the gap beforehand too, where a server that has not yet
/// noticed the edit has nothing truthful to say about it either. That is
/// the whole point: the empty answer a client gets by asking too early is a
/// real "I don't know yet", and the question this fixture exists to force is
/// whether the bridge believes it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReindexOnChange {
    /// How long after `didChange` arrives before the server begins telling
    /// anyone about it.
    begin_after_ms: u64,
    /// How long the progress runs once begun.
    hold_ms: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Script {
    #[serde(default)]
    readiness: Option<Readiness>,
    #[serde(default)]
    reindex: Option<Reindex>,
    #[serde(default)]
    reindex_on_change: Option<ReindexOnChange>,
    /// Answer `null` to everything while a progress is in flight - a server
    /// that has not finished loading and says so only through `$/progress`.
    /// This is the behaviour the readiness gate exists for: without the gate,
    /// every one of those nulls is recorded as "there is no such symbol".
    #[serde(default)]
    null_while_indexing: bool,
    /// What the server tells the client it counts columns in. Absent means the
    /// server says nothing, which the specification reads as UTF-16.
    #[serde(default)]
    position_encoding: Option<String>,
    #[serde(default)]
    answers: Vec<ScriptedAnswer>,
    /// Sleep before answering every request.
    #[serde(default)]
    delay_ms: u64,
    /// Exit abruptly after answering this many definition/implementation
    /// requests - a crash mid-pass.
    #[serde(default)]
    crash_after_requests: Option<u32>,
    /// With `crashAfterRequests`: close stdin before the last answer goes
    /// out, so the client, having read that answer, finds its next question
    /// meets a broken pipe rather than a closed stdout.
    #[serde(default)]
    close_input_before_crash: bool,
    /// Stop answering (without exiting) from the nth request on - a server
    /// that hangs rather than dies.
    #[serde(default)]
    silent_from: Option<u32>,
    /// Close stdin right after `initialized` and stay alive with stdout open:
    /// a server the client can no longer write to, but which has not exited,
    /// so the next question fails to *send* rather than going unanswered.
    #[serde(default)]
    close_input_after_initialized: bool,
    /// Where to append a line per request received, for a test that wants to
    /// count what was asked.
    #[serde(default)]
    log: Option<String>,
    /// Sections to ask the *client* for with `workspace/configuration`, right
    /// after `initialized` - which is when pyright asks, and the only channel
    /// it takes settings through at all (GM-299). A server asking its client
    /// something is the one direction the rest of this script cannot
    /// exercise.
    #[serde(default)]
    ask_configuration: Vec<String>,
    /// Where to write the client's answer to that request, verbatim, so a
    /// test can assert what the client actually sent rather than what it
    /// meant to.
    #[serde(default)]
    configuration_out: Option<String>,
}

/// The id this server uses for its own `workspace/configuration` request.
/// Far away from the client's own ids, which start at 1, so a frame carrying
/// it cannot be mistaken for anything else.
const CONFIGURATION_REQUEST_ID: i64 = 9001;

fn main() {
    let script = read_script();
    let mut stdout = std::io::stdout();
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());

    let mut asked = 0u32;
    // Whether a `$/progress` this server began is still in flight. Shared with
    // the threads that end one, and read by every answer when
    // `nullWhileIndexing` is set.
    let indexing = Arc::new(AtomicBool::new(false));
    // Whether a `didChange` has started a `reindexOnChange` cycle at all -
    // see [`ReindexOnChange`]. Without this, a script carrying the table
    // would null every answer from the first request on, including the ones
    // asked before any edit exists to be racing.
    let changed = Arc::new(AtomicBool::new(false));
    // Whether that cycle has completed. `false` covers both the gap before
    // its progress begins and the progress itself, which is the one thing
    // `indexing` alone cannot say: `indexing` is false in that gap too, and a
    // script that only checked it would answer truthfully before the server
    // has any right to.
    let revealed = Arc::new(AtomicBool::new(false));

    while let Some(message) = read_frame(&mut reader) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        log(&script, &method);

        match method.as_str() {
            "initialize" => {
                let mut capabilities = json!({
                    "definitionProvider": true,
                    "implementationProvider": true,
                    "textDocumentSync": 1,
                });
                if let Some(encoding) = &script.position_encoding {
                    capabilities["positionEncoding"] = json!(encoding);
                }
                respond(&mut stdout, id, json!({ "capabilities": capabilities }));
            }
            "initialized" => {
                start_progress(&script, Arc::clone(&indexing));
                if !script.ask_configuration.is_empty() {
                    let items: Vec<Value> = script
                        .ask_configuration
                        .iter()
                        .map(|section| json!({ "scopeUri": "file:///p", "section": section }))
                        .collect();
                    write_frame(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": CONFIGURATION_REQUEST_ID,
                            "method": "workspace/configuration",
                            "params": { "items": items },
                        }),
                    );
                }
                if script.close_input_after_initialized {
                    close_stdin();
                    loop {
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
            "shutdown" => respond(&mut stdout, id, Value::Null),
            "exit" => return,
            "textDocument/definition" | "textDocument/implementation" => {
                asked += 1;
                if script.silent_from.is_some_and(|from| asked >= from) {
                    continue;
                }
                if script.delay_ms > 0 {
                    std::thread::sleep(Duration::from_millis(script.delay_ms));
                }
                // A reindex begins *before* the answer goes out, so the client
                // is guaranteed to have seen the `begin` by the time it reads
                // the empty answer - which is what makes "an empty answer while
                // the server is busy" a deterministic test rather than a race.
                if script.reindex.as_ref().is_some_and(|reindex| reindex.at_request == asked) {
                    begin_reindex(&script, Arc::clone(&indexing));
                }
                let crashing = script.crash_after_requests.is_some_and(|after| asked >= after);
                if crashing && script.close_input_before_crash {
                    close_stdin();
                }
                let key = position_of(&params);
                let answer = script.answers.iter().find(|answer| {
                    (answer.uri.as_str(), answer.line, answer.character) == (key.0.as_str(), key.1, key.2)
                });
                match answer {
                    Some(answer) if answer.silent => continue,
                    Some(answer) if answer.error.is_some() => {
                        error_response(&mut stdout, id, answer.error.clone().unwrap_or_default())
                    }
                    Some(_) if script.null_while_indexing && indexing.load(Ordering::SeqCst) => {
                        respond(&mut stdout, id, Value::Null)
                    }
                    // GM-309's window: a `didChange` has started a
                    // `reindexOnChange` cycle and it has not yet ended,
                    // whether or not its progress has even begun - see
                    // [`ReindexOnChange`].
                    Some(_) if changed.load(Ordering::SeqCst) && !revealed.load(Ordering::SeqCst) => {
                        respond(&mut stdout, id, Value::Null)
                    }
                    Some(answer) if method.ends_with("definition") => {
                        let result = answer.definition.as_ref().map(Location::to_json).unwrap_or(Value::Null);
                        respond(&mut stdout, id, result)
                    }
                    Some(answer) => {
                        let result: Vec<Value> =
                            answer.implementation.iter().map(Location::to_json).collect();
                        respond(&mut stdout, id, Value::Array(result))
                    }
                    None => respond(&mut stdout, id, Value::Null),
                }
                if crashing {
                    // Not an `exit`: the point is a server that goes away
                    // without saying anything, which is what a crash is.
                    std::process::exit(101);
                }
            }
            // The client's answer to *our* `workspace/configuration`: a frame
            // with an id and no method. It must be recognised before the
            // catch-all below, which would otherwise "answer" a response and
            // leave the client correlating a reply to nothing.
            "" if id.as_ref().and_then(Value::as_i64) == Some(CONFIGURATION_REQUEST_ID) => {
                if let Some(path) = &script.configuration_out {
                    let answer = message.get("result").cloned().unwrap_or(Value::Null);
                    let _ = std::fs::write(path, serde_json::to_string(&answer).unwrap_or_default());
                }
            }
            // A `didChange` starts the GM-309 clock, if one is scripted - see
            // [`ReindexOnChange`]. Every `didChange` restarts it, which is
            // fine for the one fixture that uses this: it edits one file once.
            "textDocument/didChange" if script.reindex_on_change.is_some() => {
                changed.store(true, Ordering::SeqCst);
                begin_reindex_on_change(&script, Arc::clone(&indexing), Arc::clone(&revealed));
            }
            // Everything else - `didOpen`, `didChange` with nothing scripted,
            // `$/cancelRequest` - is accepted and ignored. A request this
            // fixture does not know still gets an answer, because a client
            // left waiting on one would hang for a reason that has nothing to
            // do with the test.
            _ => {
                if id.is_some() && !method.is_empty() {
                    respond(&mut stdout, id, Value::Null);
                }
            }
        }
    }
}

/// The script named by `--script <path>`, or an empty one.
fn read_script() -> Script {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--script" {
            let Some(path) = args.next() else { break };
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("fake-lsp: cannot read {path}: {err}"));
            return serde_json::from_str(&contents)
                .unwrap_or_else(|err| panic!("fake-lsp: {path} is not a script: {err}"));
        }
    }
    Script::default()
}

fn position_of(params: &Value) -> (String, u32, u32) {
    let uri = params
        .get("textDocument")
        .and_then(|document| document.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let position = params.get("position");
    let line = position.and_then(|at| at.get("line")).and_then(Value::as_u64).unwrap_or(0) as u32;
    let character = position.and_then(|at| at.get("character")).and_then(Value::as_u64).unwrap_or(0) as u32;
    (uri, line, character)
}

/// Sends the `$/progress` the bridge's readiness gate waits for, on its own
/// thread so that `initialized` returns immediately - which is what a real
/// server does, and what makes "the client asked before the server was ready"
/// reachable at all.
fn start_progress(script: &Script, indexing: Arc<AtomicBool>) {
    let Some(readiness) = script.readiness.clone() else { return };
    let kind = readiness.kind.clone().unwrap_or_else(|| "progress".to_string());
    if kind == "none" {
        return;
    }
    indexing.store(true, Ordering::SeqCst);
    if !readiness.phases.is_empty() {
        std::thread::spawn(move || {
            let mut stdout = std::io::stdout();
            for phase in &readiness.phases {
                std::thread::sleep(Duration::from_millis(phase.begin_after_ms));
                notify(
                    &mut stdout,
                    "$/progress",
                    json!({ "token": phase.token, "value": { "kind": "begin", "title": phase.token } }),
                );
                std::thread::sleep(Duration::from_millis(phase.hold_ms));
                notify(
                    &mut stdout,
                    "$/progress",
                    json!({ "token": phase.token, "value": { "kind": "end" } }),
                );
            }
            // Only now: the gaps between phases are not readiness, which is
            // the whole point of this shape.
            indexing.store(false, Ordering::SeqCst);
        });
        return;
    }
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        std::thread::sleep(Duration::from_millis(readiness.begin_after_ms));
        notify(
            &mut stdout,
            "$/progress",
            json!({ "token": "indexing", "value": { "kind": "begin", "title": "indexing" } }),
        );
        if kind == "never" {
            return;
        }
        std::thread::sleep(Duration::from_millis(readiness.end_after_ms));
        indexing.store(false, Ordering::SeqCst);
        notify(&mut stdout, "$/progress", json!({ "token": "indexing", "value": { "kind": "end" } }));
    });
}

/// Begins a second progress and ends it after `hold_ms` - see [`Reindex`].
/// The `begin` is written from this thread, before the caller writes the
/// answer it is about to send; the `end` is written from a spawned one.
fn begin_reindex(script: &Script, indexing: Arc<AtomicBool>) {
    let Some(reindex) = script.reindex.clone() else { return };
    indexing.store(true, Ordering::SeqCst);
    notify(
        &mut std::io::stdout(),
        "$/progress",
        json!({ "token": "reindex", "value": { "kind": "begin", "title": "reindexing" } }),
    );
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(reindex.hold_ms));
        indexing.store(false, Ordering::SeqCst);
        notify(
            &mut std::io::stdout(),
            "$/progress",
            json!({ "token": "reindex", "value": { "kind": "end" } }),
        );
    });
}

/// Begins the GM-309 clock a `didChange` starts - see [`ReindexOnChange`].
/// Unlike [`begin_reindex`], the `begin` is written from a spawned thread
/// after a real sleep, not synchronously before an answer: this is what makes
/// the gap between the edit and the server's own recognition of it a real
/// span of wall-clock time rather than something the fixture can order away.
fn begin_reindex_on_change(script: &Script, indexing: Arc<AtomicBool>, revealed: Arc<AtomicBool>) {
    let Some(cfg) = script.reindex_on_change.clone() else { return };
    std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        std::thread::sleep(Duration::from_millis(cfg.begin_after_ms));
        indexing.store(true, Ordering::SeqCst);
        notify(
            &mut stdout,
            "$/progress",
            json!({ "token": "reindex-on-change", "value": { "kind": "begin", "title": "reindexing" } }),
        );
        std::thread::sleep(Duration::from_millis(cfg.hold_ms));
        indexing.store(false, Ordering::SeqCst);
        revealed.store(true, Ordering::SeqCst);
        notify(
            &mut stdout,
            "$/progress",
            json!({ "token": "reindex-on-change", "value": { "kind": "end" } }),
        );
    });
}

/// Closes this process's end of its stdin pipe, so the client's next write
/// fails. Nothing reads stdin afterwards.
#[cfg(unix)]
fn close_stdin() {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SAFETY: fd 0 is owned by this process and nothing reads it again.
    drop(unsafe { OwnedFd::from_raw_fd(std::io::stdin().as_raw_fd()) });
}

/// Closes this process's end of its stdin pipe, so the client's next write
/// fails. Nothing reads stdin afterwards.
#[cfg(windows)]
fn close_stdin() {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    // SAFETY: the stdin handle is owned by this process and nothing reads it
    // again.
    drop(unsafe { OwnedHandle::from_raw_handle(std::io::stdin().as_raw_handle()) });
}

fn log(script: &Script, method: &str) {
    let Some(path) = &script.log else { return };
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{method}");
    }
}

fn respond<W: Write>(out: &mut W, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    write_frame(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn error_response<W: Write>(out: &mut W, id: Option<Value>, message: String) {
    let Some(id) = id else { return };
    write_frame(out, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": message } }));
}

fn notify<W: Write>(out: &mut W, method: &str, params: Value) {
    write_frame(out, &json!({ "jsonrpc": "2.0", "method": method, "params": params }));
}

/// The base protocol's framing, written out here rather than imported from the
/// SDK on purpose: a fixture that shares an implementation with the thing it
/// tests cannot catch that implementation being wrong.
fn write_frame<W: Write>(out: &mut W, message: &Value) {
    let body = serde_json::to_vec(message).expect("a script's message always serializes");
    let _ = out.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    let _ = out.write_all(&body);
    let _ = out.flush();
}

fn read_frame<R: BufRead>(reader: &mut R) -> Option<Value> {
    let mut length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse().ok();
        }
    }
    let mut body = vec![0u8; length?];
    reader.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}
