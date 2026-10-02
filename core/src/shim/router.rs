//! The shim's switchable router (D11 step 3 in
//! `docs/architecture/lazy-indexing.md`, GM-399 slice 5).
//!
//! A session starts on the daemon serving the shim's own root. When that root
//! is a folder of projects, the daemon is a *front*, and a `select_project`
//! result carries `_meta["g-mesh/switchProject"].root = C`. The router then
//! connects to `C`'s own, normal daemon, replays the client's recorded
//! `initialize` to it, and from then on routes the session there - except
//! `select_project` and `tools/list`, which keep going to the front so the
//! agent can switch again.
//!
//! # What gets parsed
//!
//! Client frames are parsed, to learn which ones to record or route
//! specially; they are forwarded as the original bytes, never re-serialized.
//! Every upstream frame is scanned for its `id` and `method` only (the rest,
//! such as a large tool result, is skipped, not built), to settle the request
//! it answers. Upstream frames are parsed in full only on the front's
//! connection, and only while a `select_project` call is outstanding. A
//! single-project session never has a switch directive to act on, so for it
//! every frame crosses byte-for-byte, as it did before this module existed; a
//! frame that fails to parse is forwarded raw.
//!
//! # Which project answered
//!
//! Every agent on one client connection shares the session's selection, and
//! the shim cannot tell them apart, so one agent's `select_project` reroutes
//! another's later calls. A `tools/call` result that a sub-project's daemon
//! answers therefore starts with a text item naming that project, relative
//! to the shim's root ([`answered_from`]): the root of the upstream the call
//! was sent to, not the selection at the time the answer arrives. Such a
//! result is parsed and re-serialized to add it. Front answers, JSON-RPC
//! error responses, and answers the shim makes itself are not stamped (see
//! `docs/adr/0014-answering-project-stamp.md`).
//!
//! # Every forwarded request is answered
//!
//! Each request forwarded upstream is recorded with the upstream it went to
//! until its response passes back. A switch does not close the previous
//! sub-project upstream while it still owes answers: it is *retired*, and
//! half-closed once its last one has passed. An upstream that ends while it
//! still owes answers has the shim answer each of them with an error, so the
//! client never waits on a request nobody will answer.
//!
//! # Before initialize
//!
//! A daemon ends a connection whose first message is not `initialize`, so
//! nothing reaches it before the client's `initialize` does. Until then the
//! shim answers each request itself: `ping` with an empty result, any other
//! method with JSON-RPC -32601 (Method not found), which is how a client
//! probing for a newer protocol era (`server/discover`) learns to fall back
//! to `initialize`. Notifications, responses and unparsable frames sent
//! before `initialize` are dropped. A `ping` sent after `initialize` but
//! before `notifications/initialized` is answered by the shim too, since the
//! daemon accepts nothing but that notification in between.
//!
//! # Who writes stdout
//!
//! Only the thread running [`serve`]. Every upstream reader hands it whole
//! frames through one channel, so two readers' frames cannot interleave: that
//! holds by construction rather than by locking discipline.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::de::IgnoredAny;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::front::{SELECT_PROJECT, SWITCH_PROJECT_META};
use crate::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};

/// One connection to a daemon, split into what the router needs of it.
pub(crate) struct Link {
    pub reader: Box<dyn BufRead + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Half-closes (or closes) the connection without needing the writer,
    /// which another thread may be blocked in.
    pub closer: Box<dyn Fn(Shutdown) + Send + Sync>,
}

/// Connects to (bootstrapping if needed) the daemon serving a root.
pub(crate) type Connector = Box<dyn Fn(&Path) -> Result<Link> + Send + Sync>;

#[derive(Clone)]
struct Upstream {
    id: u64,
    /// The root its daemon serves.
    root: PathBuf,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    closer: Arc<dyn Fn(Shutdown) + Send + Sync>,
}

impl Upstream {
    fn new(
        id: u64,
        root: PathBuf,
        writer: Box<dyn Write + Send>,
        closer: Box<dyn Fn(Shutdown) + Send + Sync>,
    ) -> Self {
        Self { id, root, writer: Arc::new(Mutex::new(writer)), closer: Arc::from(closer) }
    }

    fn send(&self, frame: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        write_ndjson_frame(&mut *writer, frame)
    }

    fn close(&self, how: Shutdown) {
        (self.closer)(how);
    }
}

struct Router {
    /// The shim's own root, canonical: a switch target must lie below it.
    root: PathBuf,
    /// The connection to the root's daemon (the front, in a multi-project
    /// session); `None` once it has ended.
    front: Option<Upstream>,
    /// The selected sub-project's daemon, once a switch has happened.
    sub: Option<Upstream>,
    /// Sub-project upstreams switched away from while they still owed
    /// answers. Each is half-closed once it owes none, and dropped from here
    /// when its connection ends.
    retired: Vec<Retired>,
    /// Client requests forwarded upstream and not yet answered, by id.
    pending: HashMap<Value, InFlight>,
    init_frame: Option<Vec<u8>>,
    initialized_frame: Option<Vec<u8>>,
    /// Ids of `select_project` calls sent to the front and not yet answered.
    select_ids: HashSet<Value>,
    replay_seq: u64,
    next_upstream: u64,
}

struct Retired {
    upstream: Upstream,
    /// The root the session switched to when this upstream was retired.
    switched_to: PathBuf,
}

struct InFlight {
    upstream: u64,
    method: String,
    sent: Instant,
}

impl Router {
    fn current(&self) -> Option<&Upstream> {
        self.sub.as_ref().or(self.front.as_ref())
    }

    fn upstream(&self, id: u64) -> Option<&Upstream> {
        [self.front.as_ref(), self.sub.as_ref()]
            .into_iter()
            .flatten()
            .chain(self.retired.iter().map(|retired| &retired.upstream))
            .find(|upstream| upstream.id == id)
    }

    /// Half-closes upstream `id` if it is retired and owes no answers. Its
    /// reader then sees the connection end.
    fn release_if_drained(&self, id: u64) {
        if self.pending.values().any(|in_flight| in_flight.upstream == id) {
            return;
        }
        if let Some(retired) = self.retired.iter().find(|retired| retired.upstream.id == id) {
            retired.upstream.close(Shutdown::Write);
        }
    }
}

enum Event {
    Frame(Vec<u8>),
    /// The current upstream's connection ended: the session is over.
    Done,
}

struct Shared {
    router: Mutex<Router>,
    events: mpsc::Sender<Event>,
    connector: Connector,
}

/// What the router needs to know about a client frame.
enum ClientFrame {
    Initialize,
    Initialized,
    SelectProject(Value),
    ToolsList,
    /// `notifications/cancelled` for the request with this id.
    Cancelled(Value),
    Other,
}

/// A client frame's routing class, and its id and method when it is a
/// request (a response to a server request has an id but no method).
fn classify(frame: &[u8]) -> (ClientFrame, Option<(Value, String)>) {
    let Ok(message) = serde_json::from_slice::<Value>(frame) else {
        return (ClientFrame::Other, None);
    };
    let method = message.get("method").and_then(Value::as_str);
    let request = match (message.get("id"), method) {
        (Some(id), Some(method)) if !id.is_null() => Some((id.clone(), method.to_string())),
        _ => None,
    };
    let kind = match method {
        Some("initialize") => ClientFrame::Initialize,
        Some("notifications/initialized") => ClientFrame::Initialized,
        Some("tools/list") => ClientFrame::ToolsList,
        Some("notifications/cancelled") => {
            match message.get("params").and_then(|params| params.get("requestId")) {
                Some(id) => ClientFrame::Cancelled(id.clone()),
                None => ClientFrame::Other,
            }
        }
        Some("tools/call") => {
            let name = message.get("params").and_then(|params| params.get("name")).and_then(Value::as_str);
            match (name, message.get("id")) {
                (Some(SELECT_PROJECT), Some(id)) => ClientFrame::SelectProject(id.clone()),
                _ => ClientFrame::Other,
            }
        }
        _ => ClientFrame::Other,
    };
    (kind, request)
}

/// The id of the request an upstream frame answers, or `None` for a
/// notification, a server request, or an unparsable frame. Only `id` and
/// `method` are built; every other field is skipped.
fn response_id(frame: &[u8]) -> Option<Value> {
    #[derive(Deserialize)]
    struct Head {
        id: Option<Value>,
        method: Option<IgnoredAny>,
    }
    match serde_json::from_slice::<Head>(frame) {
        Ok(Head { id: Some(id), method: None }) => Some(id),
        _ => None,
    }
}

/// Runs a session: `client_in`/`client_out` are the MCP client's side,
/// `front` the connection to the daemon serving `root`. Returns when the
/// current upstream's connection ends - the shim lives as long as its
/// session, exactly as before the router existed.
pub(crate) fn serve<R, W>(
    client_in: R,
    mut client_out: W,
    front: Link,
    root: PathBuf,
    connector: Connector,
) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write,
{
    let (events, received) = mpsc::channel();
    let Link { reader, writer, closer } = front;
    let shared = Arc::new(Shared {
        router: Mutex::new(Router {
            root: root.clone(),
            front: Some(Upstream::new(0, root.clone(), writer, closer)),
            sub: None,
            retired: Vec::new(),
            pending: HashMap::new(),
            init_frame: None,
            initialized_frame: None,
            select_ids: HashSet::new(),
            replay_seq: 0,
            next_upstream: 1,
        }),
        events,
        connector,
    });
    spawn_reader(&shared, 0, reader, true);

    // Never joined: a blocking read on stdin cannot be interrupted, and
    // process exit tears it down.
    let client = Arc::clone(&shared);
    thread::spawn(move || client.client_loop(client_in));

    for event in received {
        match event {
            Event::Frame(frame) => write_ndjson_frame(&mut client_out, &frame)?,
            Event::Done => break,
        }
    }
    Ok(())
}

fn spawn_reader(shared: &Arc<Shared>, id: u64, mut reader: Box<dyn BufRead + Send>, is_front: bool) {
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        loop {
            match read_ndjson_frame(&mut reader) {
                Ok(Some(mut frame)) => {
                    if let Some(answered) = response_id(&frame) {
                        if let Some(stamp) = shared.settle(id, &answered) {
                            frame = stamped(frame, &stamp);
                        }
                    }
                    let frame = if is_front { shared.on_front_frame(frame) } else { Some(frame) };
                    if let Some(frame) = frame {
                        if shared.events.send(Event::Frame(frame)).is_err() {
                            return;
                        }
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    eprintln!("g-mesh mcp-shim: daemon stream ended: {err:#}");
                    break;
                }
            }
        }
        shared.upstream_ended(id);
    });
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Router> {
        self.router.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn client_loop<R: BufRead>(&self, mut input: R) {
        let result = (|| -> Result<()> {
            while let Some(frame) = read_ndjson_frame(&mut input)? {
                self.on_client_frame(frame);
            }
            Ok(())
        })();
        // EOF on stdin means the client is done: half-close every upstream so
        // each can still flush replies in flight, then closes. Windows named
        // pipes have no half-close, so there this ends the whole connection
        // instead - see `ipc::windows::Stream::shutdown` for what that costs.
        let how = match &result {
            Ok(()) => Shutdown::Write,
            Err(err) => {
                eprintln!("g-mesh mcp-shim: client stream ended: {err:#}");
                Shutdown::Both
            }
        };
        let router = self.lock();
        let retired = router.retired.iter().map(|retired| &retired.upstream);
        for upstream in [router.front.as_ref(), router.sub.as_ref()].into_iter().flatten().chain(retired) {
            upstream.close(how);
        }
    }

    /// Records that upstream `upstream` answered request `id`. Returns the
    /// line the answer must start with: [`answered_from`] when it answers a
    /// `tools/call` and `upstream` serves a sub-project, `None` otherwise.
    fn settle(&self, upstream: u64, id: &Value) -> Option<String> {
        let mut router = self.lock();
        if !router.pending.get(id).is_some_and(|in_flight| in_flight.upstream == upstream) {
            return None;
        }
        let in_flight = router.pending.remove(id)?;
        router.release_if_drained(upstream);
        let served = &router.upstream(upstream)?.root;
        // The front serves the shim's root; a sub-project never does (a
        // switch refuses the root itself).
        (in_flight.method == "tools/call" && *served != router.root)
            .then(|| answered_from(&router.root, served))
    }

    fn on_client_frame(&self, frame: Vec<u8>) {
        let (kind, request) = classify(&frame);
        if self.answered_before_session(&kind, request.as_ref()) {
            return;
        }
        // The upstream a cancelled request went to, when it owes nothing else
        // once the cancel is sent.
        let mut cancelled_on = None;
        let target = {
            let mut router = self.lock();
            let target = match kind {
                ClientFrame::Initialize => {
                    router.init_frame = Some(frame.clone());
                    router.current().cloned()
                }
                ClientFrame::Initialized => {
                    router.initialized_frame = Some(frame.clone());
                    router.current().cloned()
                }
                ClientFrame::SelectProject(id) => match router.front.clone() {
                    Some(front) => {
                        // Recorded before the frame is sent, so the answer
                        // can never overtake it.
                        router.select_ids.insert(id);
                        Some(front)
                    }
                    None => {
                        let _ = self.events.send(Event::Frame(error_result(
                            &id,
                            &format!(
                                "g-mesh: the connection to the daemon serving {} has ended, so this session \
                                 cannot switch projects any more. It keeps serving the project it serves now.",
                                router.root.display()
                            ),
                        )));
                        None
                    }
                },
                ClientFrame::ToolsList => router.front.clone().or_else(|| router.current().cloned()),
                // The cancel goes where the request went; a daemon is not
                // required to answer a cancelled request, so it is no longer
                // owed.
                ClientFrame::Cancelled(id) => match router.pending.remove(&id) {
                    Some(in_flight) => {
                        cancelled_on = Some(in_flight.upstream);
                        router.upstream(in_flight.upstream).cloned()
                    }
                    None => router.current().cloned(),
                },
                ClientFrame::Other => router.current().cloned(),
            };
            // Recorded before the frame is sent, so the answer can never
            // overtake it.
            if let (Some(upstream), Some((id, method))) = (&target, request) {
                router.pending.insert(id, InFlight { upstream: upstream.id, method, sent: Instant::now() });
            }
            target
        };
        if let Some(upstream) = target {
            if let Err(err) = upstream.send(&frame) {
                // Its reader ends too, and decides whether the session does.
                eprintln!("g-mesh mcp-shim: could not forward a frame to the daemon: {err:#}");
            }
        }
        if let Some(upstream) = cancelled_on {
            self.lock().release_if_drained(upstream);
        }
    }

    /// Handles a client frame that must not reach a daemon yet (see the
    /// module docs, "Before initialize"). Returns whether it did.
    fn answered_before_session(&self, kind: &ClientFrame, request: Option<&(Value, String)>) -> bool {
        let (initialize_sent, initialized_sent) = {
            let router = self.lock();
            (router.init_frame.is_some(), router.initialized_frame.is_some())
        };
        if initialized_sent || matches!(kind, ClientFrame::Initialize) {
            return false;
        }
        let answer = match request {
            Some((id, method)) if method == "ping" => {
                json!({ "jsonrpc": "2.0", "id": id, "result": {} })
            }
            Some(_) if initialize_sent => return false,
            Some((id, method)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") },
            }),
            None if initialize_sent => return false,
            None => {
                eprintln!(
                    "g-mesh mcp-shim: dropped a client frame that is not a request, sent before initialize"
                );
                return true;
            }
        };
        let _ =
            self.events.send(Event::Frame(serde_json::to_vec(&answer).expect("a Value always serializes")));
        true
    }

    /// Passes a front frame through, or - for the answer to a
    /// `select_project` call that carries a switch directive - performs the
    /// switch, sends the rewritten answer itself and returns `None`.
    fn on_front_frame(self: &Arc<Self>, frame: Vec<u8>) -> Option<Vec<u8>> {
        let mut router = self.lock();
        if router.select_ids.is_empty() {
            return Some(frame);
        }
        let Ok(mut message) = serde_json::from_slice::<Value>(&frame) else {
            return Some(frame);
        };
        let is_response = message.get("method").is_none();
        match message.get("id") {
            Some(id) if is_response && router.select_ids.remove(id) => {}
            _ => return Some(frame),
        }
        let Some(target) = message
            .get("result")
            .and_then(|result| result.get("_meta"))
            .and_then(|meta| meta.get(SWITCH_PROJECT_META))
            .and_then(|switch| switch.get("root"))
            .and_then(Value::as_str)
            .map(PathBuf::from)
        else {
            return Some(frame);
        };

        let (text, is_error) = match self.switch(&mut router, &target) {
            Ok((served, guidance)) => (
                format!(
                    "g-mesh: this session now serves {served}; file paths are relative to it. Agents \
                     sharing this connection share this choice, so each tool result from here on starts \
                     with `g-mesh: answered from project <name>.`; if it names a project other than the \
                     one you selected, call select_project again before using the result. Guidance for \
                     this project, as a session started in {served} would receive it:\n\n{guidance}",
                    served = served.display()
                ),
                false,
            ),
            Err(err) => (
                format!(
                    "g-mesh: could not switch this session to {}: {err:#}. Nothing changed: the session \
                     keeps talking to the daemon it used before.",
                    target.display()
                ),
                true,
            ),
        };
        let result = &mut message["result"];
        result["content"] = json!([{ "type": "text", "text": text }]);
        if is_error {
            result["isError"] = Value::Bool(true);
        }
        if let Some(meta) = result.get_mut("_meta").and_then(Value::as_object_mut) {
            meta.remove(SWITCH_PROJECT_META);
            if meta.is_empty() {
                result.as_object_mut().map(|object| object.remove("_meta"));
            }
        }
        // Sent under the lock, so it reaches the client before anything the
        // new upstream answers.
        let _ =
            self.events.send(Event::Frame(serde_json::to_vec(&message).expect("a Value always serializes")));
        None
    }

    /// D11 step 3.1-3.5 and 3.7. On error nothing about routing has changed.
    fn switch(self: &Arc<Self>, router: &mut Router, target: &Path) -> Result<(PathBuf, String)> {
        let target = target.canonicalize().with_context(|| format!("cannot resolve {}", target.display()))?;
        if target == router.root || !target.starts_with(&router.root) {
            bail!("{} is not a project below {}", target.display(), router.root.display());
        }
        let init = router.init_frame.clone().context("the client never sent initialize")?;
        let mut init: Value = serde_json::from_slice(&init).context("the recorded initialize is not JSON")?;

        let mut link = (self.connector)(&target)?;

        router.replay_seq += 1;
        let replay_id = Value::String(format!("g-mesh-shim-replay-{}", router.replay_seq));
        init["id"] = replay_id.clone();
        write_ndjson_frame(&mut link.writer, &serde_json::to_vec(&init)?)
            .context("could not send initialize to its daemon")?;
        // Read here, before this connection has a reader thread, so the
        // replay's answer can never reach the client.
        let response = loop {
            let frame = read_ndjson_frame(&mut link.reader)?
                .context("its daemon closed the connection instead of answering initialize")?;
            if let Ok(message) = serde_json::from_slice::<Value>(&frame) {
                if message.get("id") == Some(&replay_id) {
                    break message;
                }
            }
        };
        if let Some(error) = response.get("error") {
            bail!("its daemon refused initialize: {error}");
        }
        let guidance = response
            .get("result")
            .and_then(|result| result.get("instructions"))
            .and_then(Value::as_str)
            .context("its daemon's initialize response carries no instructions")?
            .to_string();
        let initialized = router
            .initialized_frame
            .clone()
            .unwrap_or_else(|| br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_vec());
        write_ndjson_frame(&mut link.writer, &initialized)
            .context("could not send notifications/initialized to its daemon")?;

        let Link { reader, writer, closer } = link;
        let id = router.next_upstream;
        router.next_upstream += 1;
        spawn_reader(self, id, reader, false);
        if let Some(previous) = router.sub.replace(Upstream::new(id, target.clone(), writer, closer)) {
            // Its reader passes the replies it still owes, then ends.
            let previous_id = previous.id;
            router.retired.push(Retired { upstream: previous, switched_to: target.clone() });
            router.release_if_drained(previous_id);
        }
        Ok((target, guidance))
    }

    fn upstream_ended(&self, id: u64) {
        let mut router = self.lock();
        let current = router.current().map(|upstream| upstream.id);
        let served = router.upstream(id).map(|upstream| upstream.root.clone());
        if router.front.as_ref().map(|front| front.id) == Some(id) {
            router.front = None;
            // Nobody is left to answer these; say so rather than leave the
            // client waiting.
            let message = format!(
                "g-mesh: the connection to the daemon serving {} ended before it answered.",
                router.root.display()
            );
            for pending in router.select_ids.drain().collect::<Vec<_>>() {
                router.pending.remove(&pending);
                let _ = self.events.send(Event::Frame(error_result(&pending, &message)));
            }
        }
        if router.sub.as_ref().map(|sub| sub.id) == Some(id) {
            router.sub = None;
        }
        let reason = match router.retired.iter().position(|retired| retired.upstream.id == id) {
            Some(index) => {
                format!(
                    " (the session had switched to {})",
                    router.retired.remove(index).switched_to.display()
                )
            }
            None => String::new(),
        };
        let owed: Vec<Value> = router
            .pending
            .iter()
            .filter(|(_, in_flight)| in_flight.upstream == id)
            .map(|(request, _)| request.clone())
            .collect();
        let served = served.unwrap_or_else(|| router.root.clone());
        for request in owed {
            let Some(in_flight) = router.pending.remove(&request) else { continue };
            let message = format!(
                "g-mesh: this call was not answered: the connection to the daemon serving {} ended {} s \
                 after the call was sent{reason}. Its result, if any, was not received - call the tool again.",
                served.display(),
                in_flight.sent.elapsed().as_secs(),
            );
            let _ = self.events.send(Event::Frame(unanswered(&request, &in_flight.method, &message)));
        }
        if current == Some(id) {
            let _ = self.events.send(Event::Done);
        }
    }
}

/// The first text item of a `tools/call` result answered by the daemon
/// serving `served`, a project below `root`.
fn answered_from(root: &Path, served: &Path) -> String {
    let relative = served.strip_prefix(root).unwrap_or(served);
    let name: Vec<_> = relative.components().map(|part| part.as_os_str().to_string_lossy()).collect();
    format!("g-mesh: answered from project {}.", name.join("/"))
}

/// `frame` with `stamp` as the first text item of its tool result. A frame
/// with no `result.content` array (a JSON-RPC error, or a frame that does not
/// parse) is returned unchanged. A tool error (`isError`) is stamped too.
fn stamped(frame: Vec<u8>, stamp: &str) -> Vec<u8> {
    let Ok(mut message) = serde_json::from_slice::<Value>(&frame) else {
        return frame;
    };
    let Some(content) = message.get_mut("result").and_then(|result| result.get_mut("content")) else {
        return frame;
    };
    let Some(items) = content.as_array_mut() else {
        return frame;
    };
    items.insert(0, json!({ "type": "text", "text": stamp }));
    serde_json::to_vec(&message).expect("a Value always serializes")
}

fn error_result(id: &Value, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": true },
    }))
    .expect("a Value always serializes")
}

/// The shim's own answer to a request its upstream will never answer: a
/// tool error for `tools/call`, a JSON-RPC error for any other method, whose
/// result would not have a tool result's shape.
fn unanswered(id: &Value, method: &str, text: &str) -> Vec<u8> {
    if method == "tools/call" {
        return error_result(id, text);
    }
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32603, "message": text },
    }))
    .expect("a Value always serializes")
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufReader, Read};
    use std::time::Duration;

    use super::*;

    /// A single-project session crosses byte-for-byte in both directions:
    /// odd key order and whitespace survive, and an unparsable line is
    /// forwarded raw. Control: re-serialize parsed frames instead of
    /// forwarding the original bytes.
    #[test]
    fn single_project_frames_are_byte_identical() {
        let client_frames: [&[u8]; 4] = [
            br#"{ "params" : {"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}, "method":"initialize","id":0,"jsonrpc":"2.0"}"#,
            br#"{"method":"notifications/initialized",   "jsonrpc":"2.0"}"#,
            br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"arguments":{"b":1,"a":2},"name":"find_references"}}"#,
            b"this is { not json",
        ];
        let upstream_frames: [&[u8]; 3] = [
            br#"{"result":{"serverInfo":{"version":"1","name":"g-mesh"},"protocolVersion":"2025-06-18"},"id":0,"jsonrpc":"2.0"}"#,
            br#"{ "id" : 7 , "jsonrpc":"2.0", "result":{"content":[{"text":"x","type":"text"}]}}"#,
            b"} neither is this",
        ];

        let (client_r, mut client_w) = io::pipe().unwrap();
        let (to_upstream_r, to_upstream_w) = io::pipe().unwrap();
        let (from_upstream_r, mut from_upstream_w) = io::pipe().unwrap();
        let (mut out_r, out_w) = io::pipe().unwrap();

        let front = Link {
            reader: Box::new(BufReader::new(from_upstream_r)),
            writer: Box::new(to_upstream_w),
            closer: Box::new(|_| {}),
        };
        let connector: Connector = Box::new(|_| bail!("a single-project session never connects elsewhere"));
        let session = thread::spawn(move || {
            serve(BufReader::new(client_r), out_w, front, PathBuf::from("/nowhere"), connector)
        });

        for frame in client_frames {
            write_ndjson_frame(&mut client_w, frame).unwrap();
        }
        let mut upstream_side = BufReader::new(to_upstream_r);
        for expected in client_frames {
            let got =
                read_ndjson_frame(&mut upstream_side).unwrap().expect("the shim forwarded fewer frames");
            assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(expected));
        }

        for frame in upstream_frames {
            write_ndjson_frame(&mut from_upstream_w, frame).unwrap();
        }
        drop(from_upstream_w);
        session.join().unwrap().expect("the session must end cleanly when its daemon does");

        let mut out = Vec::new();
        out_r.read_to_end(&mut out).unwrap();
        let expected: Vec<u8> =
            upstream_frames.iter().flat_map(|frame| frame.iter().copied().chain(*b"\n")).collect();
        assert_eq!(String::from_utf8_lossy(&out), String::from_utf8_lossy(&expected));
        drop(client_w);
    }

    const WAIT: Duration = Duration::from_secs(2);

    /// A scripted daemon. Like the real daemon, it ends the connection when
    /// its first frame is not `initialize`. It answers `initialize` (with
    /// instructions naming its root) and `select_project` (with a switch
    /// directive to `<root>/<project>`) itself, and hands every other frame
    /// to the test, which answers through [`Fake::answer`]. Like the real
    /// daemon, it takes a half-close as "client gone" and closes its side:
    /// whatever it has not answered by then is never answered.
    struct Fake {
        root: PathBuf,
        got: mpsc::Receiver<Value>,
        writer: Arc<Mutex<Option<io::PipeWriter>>>,
        closes: Arc<Mutex<Vec<Shutdown>>>,
    }

    impl Fake {
        fn spawn(root: PathBuf) -> (Link, Fake) {
            let (to_daemon_r, to_daemon_w) = io::pipe().unwrap();
            let (from_daemon_r, from_daemon_w) = io::pipe().unwrap();
            let writer = Arc::new(Mutex::new(Some(from_daemon_w)));
            let closes = Arc::new(Mutex::new(Vec::new()));
            let (got_tx, got) = mpsc::channel();
            let (auto, served) = (Arc::clone(&writer), root.clone());
            thread::spawn(move || {
                let mut input = BufReader::new(to_daemon_r);
                let mut initialized = false;
                while let Ok(Some(frame)) = read_ndjson_frame(&mut input) {
                    let message: Value = serde_json::from_slice(&frame).unwrap();
                    if !initialized && message["method"] != "initialize" {
                        auto.lock().unwrap().take();
                        return;
                    }
                    initialized = true;
                    let reply = match message["method"].as_str() {
                        Some("initialize") => Some(json!({
                            "jsonrpc": "2.0",
                            "id": message["id"],
                            "result": { "instructions": format!("guidance for {}", served.display()) },
                        })),
                        Some("tools/call") if message["params"]["name"] == SELECT_PROJECT => {
                            let project = message["params"]["arguments"]["project"].as_str().unwrap();
                            let mut meta = serde_json::Map::new();
                            meta.insert(SWITCH_PROJECT_META.into(), json!({ "root": served.join(project) }));
                            Some(json!({
                                "jsonrpc": "2.0",
                                "id": message["id"],
                                "result": { "content": [], "_meta": meta },
                            }))
                        }
                        _ => None,
                    };
                    match reply {
                        Some(reply) => {
                            if let Some(out) = auto.lock().unwrap().as_mut() {
                                write_ndjson_frame(out, &serde_json::to_vec(&reply).unwrap()).unwrap();
                            }
                        }
                        None => {
                            if got_tx.send(message).is_err() {
                                return;
                            }
                        }
                    }
                }
            });
            let (recorded, closing) = (Arc::clone(&closes), Arc::clone(&writer));
            let link = Link {
                reader: Box::new(BufReader::new(from_daemon_r)),
                writer: Box::new(to_daemon_w),
                closer: Box::new(move |how| {
                    recorded.lock().unwrap().push(how);
                    closing.lock().unwrap().take();
                }),
            };
            (link, Fake { root, got, writer, closes })
        }

        /// The next frame this daemon received with `method`, skipping others.
        fn expect(&self, method: &str) -> Value {
            loop {
                let message = self.got.recv_timeout(WAIT).unwrap_or_else(|_| panic!("no {method} arrived"));
                if message["method"] == method {
                    return message;
                }
            }
        }

        fn answer(&self, id: u64) {
            self.reply(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "content": [{ "type": "text", "text": format!("answer from {}", self.root.display()) }] },
            }));
        }

        fn reply(&self, reply: Value) {
            if let Some(writer) = self.writer.lock().unwrap().as_mut() {
                write_ndjson_frame(writer, &serde_json::to_vec(&reply).unwrap()).unwrap();
            }
        }

        /// Ends the connection from the daemon's side.
        fn hang_up(&self) {
            self.writer.lock().unwrap().take();
        }

        fn closes(&self) -> Vec<Shutdown> {
            self.closes.lock().unwrap().clone()
        }
    }

    /// A session over a folder holding projects `a` and `b`, initialized.
    struct Session {
        root: PathBuf,
        client: io::PipeWriter,
        out: mpsc::Receiver<Value>,
        front: Fake,
        /// Every daemon the connector connected to, in order.
        daemons: mpsc::Receiver<Fake>,
        session: thread::JoinHandle<Result<()>>,
        _dir: tempfile::TempDir,
    }

    impl Session {
        fn start() -> Self {
            let mut session = Self::open();
            session.initialize();
            session
        }

        /// A session the client has not initialized yet.
        fn open() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            std::fs::create_dir(root.join("a")).unwrap();
            std::fs::create_dir(root.join("b")).unwrap();
            let (front_link, front) = Fake::spawn(root.clone());
            let (daemon_tx, daemons) = mpsc::channel();
            let daemon_tx = Mutex::new(daemon_tx);
            let connector: Connector = Box::new(move |path| {
                let (link, fake) = Fake::spawn(path.to_path_buf());
                daemon_tx.lock().unwrap().send(fake).unwrap();
                Ok(link)
            });
            let (client_r, client) = io::pipe().unwrap();
            let (out_r, out_w) = io::pipe().unwrap();
            let serve_root = root.clone();
            let session = thread::spawn(move || {
                serve(BufReader::new(client_r), out_w, front_link, serve_root, connector)
            });
            let (out_tx, out) = mpsc::channel();
            thread::spawn(move || {
                let mut reader = BufReader::new(out_r);
                while let Ok(Some(frame)) = read_ndjson_frame(&mut reader) {
                    if out_tx.send(serde_json::from_slice(&frame).unwrap()).is_err() {
                        return;
                    }
                }
            });
            Self { root, client, out, front, daemons, session, _dir: dir }
        }

        fn initialize(&mut self) {
            self.send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} }));
            assert_eq!(self.recv()["id"], 0);
            self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        }

        fn send(&mut self, message: Value) {
            write_ndjson_frame(&mut self.client, &serde_json::to_vec(&message).unwrap()).unwrap();
        }

        fn recv(&self) -> Value {
            self.out.recv_timeout(WAIT).expect("no frame reached the client within 2 s")
        }

        fn call(&mut self, id: u64) {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": "find_references", "arguments": {} },
            }));
        }

        /// Selects `project` and returns the text of the answer.
        fn select(&mut self, id: u64, project: &str) -> String {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": { "name": SELECT_PROJECT, "arguments": { "project": project } },
            }));
            let answer = self.recv();
            assert_eq!(answer["id"], id);
            assert!(answer["result"]["isError"].is_null(), "{answer}");
            answer["result"]["content"][0]["text"].as_str().unwrap().to_string()
        }

        fn daemon(&self) -> Fake {
            self.daemons.recv_timeout(WAIT).expect("the shim did not connect")
        }
    }

    /// Re-selecting the project already served is a full switch: a second
    /// connection, while the first stays open for the call in flight on it,
    /// whose late answer reaches the client.
    ///
    /// Control: restore `previous.close(Shutdown::Write)` at the switch and
    /// drop the pending-id tracking: the first connection is half-closed at
    /// once, and id 7's answer never arrives within 2 s.
    #[test]
    fn reselecting_the_served_project_keeps_a_call_in_flight() {
        let mut session = Session::start();
        session.select(1, "a");
        let first = session.daemon();
        session.call(7);
        assert_eq!(first.expect("tools/call")["id"], 7);

        session.select(2, "a");
        let second = session.daemon();
        assert_eq!(second.root, first.root, "a reselect reconnects to the same project");
        assert!(first.closes().is_empty(), "the first connection still owes 7");

        first.answer(7);
        let answer = session.recv();
        assert_eq!(answer["id"], 7, "{answer}");
        assert!(answer["result"]["isError"].is_null(), "{answer}");
        assert_eq!(first.closes(), vec![Shutdown::Write], "closed once it owes nothing");
    }

    /// A switch keeps the previous project's connection open while it owes
    /// answers, passes its late answers through, and when that connection
    /// ends with one still owed, the shim answers it with an error naming
    /// the switch.
    ///
    /// Control: restore `previous.close(Shutdown::Write)` at the switch and
    /// drop the pending-id tracking: the old connection is half-closed at
    /// once, and id 8 gets no answer within 2 s.
    #[test]
    fn a_switch_never_drops_a_call_in_flight() {
        let mut session = Session::start();
        session.select(1, "a");
        let a = session.daemon();
        session.call(7);
        session.call(8);
        assert_eq!(a.expect("tools/call")["id"], 7);
        assert_eq!(a.expect("tools/call")["id"], 8);

        session.select(2, "b");
        let b = session.daemon();
        assert!(a.closes().is_empty(), "a still owes 7 and 8, so it must not be closed");

        a.answer(7);
        let answer = session.recv();
        assert_eq!(answer["id"], 7, "{answer}");
        assert!(answer["result"]["content"][1]["text"].as_str().unwrap().contains("answer from"), "{answer}");
        assert!(a.closes().is_empty(), "a still owes 8");

        session.call(9);
        assert_eq!(b.expect("tools/call")["id"], 9, "calls after the switch go to b");

        a.hang_up();
        let answer = session.recv();
        assert_eq!(answer["id"], 8, "{answer}");
        assert_eq!(answer["result"]["isError"], true, "{answer}");
        let text = answer["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("the daemon serving {} ended", a.root.display())), "{text}");
        assert!(text.contains(&format!("(the session had switched to {})", b.root.display())), "{text}");
        assert!(text.contains("call the tool again"), "{text}");
    }

    /// The first text item of an answer, which a switched session's
    /// sub-project answers carry as the answering project's name.
    fn first_text(answer: &Value) -> &str {
        answer["result"]["content"][0]["text"].as_str().unwrap_or_else(|| panic!("no text item: {answer}"))
    }

    /// Two agents sharing one client connection share one selection: agent
    /// A selects `a`, agent B selects `b`, and A's next call is routed to
    /// `b`. The answer says so in its first text item, so A can tell and
    /// re-select. The JSON-RPC ids are the only thing telling the two agents
    /// apart here, exactly as on a real shared connection.
    ///
    /// Control: make `settle` return `None` always: id 11's first text item
    /// is `b`'s own answer, and the `answered from project b.` assertion
    /// fails.
    #[test]
    fn another_agents_select_is_named_in_this_agents_answer() {
        let mut session = Session::start();
        // Agent A.
        session.select(1, "a");
        let a = session.daemon();
        session.call(10);
        assert_eq!(a.expect("tools/call")["id"], 10);
        a.answer(10);
        let answer = session.recv();
        assert_eq!(answer["id"], 10);
        assert_eq!(first_text(&answer), "g-mesh: answered from project a.", "{answer}");

        // Agent B, on the same connection.
        let selected = session.select(2, "b");
        assert!(selected.contains("call select_project again"), "{selected}");
        let b = session.daemon();

        // Agent A again: its call goes to b, and the answer says so.
        session.call(11);
        assert_eq!(b.expect("tools/call")["id"], 11);
        b.answer(11);
        let answer = session.recv();
        assert_eq!(answer["id"], 11, "{answer}");
        assert_eq!(first_text(&answer), "g-mesh: answered from project b.", "{answer}");
        let served = answer["result"]["content"][1]["text"].as_str().unwrap();
        assert_eq!(served, format!("answer from {}", b.root.display()), "the answer itself is untouched");
    }

    /// The stamp names the daemon a call was sent to, not the selection when
    /// its answer arrives: a call sent to `a` and answered after a switch to
    /// `b` names `a`.
    ///
    /// Control: in `settle`, take the stamp's root from `router.current()`
    /// instead of `router.upstream(upstream)`: id 7 is stamped `b`.
    #[test]
    fn the_stamp_names_the_project_a_call_was_sent_to() {
        let mut session = Session::start();
        session.select(1, "a");
        let a = session.daemon();
        session.call(7);
        assert_eq!(a.expect("tools/call")["id"], 7);
        session.select(2, "b");
        let _b = session.daemon();

        a.answer(7);
        let answer = session.recv();
        assert_eq!(answer["id"], 7, "{answer}");
        assert_eq!(first_text(&answer), "g-mesh: answered from project a.", "{answer}");
    }

    /// No stamp before a switch, and none on front answers after one: a call
    /// answered by the front, then `tools/list` after the switch, cross as
    /// the daemon sent them; only the sub-project's `tools/call` answer is
    /// stamped, and another method's answer from the sub-project is not.
    ///
    /// Controls: drop the `*served != router.root` condition in `settle`: id
    /// 7, answered by the front before any switch, is stamped. Drop the
    /// `in_flight.method == "tools/call"` condition: id 10 is stamped.
    #[test]
    fn only_sub_project_tool_answers_are_stamped() {
        let mut session = Session::start();
        session.call(7);
        assert_eq!(session.front.expect("tools/call")["id"], 7);
        session.front.answer(7);
        let answer = session.recv();
        assert_eq!(answer["id"], 7, "{answer}");
        assert_eq!(first_text(&answer), format!("answer from {}", session.root.display()), "{answer}");

        session.select(1, "a");
        let a = session.daemon();
        session.send(json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/list" }));
        assert_eq!(session.front.expect("tools/list")["id"], 8);
        let listed = json!({ "jsonrpc": "2.0", "id": 8, "result": { "tools": [] } });
        session.front.reply(listed.clone());
        assert_eq!(session.recv(), listed);

        session.call(9);
        assert_eq!(a.expect("tools/call")["id"], 9);
        a.answer(9);
        assert_eq!(first_text(&session.recv()), "g-mesh: answered from project a.");

        // Any other method the sub-project answers crosses unchanged, even
        // when its result happens to carry a `content` array.
        session.send(json!({ "jsonrpc": "2.0", "id": 10, "method": "resources/read", "params": {} }));
        assert_eq!(a.expect("resources/read")["id"], 10);
        let read =
            json!({ "jsonrpc": "2.0", "id": 10, "result": { "content": [{ "type": "text", "text": "r" }] } });
        a.reply(read.clone());
        assert_eq!(session.recv(), read);
    }

    /// A tool error from a sub-project is stamped like a success; a JSON-RPC
    /// error response, which has no content to carry it, crosses unchanged.
    /// The project's name is relative to the shim's root, `/`-separated.
    ///
    /// Control: in `stamped`, return `frame` unchanged when
    /// `result.isError` is true: id 7's first text item is the error text.
    #[test]
    fn tool_errors_are_stamped_and_protocol_errors_are_not() {
        let mut session = Session::start();
        std::fs::create_dir_all(session.root.join("nested").join("inner")).unwrap();
        session.select(1, "nested/inner");
        let inner = session.daemon();
        session.call(7);
        session.call(8);
        assert_eq!(inner.expect("tools/call")["id"], 7);
        assert_eq!(inner.expect("tools/call")["id"], 8);
        inner.reply(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": { "content": [{ "type": "text", "text": "no such symbol" }], "isError": true },
        }));
        let answer = session.recv();
        assert_eq!(answer["id"], 7, "{answer}");
        assert_eq!(first_text(&answer), "g-mesh: answered from project nested/inner.", "{answer}");
        assert_eq!(answer["result"]["isError"], true, "{answer}");

        let error = json!({ "jsonrpc": "2.0", "id": 8, "error": { "code": -32602, "message": "bad" } });
        inner.reply(error.clone());
        assert_eq!(session.recv(), error);
    }

    /// A previous project's connection is half-closed once it owes nothing:
    /// a cancelled call is no longer owed, and its cancel goes to the daemon
    /// the call went to.
    ///
    /// Control: route `notifications/cancelled` to the current upstream and
    /// keep the cancelled id pending: the cancel reaches `b`, and `a` is
    /// never closed.
    #[test]
    fn a_retired_connection_closes_once_it_owes_nothing() {
        let mut session = Session::start();
        session.select(1, "a");
        let a = session.daemon();
        session.call(7);
        session.call(8);
        assert_eq!(a.expect("tools/call")["id"], 7);
        assert_eq!(a.expect("tools/call")["id"], 8);
        session.select(2, "b");
        let b = session.daemon();

        session.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": 8, "reason": "t" },
        }));
        assert_eq!(a.expect("notifications/cancelled")["params"]["requestId"], 8);
        assert!(a.closes().is_empty(), "a still owes 7");

        a.answer(7);
        assert_eq!(session.recv()["id"], 7);
        assert_eq!(a.closes(), vec![Shutdown::Write]);
        assert!(b.closes().is_empty());
        a.hang_up();
        assert!(
            session.out.recv_timeout(Duration::from_millis(300)).is_err(),
            "a owed nothing when it ended"
        );
    }

    /// The served daemon closing with a call pending gets that call answered
    /// before the session ends.
    ///
    /// Control: skip answering owed ids in `upstream_ended`: the session
    /// ends with no frame for id 7.
    #[test]
    fn a_daemon_that_closes_with_a_call_pending_gets_it_answered() {
        let mut session = Session::start();
        session.call(7);
        assert_eq!(session.front.expect("tools/call")["id"], 7);
        session.front.hang_up();
        let answer = session.recv();
        assert_eq!(answer["id"], 7, "{answer}");
        assert_eq!(answer["result"]["isError"], true, "{answer}");
        let text = answer["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(&format!("serving {} ended", session.root.display())), "{text}");
        assert!(
            text.contains("after the call was sent. "),
            "no switch happened, so no cause is claimed: {text}"
        );
        assert!(text.contains("Its result, if any, was not received"), "{text}");
        session.session.join().unwrap().expect("the session ends when its daemon does");
    }

    /// A request before `initialize` is refused by the shim with -32601 and
    /// the same id, a ping is answered, a notification is dropped, and none
    /// of them reaches the daemon: the session then initializes and serves
    /// as usual.
    ///
    /// Control: remove the `answered_before_session` early return in
    /// `on_client_frame`: the frames are forwarded, the daemon ends the
    /// connection, and the probe is answered -32603 instead.
    #[test]
    fn a_request_before_initialize_is_refused_and_the_session_continues() {
        let mut session = Session::open();
        session.send(json!({ "jsonrpc": "2.0", "method": "notifications/early" }));
        session.send(json!({
            "jsonrpc": "2.0",
            "id": "server-discover-probe-1",
            "method": "server/discover",
            "params": {},
        }));
        let refused = session.recv();
        assert_eq!(refused["id"], "server-discover-probe-1");
        assert_eq!(refused["error"]["code"], -32601, "{refused}");
        session.send(json!({ "jsonrpc": "2.0", "id": 5, "method": "ping" }));
        let pong = session.recv();
        assert_eq!((&pong["id"], &pong["result"]), (&json!(5), &json!({})), "{pong}");

        session.initialize();
        session.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
        let early: Vec<Value> = std::iter::from_fn(|| session.front.got.recv_timeout(WAIT).ok())
            .take_while(|message| message["method"] != "tools/list")
            .filter(|message| message["method"] != "notifications/initialized")
            .collect();
        assert!(early.is_empty(), "the daemon got frames sent before initialize: {early:?}");
        session.front.reply(json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } }));
        let listed = session.recv();
        assert_eq!((&listed["id"], &listed["result"]["tools"]), (&json!(1), &json!([])), "{listed}");
    }
}
