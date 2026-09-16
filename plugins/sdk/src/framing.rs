//! `Content-Length` framing: one header, a blank line, a JSON body.
//!
//! # Why this is its own module
//!
//! A plugin speaks this wire in two directions, and until GM-289 only one of
//! them existed here. Upwards it answers core's control plane
//! ([`run`](crate::run)), whose framing is core's `protocol::jsonrpc`.
//! Downwards - for a plugin whose semantic tier drives a language server
//! ([`lsp`](crate::lsp)) - it *is* the LSP client, and LSP's base protocol is
//! the same header, blank line and body, because core's control plane was
//! modelled on it in the first place.
//!
//! Two copies of a byte format is how two copies drift, and a drift here is
//! not a compile error in either direction: it is a stream that desynchronizes
//! at run time, against a peer nobody in this repository controls. So the
//! format lives once, in functions that know nothing about who is on the other
//! end.
//!
//! What this module deliberately does **not** own is the JSON inside the body.
//! Core's control plane and LSP are both JSON-RPC 2.0 but they are not the
//! same protocol, and a shared "message" type would have to be the union of
//! two vocabularies that have no reason to converge.
//!
//! It is still not shared with *core*: this crate does not depend on core (see
//! this crate's own module doc), so core's `protocol::jsonrpc` remains a
//! separate implementation of the same format. What has to match between those
//! two is the format, not the code - and the conformance kit is what catches a
//! divergence, because a plugin whose frames core cannot read fails at the
//! handshake.

use std::io::{self, BufRead, Write};

/// A misbehaving peer must not be able to make this process allocate an
/// arbitrary buffer by announcing a huge body. The same limit core applies in
/// the other direction: control messages are small, bulk data does not travel
/// here, and a language server's own answers - a list of locations - are
/// smaller still.
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Writes one message as a frame and flushes it.
///
/// The flush is not optional: both peers block reading the answer to what was
/// just written, so a message left in a `BufWriter` is a deadlock with a
/// timeout on it.
pub(crate) fn write_message<T: serde::Serialize + ?Sized, W: Write>(
    out: &mut W,
    message: &T,
) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    out.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())?;
    out.write_all(&body)?;
    out.flush()
}

/// Reads one frame, or `None` at a clean EOF on a frame boundary.
///
/// Headers other than `Content-Length` are ignored rather than rejected: LSP
/// servers send `Content-Type`, and a header this side does not know is not a
/// reason to stop reading a stream it can otherwise parse exactly.
pub(crate) fn read_frame<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    let mut length: Option<usize> = None;
    let mut started = false;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            anyhow::ensure!(!started, "unexpected end of input inside a frame header");
            return Ok(None);
        }
        started = true;
        let line = std::str::from_utf8(&line)?.trim_end_matches('\n').trim_end_matches('\r');
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("Content-Length") {
                length = Some(value.trim().parse()?);
            }
        }
    }

    let length = length.ok_or_else(|| anyhow::anyhow!("frame header is missing Content-Length"))?;
    anyhow::ensure!(length <= MAX_BODY_BYTES, "frame body of {length} bytes exceeds the limit");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    #[test]
    fn a_written_frame_is_the_lsp_wire_format_core_reads() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &serde_json::json!({ "ok": true })).unwrap();
        assert_eq!(buffer, b"Content-Length: 11\r\n\r\n{\"ok\":true}");
    }

    #[test]
    fn frames_round_trip_in_order_and_end_at_a_clean_eof() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &serde_json::json!({ "a": 1 })).unwrap();
        write_message(&mut buffer, &serde_json::json!({ "b": 2 })).unwrap();

        let mut reader = BufReader::new(Cursor::new(buffer));
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), br#"{"a":1}"#);
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), br#"{"b":2}"#);
        assert!(read_frame(&mut reader).unwrap().is_none(), "EOF on a frame boundary is not an error");
    }

    #[test]
    fn headers_other_than_content_length_are_ignored() {
        let raw = b"Content-Type: application/vscode-jsonrpc\r\nContent-Length: 2\r\n\r\n{}".to_vec();
        let mut reader = BufReader::new(Cursor::new(raw));
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), b"{}");
    }

    #[test]
    fn malformed_framing_is_an_error_rather_than_a_guess() {
        let cases: &[&[u8]] = &[
            b"\r\n{}",                                // no Content-Length
            b"Content-Length: nope\r\n\r\n{}",        // unparsable length
            b"Content-Length: 64\r\n\r\n{}",          // body shorter than announced
            b"Content-Length: 99999999999\r\n\r\n{}", // over the size limit
            b"Content-Length: 2\r\n",                 // EOF inside the header block
        ];
        for case in cases {
            let mut reader = BufReader::new(Cursor::new(case.to_vec()));
            assert!(read_frame(&mut reader).is_err(), "expected a framing error for {case:?}");
        }
    }
}
