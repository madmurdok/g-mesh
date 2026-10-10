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
    /// Several locations, answered as an array in this order - what pyright
    /// answers at a call into an `@overload` set (GM-348). Takes precedence
    /// over `definition` when non-empty.
    #[serde(default)]
    definitions: Vec<Location>,
    /// What `textDocument/hover` answers there: the markdown `value` of a
    /// `MarkupContent`, verbatim, so a script spells the fences and
    /// paragraphs a real server would. Omitted means `null`.
    #[serde(default)]
    hover: Option<String>,
    /// Answer `textDocument/hover` there with this JSON-RPC error instead.
    #[serde(default)]
    hover_error: Option<String>,
    /// What `textDocument/implementation` answers there.
    #[serde(default)]
    implementation: Vec<Location>,
    /// Answer this one with a JSON-RPC error instead.
    #[serde(default)]
    error: Option<String>,
    /// That error's code - `-32603` (InternalError) when absent; `-32801` is
    /// LSP's `ContentModified` (GM-433).
    #[serde(default)]
    error_code: Option<i64>,
    /// Answer with the error only this many times, then answer normally -
    /// a server whose state moved under one question and settled. Absent
    /// means every time.
    #[serde(default)]
    error_times: Option<u32>,
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

/// rust-analyzer's `experimental/serverStatus` extension (GM-550): what the
/// server says about whether it has finished loading the project.
///
/// Like rust-analyzer, the server sends it only to a client that advertised
/// `capabilities.experimental.serverStatusNotification` in `initialize`
/// (unless `unasked`), and its first status - `quiescent: false` - is written
/// while `initialized` is handled, before any `$/progress` of [`Readiness`]
/// begins: measured, rust-analyzer sends it 4ms after `initialized`, so a
/// client never sees a gap between phases without having seen it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerStatus {
    /// `"afterReadiness"` (the default): `quiescent: true` once the last
    /// [`Readiness`] progress has ended - at once when the script reports
    /// none. `"never"`: the server stays `quiescent: false` for good.
    #[serde(default)]
    quiescent: Option<String>,
    /// Send the status even to a client that did not ask for it - a server
    /// pushing an extension the client never negotiated.
    #[serde(default)]
    unasked: bool,
}

/// The GM-433 shape: a server reloading its project model after a change it
/// noticed itself - rust-analyzer after `Cargo.toml` changed, which it
/// watches on its own, so nothing on the client's wire starts it.
///
/// The test starts it by creating `trigger` (the stand-in for the edited
/// manifest); a watcher thread runs `phases` as [`Readiness::phases`] does
/// and writes `started` once the first phase's `begin` is on the wire, so the
/// test can know the server is mid-reload before it asks - which is what
/// makes the gap between the phases, not a race with the trigger, the thing
/// under test. Every question is answered `null` from the trigger until the
/// last phase ends, gaps included: measured, rust-analyzer answers nothing
/// useful between "Building compile-time-deps" and the end of "Indexing".
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReloadOnFile {
    trigger: String,
    started: String,
    phases: Vec<Phase>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Script {
    #[serde(default)]
    reload_on_file: Option<ReloadOnFile>,
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
    /// See [`ServerStatus`]. Absent: the server never sends a status, as a
    /// server without the extension does not.
    #[serde(default)]
    server_status: Option<ServerStatus>,
    /// Where to write `capabilities.experimental` from the client's
    /// `initialize`, verbatim (`null` when it sent none), so a test can
    /// assert which extensions the client asked for.
    #[serde(default)]
    capabilities_out: Option<String>,
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
    /// Where to append `<uri> <languageId>` per `didOpen`, so a test can
    /// assert which language each document was opened as.
    #[serde(default)]
    opened_log: Option<String>,
    /// Hold the nth definition/implementation answer (1-based, counted per
    /// server process, in the order the requests arrive) for `holdMs[n-1]`
    /// milliseconds, on its own thread, while this server goes on reading -
    /// a server that is slow to answer one question without being deaf to
    /// the next, which is what lets a test see how many questions a client
    /// keeps in flight. A missing or zero entry is not held.
    #[serde(default)]
    hold_ms: Vec<u64>,
    /// Answer these arrivals (1-based, as `holdMs` counts them) with a
    /// JSON-RPC error instead - a refusal tied to *when* a question comes
    /// rather than to where it points.
    #[serde(default)]
    refuse: Vec<u32>,
    /// Where to append `asked <n>` when the nth definition/implementation
    /// request arrives and `answered <n>` just before its answer is written,
    /// so a test can count the questions a client had outstanding at once
    /// from the server's side rather than from a clock.
    #[serde(default)]
    timeline: Option<String>,
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
    // Whether a `reloadOnFile` reload is under way - see [`ReloadOnFile`].
    let reloading = Arc::new(AtomicBool::new(false));
    // How many times each scripted answer has been sent as an error, for
    // `errorTimes`.
    let mut errors_sent: Vec<u32> = vec![0; script.answers.len()];
    // Whether the client's `initialize` asked for `experimental/serverStatus`.
    let mut status_asked = false;

    while let Some(message) = read_frame(&mut reader) {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        log(&script, &method);
        if method == "textDocument/didOpen" {
            log_opened(&script, &params);
        }

        match method.as_str() {
            "initialize" => {
                let experimental =
                    params.pointer("/capabilities/experimental").cloned().unwrap_or(Value::Null);
                status_asked = experimental.get("serverStatusNotification") == Some(&json!(true));
                if let Some(path) = &script.capabilities_out {
                    let _ = std::fs::write(path, experimental.to_string());
                }
                let mut capabilities = json!({
                    "definitionProvider": true,
                    "implementationProvider": true,
                    "hoverProvider": true,
                    "textDocumentSync": 1,
                });
                if let Some(encoding) = &script.position_encoding {
                    capabilities["positionEncoding"] = json!(encoding);
                }
                respond(&mut stdout, id, json!({ "capabilities": capabilities }));
            }
            "initialized" => {
                let status = script.server_status.as_ref().filter(|status| status.unasked || status_asked);
                if status.is_some() {
                    notify(
                        &mut stdout,
                        "experimental/serverStatus",
                        json!({ "health": "ok", "quiescent": false }),
                    );
                }
                let then_quiescent =
                    status.is_some_and(|status| status.quiescent.as_deref() != Some("never"));
                start_progress(&script, Arc::clone(&indexing), then_quiescent);
                watch_for_reload(&script, Arc::clone(&reloading));
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
                mark(&script.timeline, &format!("asked {asked}"));
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
                let found = script.answers.iter().position(|answer| {
                    (answer.uri.as_str(), answer.line, answer.character) == (key.0.as_str(), key.1, key.2)
                });
                let answer = found.map(|at| &script.answers[at]);
                let erroring = found.is_some_and(|at| {
                    let answer = &script.answers[at];
                    answer.error.is_some() && answer.error_times.is_none_or(|times| errors_sent[at] < times)
                });
                // The answer is built here and written below, now or after its
                // hold.
                let mut frame: Vec<u8> = Vec::new();
                let refusing = script.refuse.contains(&asked);
                match answer {
                    Some(answer) if answer.silent && !refusing => continue,
                    _ if refusing => {
                        error_response(&mut frame, id, -32603, "refused by the script".to_string())
                    }
                    Some(answer) if erroring => {
                        if let Some(at) = found {
                            errors_sent[at] += 1;
                        }
                        error_response(
                            &mut frame,
                            id,
                            answer.error_code.unwrap_or(-32603),
                            answer.error.clone().unwrap_or_default(),
                        )
                    }
                    Some(_) if reloading.load(Ordering::SeqCst) => respond(&mut frame, id, Value::Null),
                    Some(_) if script.null_while_indexing && indexing.load(Ordering::SeqCst) => {
                        respond(&mut frame, id, Value::Null)
                    }
                    // GM-309's window: a `didChange` has started a
                    // `reindexOnChange` cycle and it has not yet ended,
                    // whether or not its progress has even begun - see
                    // [`ReindexOnChange`].
                    Some(_) if changed.load(Ordering::SeqCst) && !revealed.load(Ordering::SeqCst) => {
                        respond(&mut frame, id, Value::Null)
                    }
                    Some(answer) if method.ends_with("definition") && !answer.definitions.is_empty() => {
                        let result: Vec<Value> = answer.definitions.iter().map(Location::to_json).collect();
                        respond(&mut frame, id, Value::Array(result))
                    }
                    Some(answer) if method.ends_with("definition") => {
                        let result = answer.definition.as_ref().map(Location::to_json).unwrap_or(Value::Null);
                        respond(&mut frame, id, result)
                    }
                    Some(answer) => {
                        let result: Vec<Value> =
                            answer.implementation.iter().map(Location::to_json).collect();
                        respond(&mut frame, id, Value::Array(result))
                    }
                    None => respond(&mut frame, id, Value::Null),
                }
                let ordinal = asked;
                let timeline = script.timeline.clone();
                let deliver = move || {
                    mark(&timeline, &format!("answered {ordinal}"));
                    let mut out = std::io::stdout();
                    let _ = out.write_all(&frame);
                    let _ = out.flush();
                };
                let hold = script.hold_ms.get(asked as usize - 1).copied().unwrap_or(0);
                if hold > 0 {
                    let held = std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(hold));
                        deliver();
                    });
                    // A crash waits for the held answer, which is part of
                    // what this server says before it goes.
                    if crashing {
                        let _ = held.join();
                    }
                } else {
                    deliver();
                }
                if crashing {
                    // Not an `exit`: the point is a server that goes away
                    // without saying anything, which is what a crash is.
                    std::process::exit(101);
                }
            }
            // GM-348: a hover is matched on its position like a definition,
            // and is not counted by `crashAfterRequests`/`silentFrom`, which
            // were written about definitions and stay about them.
            "textDocument/hover" => {
                let key = position_of(&params);
                let answer = script.answers.iter().find(|answer| {
                    (answer.uri.as_str(), answer.line, answer.character) == (key.0.as_str(), key.1, key.2)
                });
                match answer {
                    Some(answer) if answer.silent => continue,
                    Some(answer) if answer.hover_error.is_some() => error_response(
                        &mut stdout,
                        id,
                        -32603,
                        answer.hover_error.clone().unwrap_or_default(),
                    ),
                    Some(answer) => {
                        let result = answer
                            .hover
                            .as_ref()
                            .map(|value| json!({ "contents": { "kind": "markdown", "value": value } }))
                            .unwrap_or(Value::Null);
                        respond(&mut stdout, id, result)
                    }
                    None => respond(&mut stdout, id, Value::Null),
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
///
/// `then_quiescent`: send `experimental/serverStatus` `quiescent: true` once
/// that progress has ended (at once when there is none) - see
/// [`ServerStatus`].
fn start_progress(script: &Script, indexing: Arc<AtomicBool>, then_quiescent: bool) {
    let quiescent = move |stdout: &mut std::io::Stdout| {
        if then_quiescent {
            notify(stdout, "experimental/serverStatus", json!({ "health": "ok", "quiescent": true }));
        }
    };
    let Some(readiness) = script.readiness.clone() else {
        quiescent(&mut std::io::stdout());
        return;
    };
    let kind = readiness.kind.clone().unwrap_or_else(|| "progress".to_string());
    if kind == "none" {
        quiescent(&mut std::io::stdout());
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
            quiescent(&mut stdout);
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
        quiescent(&mut stdout);
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

/// Starts the watcher for a `reloadOnFile` reload, if one is scripted - see
/// [`ReloadOnFile`]. One reload per server, which is all its one test needs.
fn watch_for_reload(script: &Script, reloading: Arc<AtomicBool>) {
    let Some(reload) = script.reload_on_file.clone() else { return };
    std::thread::spawn(move || {
        while !std::path::Path::new(&reload.trigger).exists() {
            std::thread::sleep(Duration::from_millis(5));
        }
        reloading.store(true, Ordering::SeqCst);
        let mut stdout = std::io::stdout();
        for (at, phase) in reload.phases.iter().enumerate() {
            std::thread::sleep(Duration::from_millis(phase.begin_after_ms));
            notify(
                &mut stdout,
                "$/progress",
                json!({ "token": phase.token, "value": { "kind": "begin", "title": phase.token } }),
            );
            if at == 0 {
                let _ = std::fs::write(&reload.started, "");
            }
            std::thread::sleep(Duration::from_millis(phase.hold_ms));
            // Cleared *before* the last `end` goes out, so a question the
            // client sends once it has read that `end` is answered for real.
            if at + 1 == reload.phases.len() {
                reloading.store(false, Ordering::SeqCst);
            }
            notify(&mut stdout, "$/progress", json!({ "token": phase.token, "value": { "kind": "end" } }));
        }
        reloading.store(false, Ordering::SeqCst);
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

fn log_opened(script: &Script, params: &Value) {
    let Some(path) = &script.opened_log else { return };
    let document = &params["textDocument"];
    let line = format!(
        "{} {}\n",
        document["uri"].as_str().unwrap_or(""),
        document["languageId"].as_str().unwrap_or("")
    );
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Appends one line to the `timeline` file, if the script names one.
fn mark(timeline: &Option<String>, event: &str) {
    let Some(path) = timeline else { return };
    // One `write` per line, for the reason `log` gives.
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(format!("{event}\n").as_bytes());
    }
}

fn log(script: &Script, method: &str) {
    let Some(path) = &script.log else { return };
    // One `write` per line, never `writeln!`: that writes the method and the
    // newline as two calls, and a server killed between them (a test that
    // reaps servers does exactly that, right after `initialized`) leaves a
    // line with no end, so the next server's first line joins onto it
    // (`initializedinitialize`) and a count of either comes up short.
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(format!("{method}\n").as_bytes());
    }
}

fn respond<W: Write>(out: &mut W, id: Option<Value>, result: Value) {
    let Some(id) = id else { return };
    write_frame(out, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn error_response<W: Write>(out: &mut W, id: Option<Value>, code: i64, message: String) {
    let Some(id) = id else { return };
    write_frame(out, &json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }));
}

fn notify<W: Write>(out: &mut W, method: &str, params: Value) {
    write_frame(out, &json!({ "jsonrpc": "2.0", "method": method, "params": params }));
}

/// The base protocol's framing, written out here rather than imported from the
/// SDK on purpose: a fixture that shares an implementation with the thing it
/// tests cannot catch that implementation being wrong.
fn write_frame<W: Write>(out: &mut W, message: &Value) {
    let body = serde_json::to_vec(message).expect("a script's message always serializes");
    // One write per frame: `Stdout` locks per call, so a frame written in one
    // call cannot interleave with one a held answer's thread writes.
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    let _ = out.write_all(&frame);
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
