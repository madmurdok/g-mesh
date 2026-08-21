use std::io::{BufReader, Cursor};

use crate::protocol::jsonrpc::read_message;
use crate::protocol::ndjson::NdjsonReader;
use crate::protocol::types::ControlEnvelope;

#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub context: String,
    pub message: String,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ConformanceReport {
    pub violations: Vec<Violation>,
}

impl ConformanceReport {
    pub fn is_conformant(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Validates a plugin's bulk-transfer output: every non-blank line must be a
/// well-formed `WireNode`/`WireEdge`. serde already rejects unknown
/// `kind`/`source` values as part of ordinary deserialization, so there's
/// no separate edge-kind allow-list to maintain here - an invalid kind
/// simply fails to parse as either type.
pub fn check_bulk_output(ndjson: &[u8]) -> ConformanceReport {
    let reader = NdjsonReader::new(BufReader::new(Cursor::new(ndjson.to_vec())));
    let violations = reader
        .enumerate()
        .filter_map(|(i, result)| {
            result
                .err()
                .map(|e| Violation { context: format!("NDJSON line {}", i + 1), message: e.to_string() })
        })
        .collect();
    ConformanceReport { violations }
}

/// Validates a plugin's control-plane output: every frame must be valid
/// LSP-style framing carrying a valid `ControlEnvelope`. A framing error
/// desynchronizes the byte stream (see `jsonrpc::read_frame`), so checking
/// stops at the first one - anything after it is unverifiable, not
/// necessarily conformant.
pub fn check_control_plane_output(frames: &[u8]) -> ConformanceReport {
    let mut reader = BufReader::new(Cursor::new(frames.to_vec()));
    let mut violations = Vec::new();
    let mut frame_no = 0;
    loop {
        frame_no += 1;
        match read_message::<ControlEnvelope, _>(&mut reader) {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(e) => {
                violations.push(Violation {
                    context: format!("JSON-RPC frame {frame_no}"),
                    message: e.to_string(),
                });
                break;
            }
        }
    }
    ConformanceReport { violations }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_bulk_output_is_conformant() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Function\",\"name\":\"foo\",\"qualifiedName\":\"m::foo\",\"filePath\":\"a.rs\",\"range\":{\"start\":{\"line\":1,\"col\":0},\"end\":{\"line\":2,\"col\":0}},\"exported\":false,\"language\":\"rust\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(report.is_conformant(), "{:?}", report.violations);
    }

    #[test]
    fn invalid_edge_kind_is_a_violation() {
        let ndjson = b"{\"id\":\"e1\",\"fromId\":\"n1\",\"toId\":\"n2\",\"kind\":\"NOT_A_REAL_KIND\",\"source\":\"tree-sitter\",\"resolved\":false}\n";
        let report = check_bulk_output(ndjson);
        assert!(!report.is_conformant());
        assert_eq!(report.violations[0].context, "NDJSON line 1");
    }

    #[test]
    fn well_formed_control_frame_is_conformant() {
        let frame = b"Content-Length: 72\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"reindex\",\"params\":{\"filePath\":\"a.ts\"}}";
        let report = check_control_plane_output(frame);
        assert!(report.is_conformant(), "{:?}", report.violations);
    }

    #[test]
    fn broken_framing_is_a_violation() {
        let frame = b"Content-Length nope\r\n\r\n{}";
        let report = check_control_plane_output(frame);
        assert!(!report.is_conformant());
        assert_eq!(report.violations[0].context, "JSON-RPC frame 1");
    }
}
