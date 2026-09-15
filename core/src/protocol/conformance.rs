use std::io::{BufReader, Cursor};

use crate::graph::imports::RESOLVED_MODULE_NATIVE_KIND;
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::protocol::jsonrpc::read_message;
use crate::protocol::ndjson::{BulkItem, NdjsonReader};
use crate::protocol::types::{ControlEnvelope, WireNode};

/// The `nativeKind`s a `WireNode` stands in for something outside its own
/// file rather than declaring anything (mirrors the plugin's own
/// `PLACEHOLDER_NATIVE_KINDS` in plugins/typescript/src/extract.ts, minus
/// `external_module` - core never materializes a node for that one at all,
/// so it never reaches this check). Every one of these requires a `target`
/// once `WireNode::deserialize` has normalized legacy input - see the
/// `placeholder nativeKind requires a target` shape check below.
const PLACEHOLDER_NATIVE_KINDS: [&str; 3] =
    [PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND];

/// `nativeKind` of a logical-container node (Data Model > Logical
/// containers). Core, not a plugin, materializes these - a container has
/// members in many files, so no single file's diff can own it - so a plugin
/// ever emitting one is always a conformance violation, never a legitimate
/// message. Not yet a `pub const` anywhere in `graph`, because core does not
/// build container nodes itself yet; kept local until it does.
const CONTAINER_NATIVE_KIND: &str = "container";

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
/// well-formed `WireNode`/`WireEdge`, plus the shape rules parsing alone
/// cannot express. serde already rejects unknown `kind`/`source` values as
/// part of ordinary deserialization, so there's no separate edge-kind
/// allow-list to maintain here - an invalid kind simply fails to parse as
/// either type. What *does* parse still has two rules of its own, both
/// checked on the normalized (v2) node `WireNode::deserialize` already
/// produced - so legacy v1 input whose placeholder address derives cleanly
/// passes exactly like an equivalent v2 message would:
///
///  - a placeholder `nativeKind` (`pending_symbol`, `reexport`,
///    `resolved_module`) requires a `target` - present on the wire, or
///    derived from the legacy `<file>#<name>` convention; an underivable one
///    is reported here rather than surfacing as an opaque parse failure.
///  - `nativeKind: "container"` is never plugin-emitted - core alone
///    materializes container nodes (Data Model > Logical containers).
pub fn check_bulk_output(ndjson: &[u8]) -> ConformanceReport {
    let reader = NdjsonReader::new(BufReader::new(Cursor::new(ndjson.to_vec())));
    let mut violations = Vec::new();
    for (i, result) in reader.enumerate() {
        let context = format!("NDJSON line {}", i + 1);
        match result {
            Err(e) => violations.push(Violation { context, message: e.to_string() }),
            Ok(BulkItem::Node(node)) => violations.extend(node_shape_violations(&context, &node)),
            Ok(BulkItem::Edge(_)) => {}
        }
    }
    ConformanceReport { violations }
}

fn node_shape_violations(context: &str, node: &WireNode) -> Vec<Violation> {
    let Some(native_kind) = node.native_kind.as_deref() else {
        return Vec::new();
    };
    let mut violations = Vec::new();

    if PLACEHOLDER_NATIVE_KINDS.contains(&native_kind) && node.target.is_none() {
        violations.push(Violation {
            context: context.to_string(),
            message: format!(
                "placeholder node {:?} (nativeKind {native_kind:?}) has no `target`, and none could be derived \
                 from the legacy `<file>#<name>` qualifiedName convention (qualifiedName: {:?})",
                node.id, node.qualified_name
            ),
        });
    }

    if native_kind == CONTAINER_NATIVE_KIND {
        violations.push(Violation {
            context: context.to_string(),
            message: format!(
                "node {:?} has nativeKind \"container\" - container nodes are never plugin-emitted, \
                 core alone materializes them",
                node.id
            ),
        });
    }

    violations
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

    /// A legacy (v1) placeholder whose address derives cleanly must pass -
    /// the shape check runs against the normalized v2 form, not the wire
    /// bytes, so this is exactly as conformant as sending an explicit
    /// `target` would be.
    #[test]
    fn legacy_placeholder_with_a_derivable_address_is_conformant() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"foo\",\"qualifiedName\":\"target.ts#foo\",\"filePath\":\"a.ts\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"exported\":false,\"language\":\"typescript\",\"nativeKind\":\"pending_symbol\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(report.is_conformant(), "{:?}", report.violations);
    }

    /// The negative case `legacy_placeholder_with_a_derivable_address_is_conformant`
    /// checks the positive side of: a `pending_symbol` whose `qualifiedName`
    /// does not fit the `<file>#<name>` convention (no `#` at all) derives no
    /// target, and the shape check must say so with a message naming both
    /// the `nativeKind` and the offending `qualifiedName` - not fail silently
    /// or surface as an opaque parse error.
    #[test]
    fn placeholder_with_no_derivable_legacy_target_is_a_violation() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"foo\",\"qualifiedName\":\"not-the-convention\",\"filePath\":\"a.ts\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"exported\":false,\"language\":\"typescript\",\"nativeKind\":\"pending_symbol\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(!report.is_conformant());
        assert_eq!(report.violations[0].context, "NDJSON line 1");
        assert!(report.violations[0].message.contains("pending_symbol"), "{:?}", report.violations);
        assert!(report.violations[0].message.contains("not-the-convention"), "{:?}", report.violations);
    }

    /// A v2 node whose `target` is present on the wire needs no derivation
    /// at all and must still pass - the positive v2 counterpart to the two
    /// legacy tests above.
    #[test]
    fn v2_placeholder_with_an_explicit_target_is_conformant() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"foo\",\"qualifiedName\":\"target.ts#foo\",\"filePath\":\"a.ts\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"visibility\":\"file\",\"language\":\"typescript\",\"nativeKind\":\"pending_symbol\",\"target\":{\"scope\":{\"file\":\"target.ts\"},\"key\":{\"name\":\"foo\"}}}\n";
        let report = check_bulk_output(ndjson);
        assert!(report.is_conformant(), "{:?}", report.violations);
    }

    /// Core, not a plugin, materializes container nodes (Data Model >
    /// Logical containers) - one arriving on the wire is always a
    /// conformance violation, never a legitimate message.
    #[test]
    fn a_plugin_emitted_container_node_is_a_violation() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"pkg\",\"qualifiedName\":\"pkg\",\"filePath\":\"\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":0}},\"exported\":false,\"language\":\"go\",\"nativeKind\":\"container\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(!report.is_conformant());
        assert!(report.violations[0].message.contains("container"), "{:?}", report.violations);
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
