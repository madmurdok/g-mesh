use std::io::{BufRead, Write};

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
}
