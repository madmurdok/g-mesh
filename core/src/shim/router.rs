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
//! Upstream frames are parsed only on the front's connection, and only while
//! a `select_project` call is outstanding, so a sub-project daemon's (often
//! large) tool results are never parsed at all. A single-project session
//! never has a switch directive to act on, so for it every frame crosses
//! byte-for-byte, as it did before this module existed; a frame that fails to
//! parse is forwarded raw.
//!
//! # Who writes stdout
//!
//! Only the thread running [`serve`]. Every upstream reader hands it whole
//! frames through one channel, so two readers' frames cannot interleave: that
//! holds by construction rather than by locking discipline.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
use std::thread;

use anyhow::{bail, Context, Result};
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
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    closer: Arc<dyn Fn(Shutdown) + Send + Sync>,
}

impl Upstream {
    fn new(id: u64, writer: Box<dyn Write + Send>, closer: Box<dyn Fn(Shutdown) + Send + Sync>) -> Self {
        Self { id, writer: Arc::new(Mutex::new(writer)), closer: Arc::from(closer) }
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
    init_frame: Option<Vec<u8>>,
    initialized_frame: Option<Vec<u8>>,
    /// Ids of `select_project` calls sent to the front and not yet answered.
    select_ids: HashSet<Value>,
    replay_seq: u64,
    next_upstream: u64,
}

impl Router {
    fn current(&self) -> Option<&Upstream> {
        self.sub.as_ref().or(self.front.as_ref())
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
    Other,
}

fn classify(frame: &[u8]) -> ClientFrame {
    let Ok(message) = serde_json::from_slice::<Value>(frame) else {
        return ClientFrame::Other;
    };
    match message.get("method").and_then(Value::as_str) {
        Some("initialize") => ClientFrame::Initialize,
        Some("notifications/initialized") => ClientFrame::Initialized,
        Some("tools/list") => ClientFrame::ToolsList,
        Some("tools/call") => {
            let name = message.get("params").and_then(|params| params.get("name")).and_then(Value::as_str);
            match (name, message.get("id")) {
                (Some(SELECT_PROJECT), Some(id)) => ClientFrame::SelectProject(id.clone()),
                _ => ClientFrame::Other,
            }
        }
        _ => ClientFrame::Other,
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
            root,
            front: Some(Upstream::new(0, writer, closer)),
            sub: None,
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
                Ok(Some(frame)) => {
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
        for upstream in [&router.front, &router.sub].into_iter().flatten() {
            upstream.close(how);
        }
    }

    fn on_client_frame(&self, frame: Vec<u8>) {
        let target = {
            let mut router = self.lock();
            match classify(&frame) {
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
                ClientFrame::Other => router.current().cloned(),
            }
        };
        if let Some(upstream) = target {
            if let Err(err) = upstream.send(&frame) {
                // Its reader ends too, and decides whether the session does.
                eprintln!("g-mesh mcp-shim: could not forward a frame to the daemon: {err:#}");
            }
        }
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
                    "g-mesh: this session now serves {served}; file paths are relative to it. Guidance for \
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
        if let Some(previous) = router.sub.replace(Upstream::new(id, writer, closer)) {
            // Its reader drains the replies still in flight, then ends.
            previous.close(Shutdown::Write);
        }
        Ok((target, guidance))
    }

    fn upstream_ended(&self, id: u64) {
        let mut router = self.lock();
        let current = router.current().map(|upstream| upstream.id);
        if router.front.as_ref().map(|front| front.id) == Some(id) {
            router.front = None;
            // Nobody is left to answer these; say so rather than leave the
            // client waiting.
            let message = format!(
                "g-mesh: the connection to the daemon serving {} ended before it answered.",
                router.root.display()
            );
            for pending in router.select_ids.drain().collect::<Vec<_>>() {
                let _ = self.events.send(Event::Frame(error_result(&pending, &message)));
            }
        }
        if router.sub.as_ref().map(|sub| sub.id) == Some(id) {
            router.sub = None;
        }
        if current == Some(id) {
            let _ = self.events.send(Event::Done);
        }
    }
}

fn error_result(id: &Value, text: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "content": [{ "type": "text", "text": text }], "isError": true },
    }))
    .expect("a Value always serializes")
}

#[cfg(test)]
mod tests {
    use std::io::{self, BufReader, Read};

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
}
