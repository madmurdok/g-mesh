//! Newline-delimited framing for the MCP wire, kept deliberately separate from
//! `jsonrpc`'s Content-Length framing.
//!
//! Two different JSON-RPC transports meet in this process and they do *not*
//! share a framing: the core<->plugin control plane is LSP-style
//! (`Content-Length` headers, see `jsonrpc.rs`), while MCP's stdio transport
//! delimits messages with `\n` and forbids embedded newlines inside a message.
//! The shim proxies the MCP side, so it has to speak this one; conflating the
//! two is exactly the bug this module exists to prevent.
//!
//! Like `jsonrpc`, these functions work on raw bodies rather than parsed
//! messages - the shim is a repacking proxy and never inspects the JSON.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};

/// A peer that never sends a newline must not be able to make this process
/// buffer without bound. MCP messages are single JSON documents; the same
/// generous ceiling `jsonrpc` uses is far above anything legitimate.
const MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Writes one message followed by its `\n` delimiter.
///
/// `body` is assumed newline-free, which holds for everything this is used
/// with: it is either compact serialized JSON (which escapes newlines inside
/// strings) or a body that `read_ndjson_frame` just decoded from a single
/// line. There is no re-escaping step here - a proxy that rewrote payloads
/// would no longer be a proxy.
pub fn write_ndjson_frame<W: Write>(writer: &mut W, body: &[u8]) -> Result<()> {
    let mut frame = Vec::with_capacity(body.len() + 1);
    frame.extend_from_slice(body);
    frame.push(b'\n');

    writer.write_all(&frame).context("failed to write frame")?;
    writer.flush().context("failed to flush frame")?;
    Ok(())
}

/// Reads exactly one message, returning `None` on a clean EOF at a message
/// boundary. A trailing `\r` is stripped so a CRLF-terminated peer round trips
/// unchanged.
///
/// Takes a `BufRead` for the same reason `jsonrpc::read_frame` does: a message
/// is not guaranteed to arrive in one `read`, so the same reader has to be
/// reused across calls or buffered bytes are lost.
///
/// A final line with no terminating newline is returned rather than rejected -
/// a peer that closes right after its last message is being terse, not
/// malformed, and dropping that message would silently lose a reply.
pub fn read_ndjson_frame<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();

    loop {
        // fill_buf/consume rather than `read_until`, so the size guard can
        // trip *while* an oversized line is arriving instead of after the
        // allocation it is supposed to prevent.
        let available = reader.fill_buf().context("failed to read frame")?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break; // EOF right after an unterminated final message
        }

        match available.iter().position(|byte| *byte == b'\n') {
            Some(end) => {
                line.extend_from_slice(&available[..end]);
                reader.consume(end + 1);
                break;
            }
            None => {
                let consumed = available.len();
                line.extend_from_slice(available);
                reader.consume(consumed);
            }
        }

        if line.len() > MAX_LINE_BYTES {
            bail!("frame of over {MAX_LINE_BYTES} bytes has no newline terminator");
        }
    }

    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    const REQUEST: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    const RESPONSE: &[u8] = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;

    /// Hands back at most `chunk` bytes per `read` call, so the framer has to
    /// buffer across calls instead of seeing a whole message at once.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
        reads: usize,
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
    fn frame_round_trips_through_a_pipe() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        write_ndjson_frame(&mut writer, REQUEST).unwrap();
        drop(writer);

        let mut reader = BufReader::new(reader);
        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), REQUEST);
        assert!(
            read_ndjson_frame(&mut reader).unwrap().is_none(),
            "clean EOF at a frame boundary is not an error"
        );
    }

    #[test]
    fn written_frame_is_the_body_plus_one_newline() {
        let mut buf = Vec::new();
        write_ndjson_frame(&mut buf, br#"{"ok":true}"#).unwrap();
        assert_eq!(buf, b"{\"ok\":true}\n");
    }

    #[test]
    fn frame_split_across_reads_is_reassembled() {
        let inner = ChunkedReader { data: [REQUEST, b"\n"].concat(), pos: 0, chunk: 3, reads: 0 };
        let mut reader = BufReader::with_capacity(8, inner);

        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), REQUEST);
        assert!(reader.get_ref().reads > 1, "test must actually exercise partial reads");
    }

    #[test]
    fn consecutive_frames_are_read_in_order() {
        let mut stream = Vec::new();
        write_ndjson_frame(&mut stream, REQUEST).unwrap();
        write_ndjson_frame(&mut stream, RESPONSE).unwrap();
        let mut reader = BufReader::new(Cursor::new(stream));

        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), REQUEST);
        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), RESPONSE);
        assert!(read_ndjson_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn crlf_terminator_is_accepted() {
        let mut reader = BufReader::new(Cursor::new(b"{\"ok\":true}\r\n".to_vec()));
        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), br#"{"ok":true}"#);
    }

    #[test]
    fn unterminated_final_frame_is_still_delivered() {
        let mut reader = BufReader::new(Cursor::new(REQUEST.to_vec()));
        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), REQUEST);
        assert!(read_ndjson_frame(&mut reader).unwrap().is_none());
    }

    #[test]
    fn non_json_body_is_passed_through_verbatim() {
        // Framing and parsing are separate concerns: the shim proxies bodies
        // it never validates, so a garbage line is a frame, not an error.
        let mut reader = BufReader::new(Cursor::new(b"not json\n".to_vec()));
        assert_eq!(read_ndjson_frame(&mut reader).unwrap().unwrap(), b"not json");
    }

    #[test]
    fn an_endless_line_is_an_error_not_an_endless_allocation() {
        struct Endless;
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }

        let mut reader = BufReader::with_capacity(64 * 1024, Endless);
        assert!(read_ndjson_frame(&mut reader).is_err());
    }
}
