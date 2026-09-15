use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;

const CONTENT_LENGTH: &str = "Content-Length";

/// A misbehaving plugin must not be able to make core allocate an arbitrary
/// buffer by announcing a huge body. Control-plane messages are small - bulk
/// graph data travels over NDJSON, not this transport.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(body.len() + 32);
    frame.extend_from_slice(format!("{CONTENT_LENGTH}: {}\r\n\r\n", body.len()).as_bytes());
    frame.extend_from_slice(body);

    writer.write_all(&frame).context("failed to write frame")?;
    writer.flush().context("failed to flush frame")?;
    Ok(())
}

pub fn write_message<T: Serialize + ?Sized, W: Write>(writer: &mut W, message: &T) -> Result<()> {
    let body = serde_json::to_vec(message).context("failed to serialize frame body")?;
    write_frame(writer, &body)
}

/// Reads exactly one frame, returning `None` on a clean EOF at a frame
/// boundary (the peer closed the stream between messages). Takes a `BufRead`
/// because a frame is not guaranteed to arrive in a single `read` - the same
/// reader must be reused across calls so leftover bytes are not lost.
/// An `Err` leaves the stream desynchronised: the connection is dead, not
/// resumable.
pub fn read_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let Some(len) = read_content_length(reader)? else {
        return Ok(None);
    };

    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).with_context(|| format!("failed to read {len}-byte frame body"))?;
    Ok(Some(body))
}

pub fn read_message<T: DeserializeOwned, R: BufRead>(reader: &mut R) -> Result<Option<T>> {
    let Some(body) = read_frame(reader)? else {
        return Ok(None);
    };
    let message = serde_json::from_slice(&body).context("failed to deserialize frame body")?;
    Ok(Some(message))
}

/// Marks a [`read_message_with_timeout`] failure specifically caused by its
/// deadline elapsing (or, treated the same way, its reader thread ending
/// without answering at all - see that function's doc comment on why the two
/// are folded together) - as opposed to a framing error, a malformed body, or
/// a clean EOF discovered *before* the deadline, all of which stay ordinary
/// `anyhow::Error`s with no marker.
///
/// The distinction matters one layer up: `daemon::plugin::PluginProcess
/// ::apply_file_change` already knows how to recover from an ordinary crash
/// (relaunch, then blindly replay whatever was pending - safe, because a dead
/// process never got a chance to act on the request at all). A timeout is not
/// that safe to treat the same way: the plugin may still be mid-write on the
/// very request that just timed out, so resending it blind into a freshly
/// spawned process risks a duplicate side effect for whatever that request
/// was doing. That function downcasts for this marker (via [`is_timeout`], to
/// survive any `.context()` layered on top by the callers in between) to
/// choose the safer path: relaunch, but leave the request for the caller's
/// own dirty-queue replay instead of resending it here.
#[derive(Debug)]
pub struct RoundTripTimedOut {
    pub timeout: Duration,
}

impl std::fmt::Display for RoundTripTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no response within {:?}", self.timeout)
    }
}

impl std::error::Error for RoundTripTimedOut {}

/// Whether `err` is, anywhere in its `.context()` chain, a
/// [`RoundTripTimedOut`] - see that type's doc comment for why callers need
/// to tell a timeout apart from every other round-trip failure.
pub fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.downcast_ref::<RoundTripTimedOut>().is_some())
}

/// [`read_message`], but gives up after `timeout` if the plugin has not
/// answered.
///
/// # Why a reader thread rather than a read with a deadline
///
/// A blocking `read`/`read_exact` on a pipe - a spawned plugin's `stdout`, or
/// the `std::io::pipe()` peers this module's own tests use - has no deadline
/// of its own: the underlying syscall blocks until bytes arrive or the pipe
/// closes, however wedged the process on the other end is. Neither `Read` nor
/// `BufRead` exposes a way to attach one, and there is no portable
/// alternative that works across a real child process's pipe and a test's
/// bare `std::io::pipe()` alike.
///
/// So the read is moved onto its own thread, and this function waits on *that*
/// with [`mpsc::Receiver::recv_timeout`], which does have a deadline. On a
/// timeout, `on_timeout` is called - for a real plugin,
/// `daemon::plugin::PluginProcess` kills the child process there, which
/// closes its stdout and is what actually unblocks the read (with an error or
/// an EOF); this module deliberately knows nothing about child processes, to
/// stay usable against the plain pipes its own tests fake a peer with (see
/// this module's own tests and `watcher::apply`'s).
///
/// [`std::thread::scope`] is what makes borrowing `reader` here possible
/// without demanding `'static` ownership of it, and - just as importantly -
/// is what keeps this function from ever leaking that thread: `scope` does
/// not return until every thread spawned inside it has finished, so even
/// after `on_timeout` has already been called and this function is about to
/// report failure, the actual `read_message` call below is still running
/// somewhere, and this function's caller does not get its result back until
/// that thread has actually ended - which, once `on_timeout` has closed the
/// pipe, is a matter of the OS delivering EOF or an error, not an open-ended
/// wait. Net effect: this call returns in `timeout` plus however long that
/// cleanup takes, never returns while the thread is still running, and never
/// abandons it running in the background.
pub fn read_message_with_timeout<T, R>(
    reader: &mut R,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<Option<T>>
where
    T: DeserializeOwned + Send,
    R: BufRead + Send,
{
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // The receiver may already have given up by the time this sends
            // (the timeout branch below) - nothing is left to tell, and that
            // is not this thread's problem to report.
            let _ = tx.send(read_message::<T, _>(reader));
        });

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(_) => {
                on_timeout();
                Err(anyhow::Error::new(RoundTripTimedOut { timeout }))
            }
        }
    })
}

fn read_content_length<R: BufRead>(reader: &mut R) -> Result<Option<usize>> {
    let mut content_length = None;
    let mut started = false;

    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).context("failed to read frame header")?;
        if read == 0 {
            if !started {
                return Ok(None);
            }
            bail!("unexpected EOF inside frame header");
        }
        started = true;

        let line = std::str::from_utf8(&line)
            .context("frame header is not valid UTF-8")?
            .trim_end_matches('\n')
            .trim_end_matches('\r');
        if line.is_empty() {
            break;
        }

        let (name, value) =
            line.split_once(':').ok_or_else(|| anyhow!("malformed frame header line: {line:?}"))?;
        if name.trim().eq_ignore_ascii_case(CONTENT_LENGTH) {
            let value = value.trim();
            let len =
                value.parse::<usize>().with_context(|| format!("invalid Content-Length value: {value:?}"))?;
            content_length = Some(len);
        }
    }

    let len = content_length.context("frame header is missing Content-Length")?;
    if len > MAX_BODY_BYTES {
        bail!("frame body of {len} bytes exceeds the {MAX_BODY_BYTES}-byte limit");
    }
    Ok(Some(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{ControlEnvelope, ControlMessage, RequestId, JSONRPC_VERSION};
    use std::io::{BufReader, Cursor, Read};
    use std::sync::Mutex;
    use std::time::Instant;

    fn request() -> ControlEnvelope {
        ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: Some(RequestId::Number(7)),
            message: ControlMessage::Reindex { file_path: "src/lib.rs".to_string() },
        }
    }

    fn notification() -> ControlEnvelope {
        ControlEnvelope {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: None,
            message: ControlMessage::FileChanged { file_path: "src/main.rs".to_string() },
        }
    }

    fn framed(message: &ControlEnvelope) -> Vec<u8> {
        let mut buf = Vec::new();
        write_message(&mut buf, message).unwrap();
        buf
    }

    /// Hands back at most `chunk` bytes per `read` call, so the framer has to
    /// buffer across calls instead of seeing a whole frame at once.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        reads: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunk: usize) -> Self {
            Self { data, pos: 0, chunk, reads: 0 }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            let remaining = &self.data[self.pos..];
            let n = remaining.len().min(buf.len()).min(self.chunk);
            buf[..n].copy_from_slice(&remaining[..n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn message_round_trips_through_a_pipe() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let sent = request();

        write_message(&mut writer, &sent).unwrap();
        drop(writer);

        let mut reader = BufReader::new(reader);
        let received: ControlEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(sent, received);
        assert!(
            read_message::<ControlEnvelope, _>(&mut reader).unwrap().is_none(),
            "clean EOF at a frame boundary is not an error"
        );
    }

    #[test]
    fn frame_split_across_reads_is_reassembled() {
        // 3 bytes per read splits both the Content-Length header and the body.
        let inner = ChunkedReader::new(framed(&request()), 3);
        let mut reader = BufReader::with_capacity(8, inner);

        let received: ControlEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(request(), received);
        assert!(reader.get_ref().reads > 1, "test must actually exercise partial reads");
    }

    #[test]
    fn frame_arriving_in_pieces_over_a_pipe_is_reassembled() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let bytes = framed(&notification());
        let (head, rest) = bytes.split_at(10);
        let (middle, tail) = rest.split_at(rest.len() / 2);
        let (head, middle, tail) = (head.to_vec(), middle.to_vec(), tail.to_vec());

        let sender = std::thread::spawn(move || {
            for piece in [head, middle, tail] {
                writer.write_all(&piece).unwrap();
                writer.flush().unwrap();
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });

        let mut reader = BufReader::new(reader);
        let received: ControlEnvelope = read_message(&mut reader).unwrap().unwrap();
        sender.join().unwrap();
        assert_eq!(notification(), received);
    }

    #[test]
    fn consecutive_frames_are_read_in_order() {
        let mut stream = framed(&request());
        stream.extend_from_slice(&framed(&notification()));
        let mut reader = BufReader::new(Cursor::new(stream));

        let first: ControlEnvelope = read_message(&mut reader).unwrap().unwrap();
        let second: ControlEnvelope = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(request(), first);
        assert_eq!(notification(), second);
        assert!(read_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn written_frame_uses_lsp_wire_format() {
        let mut buf = Vec::new();
        write_frame(&mut buf, br#"{"ok":true}"#).unwrap();
        assert_eq!(buf, b"Content-Length: 11\r\n\r\n{\"ok\":true}");
    }

    #[test]
    fn headers_other_than_content_length_are_ignored() {
        let raw = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}";
        let mut reader = BufReader::new(Cursor::new(raw.to_vec()));
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"{}");
    }

    #[test]
    fn malformed_frames_are_errors_not_panics() {
        let cases: &[&[u8]] = &[
            b"\r\n{}",                                // no Content-Length
            b"Content-Length: nope\r\n\r\n{}",        // unparsable length
            b"Content-Length 2\r\n\r\n{}",            // header without a colon
            b"Content-Length: 64\r\n\r\n{}",          // body shorter than announced
            b"Content-Length: 99999999999\r\n\r\n{}", // body over the size limit
            b"Content-Length: 2\r\n",                 // EOF inside the header block
        ];

        for case in cases {
            let mut reader = BufReader::new(Cursor::new(case.to_vec()));
            assert!(
                read_frame(&mut reader).is_err(),
                "expected a framing error for {:?}",
                String::from_utf8_lossy(case)
            );
        }
    }

    #[test]
    fn non_json_body_fails_deserialization_only() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"not json").unwrap();

        let mut reader = BufReader::new(Cursor::new(buf.clone()));
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"not json");

        let mut reader = BufReader::new(Cursor::new(buf));
        assert!(read_message::<ControlEnvelope, _>(&mut reader).is_err());
    }

    /// The mechanism this whole module exists to add: a peer that never
    /// writes anything must not block the caller past `timeout`, and the
    /// deadline must actually be enforced, not just documented - this is the
    /// discriminating half of the acceptance test in
    /// `daemon::lifecycle`/`daemon::plugin`, cut down to the primitive itself
    /// so it runs in milliseconds instead of spawning a real plugin process.
    ///
    /// `on_timeout` here drops the writer end of the pipe - the pipe-level
    /// equivalent of `PluginProcess` killing the child - which is what lets
    /// the abandoned reader thread actually finish (EOF) instead of staying
    /// blocked forever; `join` on the spawning thread (inside
    /// `read_message_with_timeout`'s own `thread::scope`) proves it did.
    #[test]
    fn a_peer_that_never_answers_times_out_instead_of_blocking_forever() {
        let (reader, writer) = std::io::pipe().unwrap();
        let mut reader = BufReader::new(reader);
        let writer = Mutex::new(Some(writer));

        let start = Instant::now();
        let result = read_message_with_timeout::<ControlEnvelope, _>(
            &mut reader,
            Duration::from_millis(50),
            &mut || {
                // Dropping the writer closes the pipe, which is what
                // unblocks the reader thread's blocked `read`.
                writer.lock().unwrap().take();
            },
        );
        let elapsed = start.elapsed();

        let err = result.expect_err("a peer that never answers must be reported as a failure");
        assert!(is_timeout(&err), "the failure must be recognizable as a timeout: {err:#}");
        assert!(
            elapsed < Duration::from_secs(5),
            "must not block far past its 50ms timeout - took {elapsed:?}"
        );
    }

    /// The other half of the same claim: a peer that answers before the
    /// deadline is unaffected by this function existing at all - `on_timeout`
    /// must never fire on the ordinary path.
    #[test]
    fn a_peer_that_answers_in_time_is_unaffected() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut reader = BufReader::new(reader);
        let sent = request();
        write_message(&mut writer, &sent).unwrap();

        let fired = std::sync::atomic::AtomicBool::new(false);
        let received: ControlEnvelope =
            read_message_with_timeout(&mut reader, Duration::from_secs(5), &mut || {
                fired.store(true, std::sync::atomic::Ordering::SeqCst)
            })
            .unwrap()
            .unwrap();

        assert_eq!(sent, received);
        assert!(
            !fired.load(std::sync::atomic::Ordering::SeqCst),
            "on_timeout must not fire on a timely answer"
        );
    }
}
