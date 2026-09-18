//! The client half of the Language Server Protocol: as much of it as
//! answering an open site needs, and not one method more.
//!
//! # Decision 2: no LSP crate
//!
//! The obvious move is `lsp-types` (or `lsp-server`, or `tower-lsp`), and it
//! was weighed and refused. What this bridge sends is six requests and four
//! notifications - `initialize`, `initialized`, `textDocument/didOpen`,
//! `didChange`, `definition`, `implementation`, `$/cancelRequest`,
//! `shutdown`, `exit`, plus the two server-initiated messages it has to
//! answer - and what it reads out of the replies is a URI and two integers.
//!
//! Against that:
//!
//! - **`lsp-types` is the whole protocol as Rust types.** It models every
//!   request, every capability struct and every optional field of a
//!   specification that gains more of them every release, to save this file
//!   the `json!` literals below. It also pins the protocol *version* into the
//!   types, so keeping up with a server that speaks a newer one becomes a
//!   dependency bump rather than an extra key in an object this crate already
//!   passes through.
//! - **`lsp-server`/`tower-lsp` bring a transport and a runtime** - crossbeam
//!   channels, or tokio. This crate's whole dependency list is `anyhow`,
//!   `serde`, `serde_json`, `sha2`, `toml` and `ignore`, and the design's
//!   reason for that is stated in the doc: a plugin must not link what core
//!   links.
//! - **Every later language inherits this cost.** The SDK is what languages
//!   #3 through #7 are built on (the design doc's "Paper stress test"), so a
//!   dependency here is paid seven times and removed never.
//!
//! The counter-argument - "hand-rolled protocol code is where bugs live" - is
//! real, and is why the framing is [`crate::framing`] (one implementation,
//! already exercised against core) and why the fake server in
//! `plugins/sdk/fake-lsp` drives every path in this file, including the ones
//! a real server only reaches when something goes wrong.
//!
//! # The server is a child, deliberately
//!
//! It is spawned as an ordinary child of the plugin process: no `setsid`, no
//! double fork, no re-parenting. That is what makes
//! `[plugin] memoryLimitMb` work at all - core samples a plugin's whole
//! *process tree* (`core::daemon::memory::process_tree_rss_mb`, which walks
//! parent → child links from the plugin's pid) and the design says explicitly
//! that "tsserver, rust-analyzer and a language server behind the LSP bridge
//! all count against their plugin". A detached server would be invisible to
//! that sampler while holding the gigabytes the limit exists to catch.
//!
//! The other half of that promise is [`LspClient::shutdown`], called from
//! `Drop`: core ends a plugin by closing its stdin, so a server left running
//! when the plugin exits is an orphan holding a workspace open with nothing
//! reading its pipes.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use super::config::{SemanticConfig, ServerReadiness};
use super::position::{file_uri, PositionEncoding};
use crate::framing::{read_frame, write_message};

/// How many lines of a server's stderr are forwarded before the rest is
/// counted instead of printed.
///
/// Draining it is not optional - a server whose stderr pipe fills up blocks
/// on its next log line, which looks exactly like a hang - but forwarding all
/// of it is not either: a language server at its default log level can
/// produce megabytes per pass, and the daemon's log is a shared resource.
const STDERR_LINE_BUDGET: usize = 200;

/// What [`LspClient::poll`] saw.
#[derive(Debug)]
pub(crate) enum Poll {
    /// A response to a request this client sent. `result` is whatever the
    /// server answered, `Value::Null` included - which for `definition` is
    /// the ordinary "nothing here" answer and not an error.
    Answered { id: i64, result: Value },
    /// The server answered a request with a JSON-RPC error.
    Failed { id: i64, message: String },
    /// Something moved that is not an answer: a progress notification, a
    /// server request this client replied to, a log message. The caller
    /// re-reads whatever state it is waiting on.
    Noise,
    /// Nothing arrived before the timeout.
    Idle,
    /// The server's stdout is closed: it exited, or crashed.
    Closed,
}

/// One running language server, and everything this process knows about it.
pub(crate) struct LspClient {
    language: String,
    child: Child,
    stdin: ChildStdin,
    incoming: Receiver<Value>,
    next_id: i64,
    /// What to answer `workspace/configuration` with, by section - see
    /// [`SemanticConfig::settings`](super::config::SemanticConfig::settings).
    /// Copied out of the config at start-up because a server may ask at any
    /// moment, including from inside [`LspClient::poll`], where the config is
    /// not in scope.
    settings: BTreeMap<String, Value>,
    /// How the server counts columns, from the negotiation in `initialize`.
    encoding: PositionEncoding,
    /// Work-done progress tokens the server has begun and not ended. While
    /// this is non-empty the server is doing something it told us about -
    /// which is the difference between "there is no definition there" and
    /// "ask again when it has finished loading".
    active_progress: BTreeMap<String, ()>,
    /// Since when [`active_progress`](LspClient::active_progress) has been
    /// empty, or `None` while something is in flight.
    ///
    /// This, rather than "is it empty right now", is what readiness is
    /// measured against - see [`super::bridge::LspBridge`]'s readiness rules
    /// and the rust-analyzer trace that made the difference load-bearing. It
    /// starts at the client's own birth so that a server which never reports
    /// progress becomes quiet purely by the clock.
    idle_since: Option<Instant>,
    /// Whether this server has already been quiet for a full settle once -
    /// see [`LspClient::settle`]. It lives here rather than on the bridge
    /// because it is a fact about *this* server: the next one starts up all
    /// over again, and a flag that dies with the client cannot be left stale.
    ///
    /// It starts `true` for a
    /// [`ServerReadiness::OnDemand`](super::config::ServerReadiness::OnDemand)
    /// server (GM-310). That is the whole of that feature: "on demand" means
    /// *born in the state every server reaches after its first settle*, which
    /// is a state this client already had, that `wait_ready` already reads,
    /// and that GM-290 already measured on every pass after the first. No new
    /// rule, no second state machine - one server-shaped fact setting a latch
    /// that exists.
    settled: bool,
    /// Set once the server's stdout has closed, so a caller that polls again
    /// after a crash is told the same thing rather than blocking.
    closed: bool,
}

impl LspClient {
    /// Spawns the server, runs `initialize`/`initialized`, and returns a
    /// client that is connected but not necessarily *ready* - readiness is
    /// the bridge's question, and this one is "is there a server at all".
    ///
    /// An `Err` here is the design doc's "semantic engine missing" failure
    /// mode: no binary on `PATH`, or a binary that is not a language server.
    pub(crate) fn start(
        language: &str,
        config: &SemanticConfig,
        root: &Path,
        deadline: Instant,
    ) -> Result<Self> {
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&config.env)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start the language server {}", config.command.display()))?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // One thread per pipe, both detached. They end when the server's
        // stdout/stderr close, which is the only event either of them cares
        // about, and neither holds anything the main thread needs to join on.
        let (sender, incoming) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Ok(Some(body)) = read_frame(&mut reader) {
                let Ok(message) = serde_json::from_slice::<Value>(&body) else { continue };
                if sender.send(message).is_err() {
                    return;
                }
            }
        });
        let label = language.to_string();
        let server = config.engine.clone();
        std::thread::spawn(move || drain_stderr(&label, &server, stderr));

        let mut client = Self {
            language: language.to_string(),
            child,
            stdin,
            incoming,
            next_id: 1,
            settings: config.settings.clone(),
            encoding: PositionEncoding::Utf16,
            active_progress: BTreeMap::new(),
            idle_since: Some(Instant::now()),
            settled: config.readiness == ServerReadiness::OnDemand,
            closed: false,
        };
        client.initialize(config, root, deadline)?;
        Ok(client)
    }

    /// The `initialize` handshake, and the two things this bridge reads out of
    /// its answer: how the server counts columns, and nothing else.
    ///
    /// The capabilities sent are the smallest set that is honest. Claiming
    /// capabilities a client does not have is how a server ends up sending
    /// requests nobody answers, and every unanswered server request is a
    /// server that may never finish loading.
    fn initialize(&mut self, config: &SemanticConfig, root: &Path, deadline: Instant) -> Result<()> {
        let mut params = json!({
            // Not this process's pid by accident: it is what tells a server to
            // exit if we die without saying `exit`, which is the one safety net
            // against an orphaned server holding a workspace open.
            "processId": std::process::id(),
            "rootUri": file_uri(root),
            "workspaceFolders": [{ "uri": file_uri(root), "name": root.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "root".to_string()) }],
            "capabilities": {
                "general": { "positionEncodings": PositionEncoding::OFFERED },
                "window": { "workDoneProgress": true },
                "workspace": { "configuration": true, "workspaceFolders": true },
                "textDocument": {
                    "synchronization": { "dynamicRegistration": false },
                    "definition": { "dynamicRegistration": false, "linkSupport": true },
                    "implementation": { "dynamicRegistration": false, "linkSupport": true },
                },
            },
        });
        if let Some(options) = &config.initialization_options {
            params["initializationOptions"] = options.clone();
        }

        let id = self.request("initialize", params)?;
        let result = self.await_response(id, deadline).context("the server did not initialize")?;
        self.encoding = PositionEncoding::from_capability(
            result.get("capabilities").and_then(|caps| caps.get("positionEncoding")).and_then(Value::as_str),
        );
        self.notify("initialized", json!({}))?;
        Ok(())
    }

    /// How this server counts columns.
    pub(crate) fn encoding(&self) -> PositionEncoding {
        self.encoding
    }

    /// Whether the server has reported nothing in flight for at least
    /// `quiet` - which is what "this server has stopped working" means for a
    /// server that reports its work in a *sequence* of tokens rather than in
    /// one.
    ///
    /// `Duration::ZERO` asks only "is anything in flight right now", which is
    /// the right question once a server has already shown, once, that it has
    /// finished starting up.
    pub(crate) fn quiet_for(&self, quiet: Duration) -> bool {
        self.idle_since.is_some_and(|since| since.elapsed() >= quiet)
    }

    /// Tells this client that a document was just sent a `didOpen`/`didChange`
    /// it has not yet reacted to, so [`quiet_for`](LspClient::quiet_for) stops
    /// reporting a quiet period that predates the edit - GM-309.
    ///
    /// `settle` latching once per server (GM-290, see [`LspClient::settle`])
    /// means every pass after the first asks `quiet_for(Duration::ZERO)`
    /// rather than paying the settle again - correct once the server has
    /// actually caught up with whatever the pass just sent it, and wrong in
    /// the gap right after: a server does not begin reporting progress for an
    /// edit the instant it receives one, and measured for pyright
    /// (`docs/architecture/multi-language-plugins.md`, "Readiness, measured,
    /// and deliberately not changed") that gap is ~0.6s. A question answered
    /// empty inside it is answered by a server that has not yet noticed the
    /// edit, and [`super::bridge`]'s per-answer deferral - "an empty answer
    /// while the server is indexing is re-asked once" - only catches that
    /// when `idle_since` is fresh enough to say so.
    ///
    /// So this resets it, but only when the client is not already busy: if a
    /// progress is in flight `idle_since` is `None`, which already means
    /// "not quiet" more strongly than any timestamp could, and overwriting it
    /// with `Some(now)` would make a client that is genuinely mid-progress
    /// read as quiet for the instant before the next `$/progress` message
    /// corrects it. When the client *is* idle, resetting the clock buys
    /// [`Budgets::settle`](super::bridge::Budgets::settle) worth of
    /// scepticism toward the next empty answer - the same quiet period
    /// readiness already trusts, not a longer one - and if the server never
    /// reports anything for this edit at all, the deferred question is asked
    /// again once that period passes and the second answer, empty or not, is
    /// believed - exactly the server-that-reports-no-progress case
    /// [`LspClient::settle`] already handles for start-up. It costs nothing
    /// when no question lands in the gap, and at most one settle when one
    /// does - never a settle paid by every pass, which is the guarantee
    /// GM-290 measured and this must not spend back.
    pub(crate) fn mark_edited(&mut self) {
        if self.idle_since.is_some() {
            self.idle_since = Some(Instant::now());
        }
    }

    /// Whether this server is ready to be believed, latching the first time
    /// it is.
    ///
    /// The first answer costs a full `quiet` period of silence; every answer
    /// after that costs only "nothing is in flight right now". A server
    /// proves what shape it is once - see [`super::bridge::LspBridge`]'s doc
    /// on readiness - and a per-file pass that follows an edit must not pay
    /// for that proof again.
    ///
    /// A server whose manifest declares
    /// [`ServerReadiness::OnDemand`] starts with that latch already set
    /// (GM-310): the proof is the plugin author's trace rather than this
    /// process's own two seconds of waiting. Note what is *not* skipped -
    /// `quiet_for(Duration::ZERO)` still answers "is something in flight right
    /// now", so an on-demand server that happens to be mid-progress when a
    /// pass begins is still waited for, exactly as an indexed one is on its
    /// second pass.
    pub(crate) fn settle(&mut self, quiet: Duration) -> bool {
        if self.settled {
            return self.quiet_for(Duration::ZERO);
        }
        self.settled = self.quiet_for(quiet);
        self.settled
    }

    /// Whether this server is gone.
    ///
    /// Two independent signs, because either can be the first to show: its
    /// stdout closing (which the reader thread notices, and which is how a
    /// crash mid-conversation presents), and the process having exited (which
    /// is what a send failure leaves behind, where nothing may ever be read
    /// again to notice the closed pipe).
    pub(crate) fn gone(&mut self) -> bool {
        self.closed || matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Sends a request and returns the id its answer will carry.
    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<i64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .with_context(|| format!("failed to send {method} to the language server"))?;
        Ok(id)
    }

    /// Sends a notification - no answer, no id.
    pub(crate) fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .with_context(|| format!("failed to send {method} to the language server"))
    }

    /// Tells the server to stop working on a request whose answer is no
    /// longer wanted.
    ///
    /// Best-effort in the strongest sense: it is a notification, the server
    /// may ignore it, and an answer may already be in flight. What it buys is
    /// a server that stops *computing* an answer this side has given up
    /// waiting for - which matters exactly when a pass is already over
    /// budget, and costs nothing when it does not.
    pub(crate) fn cancel(&mut self, id: i64) {
        let _ = self.notify("$/cancelRequest", json!({ "id": id }));
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        write_message(&mut self.stdin, message)?;
        Ok(())
    }

    /// Waits up to `timeout` for the next thing the server says, handling
    /// everything that is not an answer to one of our requests.
    pub(crate) fn poll(&mut self, timeout: Duration) -> Poll {
        if self.closed {
            return Poll::Closed;
        }
        let message = match self.incoming.recv_timeout(timeout) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => return Poll::Idle,
            Err(RecvTimeoutError::Disconnected) => {
                self.closed = true;
                return Poll::Closed;
            }
        };
        self.handle(message)
    }

    /// [`LspClient::poll`] without waiting - drains what has already arrived.
    fn poll_now(&mut self) -> Poll {
        if self.closed {
            return Poll::Closed;
        }
        match self.incoming.try_recv() {
            Ok(message) => self.handle(message),
            Err(TryRecvError::Empty) => Poll::Idle,
            Err(TryRecvError::Disconnected) => {
                self.closed = true;
                Poll::Closed
            }
        }
    }

    /// One message from the server: an answer to pass up, or something this
    /// client deals with itself.
    fn handle(&mut self, message: Value) -> Poll {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");

        match (method, id) {
            // A request from the server. Every one of these must be answered,
            // including the ones this client has nothing to say about: a
            // server waiting on a reply that never comes is a server that
            // never finishes indexing, and "the plugin hung" is how that
            // presents.
            (Some(method), Some(id)) => {
                let id = id.clone();
                let result = server_request_reply(method, message.get("params"), &self.settings);
                let _ = self.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                Poll::Noise
            }
            // A notification.
            (Some(method), None) => {
                if method == "$/progress" {
                    self.track_progress(message.get("params"));
                }
                Poll::Noise
            }
            // A response to one of ours.
            (None, Some(id)) => {
                let Some(id) = id.as_i64() else { return Poll::Noise };
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the server reported an error with no message");
                    return Poll::Failed { id, message: text.to_string() };
                }
                Poll::Answered { id, result: message.get("result").cloned().unwrap_or(Value::Null) }
            }
            (None, None) => Poll::Noise,
        }
    }

    /// Follows a `$/progress` notification's `begin`/`end` for work-done
    /// tokens.
    ///
    /// Only `begin` and `end` are tracked, never `report`: a percentage is
    /// information for a human, and what this needs is the one bit "is the
    /// server still working". An `end` for a token that never began still
    /// clears it - servers do send those, and a token stuck "active" forever
    /// would make this client believe a ready server is busy for the rest of
    /// its life.
    ///
    /// Every `begin` also clears [`idle_since`](LspClient::idle_since), and
    /// the `end` that empties the set starts it again, so the quiet *period*
    /// - not the instantaneous emptiness of the set - is what a caller reads.
    fn track_progress(&mut self, params: Option<&Value>) {
        let Some(params) = params else { return };
        let Some(token) = params.get("token") else { return };
        let token = match token {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        match params.get("value").and_then(|value| value.get("kind")).and_then(Value::as_str) {
            Some("begin") => {
                self.active_progress.insert(token, ());
                self.idle_since = None;
            }
            Some("end") => {
                self.active_progress.remove(&token);
                if self.active_progress.is_empty() {
                    self.idle_since = Some(Instant::now());
                }
            }
            _ => {}
        }
    }

    /// Waits for one specific answer, dealing with everything else that
    /// arrives meanwhile. Used only for `initialize`, which is the one request
    /// this client has nothing else to do during.
    fn await_response(&mut self, wanted: i64, deadline: Instant) -> Result<Value> {
        loop {
            let Some(remaining) =
                deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())
            else {
                bail!("the server did not answer request {wanted} within the budget for this pass");
            };
            match self.poll(remaining.min(Duration::from_millis(100))) {
                Poll::Answered { id, result } if id == wanted => return Ok(result),
                Poll::Failed { id, message } if id == wanted => {
                    bail!("the server answered request {wanted} with an error: {message}")
                }
                Poll::Closed => bail!("the language server exited before answering request {wanted}"),
                _ => continue,
            }
        }
    }

    /// Ends the server: `shutdown`, `exit`, and a kill for one that ignores
    /// both.
    ///
    /// Every step is best-effort and none of them can fail this process. A
    /// server that has already crashed is the normal case for the first two,
    /// and the kill is what makes the *last* case - a server that answered
    /// `shutdown` and then kept running - bounded rather than permanent.
    pub(crate) fn shutdown(&mut self, grace: Duration) {
        let id = self.request("shutdown", Value::Null).ok();
        if let Some(id) = id {
            let _ = self.await_response(id, Instant::now() + grace);
        }
        let _ = self.notify("exit", Value::Null);

        let until = Instant::now() + grace;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
                _ => break,
            }
        }
        if self.child.kill().is_ok() {
            let _ = self.child.wait();
            eprintln!("[{}] the language server did not exit when asked - killed it", self.language);
        }
    }

    /// Drains whatever the server has said since the last poll, without
    /// waiting. Called before a pass starts so that progress notifications
    /// sent while the plugin was idle are accounted for rather than read as
    /// "the server is busy" on the next question.
    pub(crate) fn drain(&mut self) {
        while matches!(self.poll_now(), Poll::Noise | Poll::Answered { .. } | Poll::Failed { .. }) {}
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        self.shutdown(Duration::from_millis(500));
    }
}

/// What to answer a server-initiated request with.
///
/// `workspace/configuration` is the one that cannot be answered with a bare
/// `null`: it asks for *n* settings and the specification says the answer is
/// an array of *n* values, so a scalar makes a server that trusts its own
/// protocol index into nothing. One value per item, in the order asked.
///
/// Each item names a `section`, and that is the whole of the lookup:
/// `settings` is keyed by section name, a section it holds is answered with
/// its value, and anything else - a section nobody configured, an item with
/// no `section` at all - is answered `null`, which means "no configuration
/// for that section" and is what every server handles. GM-289 answered
/// `null` unconditionally, which was correct only for a server that takes its
/// settings through `initializationOptions`; pyright takes them **only**
/// here, and the measurement is in [`super::config`]'s module doc.
///
/// The `scopeUri` an item may also carry is deliberately ignored: this client
/// opens exactly one workspace folder, so every scope is that folder and a
/// per-scope answer would be the same answer with more ways to get it wrong.
///
/// Everything else gets `null`, which covers
/// `window/workDoneProgress/create` (an acknowledgement),
/// `client/registerCapability` (this client registers nothing dynamically,
/// and saying so is better than not answering) and whatever a future server
/// invents. None of these is language-specific: they are base-protocol
/// methods.
///
/// A free function because it reads nothing about the client beyond the map
/// it is handed, which is also what lets its own test call the real thing
/// rather than a copy of it.
fn server_request_reply(method: &str, params: Option<&Value>, settings: &BTreeMap<String, Value>) -> Value {
    match method {
        "workspace/configuration" => {
            let items = params
                .and_then(|params| params.get("items"))
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            Value::Array(
                items
                    .iter()
                    .map(|item| {
                        item.get("section")
                            .and_then(Value::as_str)
                            .and_then(|section| settings.get(section))
                            .cloned()
                            .unwrap_or(Value::Null)
                    })
                    .collect(),
            )
        }
        _ => Value::Null,
    }
}

/// Forwards the server's stderr, prefixed, up to a budget - see
/// [`STDERR_LINE_BUDGET`].
fn drain_stderr(language: &str, server: &str, stderr: std::process::ChildStderr) {
    use std::io::BufRead;
    let mut lines = 0usize;
    let mut suppressed = 0usize;
    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
        lines += 1;
        if lines <= STDERR_LINE_BUDGET {
            eprintln!("[{language}] {server}: {line}");
            if lines == STDERR_LINE_BUDGET {
                eprintln!(
                    "[{language}] {server}: further output is counted rather than printed - \
                     {STDERR_LINE_BUDGET} lines is this log's share of it"
                );
            }
        } else {
            suppressed += 1;
        }
    }
    if suppressed > 0 {
        eprintln!("[{language}] {server}: {suppressed} further line(s) of server output were suppressed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reply shapes a server-initiated request gets, which are the
    /// difference between a server that finishes loading and one that waits
    /// forever - see [`server_request_reply`]. The end-to-end version, with a
    /// real spawned server, is in `tests/lsp_bridge.rs`.
    #[test]
    fn a_configuration_request_is_answered_one_value_per_item() {
        let none = BTreeMap::new();
        let items = json!({ "items": [{}, {}, {}] });
        assert_eq!(
            server_request_reply("workspace/configuration", Some(&items), &none),
            json!([null, null, null])
        );
        assert_eq!(server_request_reply("workspace/configuration", None, &none), json!([]));

        let token = json!({ "token": "t" });
        assert_eq!(server_request_reply("window/workDoneProgress/create", Some(&token), &none), Value::Null);
        assert_eq!(server_request_reply("client/registerCapability", None, &none), Value::Null);
    }

    /// The GM-299 half: a configured section is answered with its value, in
    /// the order asked, and everything else stays `null`. pyright asks for
    /// `python` and `pyright` together and indexes the answer positionally,
    /// so the order and the length are the contract, not just the contents.
    #[test]
    fn a_configured_section_is_answered_with_its_value_and_the_rest_stay_null() {
        let mut settings = BTreeMap::new();
        settings.insert("python".to_string(), json!({ "analysis": { "typeCheckingMode": "basic" } }));

        let items = json!({ "items": [
            { "scopeUri": "file:///p", "section": "python" },
            { "scopeUri": "file:///p", "section": "pyright" },
            { "scopeUri": "file:///p" },
        ] });
        assert_eq!(
            server_request_reply("workspace/configuration", Some(&items), &settings),
            json!([{ "analysis": { "typeCheckingMode": "basic" } }, null, null]),
        );

        // A section nobody asks for is never sent, and asking twice answers
        // twice - a server may re-request after a `didChangeConfiguration`.
        let twice = json!({ "items": [{ "section": "python" }, { "section": "python" }] });
        let answer = server_request_reply("workspace/configuration", Some(&twice), &settings);
        assert_eq!(answer.as_array().map(Vec::len), Some(2));
        assert_eq!(answer[0], answer[1]);
    }
}
