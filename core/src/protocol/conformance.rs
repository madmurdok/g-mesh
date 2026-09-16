use std::io::{BufReader, Cursor};

// `CONTAINER_NATIVE_KIND`: core, not a plugin, materializes container nodes
// (`graph::containers`) - a container has members in many files, so no single
// file's diff can own it - so a plugin ever emitting one is always a
// conformance violation, never a legitimate message.
use crate::graph::containers::CONTAINER_NATIVE_KIND;
use crate::graph::imports::RESOLVED_MODULE_NATIVE_KIND;
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::protocol::jsonrpc::read_message;
use crate::protocol::ndjson::{BulkItem, NdjsonReader};
use crate::protocol::types::{ControlEnvelope, WireNode};

/// The `nativeKind`s a `WireNode` stands in for something outside its own
/// file rather than declaring anything (mirrors the plugin's own
/// `PLACEHOLDER_NATIVE_KINDS` in plugins/typescript/src/extract.ts, minus
/// `external_module` - it names a bare specifier that never links to
/// anything in this project, so core never materializes a node for one and
/// it never reaches this check; see this task's own report for why
/// `external_module` is exempt rather than required to carry a `target`).
/// Every one of these requires a `target` - see the `placeholder nativeKind
/// requires a target` shape check below.
pub(crate) const PLACEHOLDER_NATIVE_KINDS: [&str; 3] =
    [PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND, RESOLVED_MODULE_NATIVE_KIND];

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
/// cannot express. serde already rejects unknown `kind`/`source` values (and,
/// as of GM-275, a v1-shaped line with no `visibility`/`engine`) as part of
/// ordinary deserialization, so there's no separate edge-kind allow-list to
/// maintain here - an invalid kind simply fails to parse as either type.
/// What *does* parse still has two rules of its own:
///
///  - a placeholder `nativeKind` (`pending_symbol`, `reexport`,
///    `resolved_module`) requires a `target` on the wire - a missing one is
///    reported here rather than surfacing as an opaque parse failure.
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
    placeholder_target_violation(node)
        .into_iter()
        .chain(plugin_emitted_container_violation(node))
        .map(|message| Violation { context: context.to_string(), message })
        .collect()
}

/// The `placeholder nativeKind requires a target` rule on its own, as a
/// message rather than a [`Violation`]. Split out of `node_shape_violations`
/// (GM-276) because `cli::plugin_check` reports this rule under its `shape`
/// check and [`plugin_emitted_container_violation`] under its separate
/// `ownership.no-container` check: a conformance kit whose every check must
/// be provable by a fake plugin failing *only* that check cannot have one
/// helper answer for two of them.
pub(crate) fn placeholder_target_violation(node: &WireNode) -> Option<String> {
    let native_kind = node.native_kind.as_deref()?;
    (PLACEHOLDER_NATIVE_KINDS.contains(&native_kind) && node.target.is_none()).then(|| {
        format!(
            "placeholder node {:?} (nativeKind {native_kind:?}) has no `target` (qualifiedName: {:?})",
            node.id, node.qualified_name
        )
    })
}

/// The "core alone materializes container nodes" rule on its own - see
/// [`placeholder_target_violation`] for why the two are separate functions.
pub(crate) fn plugin_emitted_container_violation(node: &WireNode) -> Option<String> {
    (node.native_kind.as_deref() == Some(CONTAINER_NATIVE_KIND)).then(|| {
        format!(
            "node {:?} has nativeKind \"container\" - container nodes are never plugin-emitted, \
             core alone materializes them",
            node.id
        )
    })
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
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Function\",\"name\":\"foo\",\"qualifiedName\":\"m::foo\",\"filePath\":\"a.rs\",\"range\":{\"start\":{\"line\":1,\"col\":0},\"end\":{\"line\":2,\"col\":0}},\"visibility\":\"file\",\"language\":\"rust\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(report.is_conformant(), "{:?}", report.violations);
    }

    #[test]
    fn invalid_edge_kind_is_a_violation() {
        let ndjson = b"{\"id\":\"e1\",\"fromId\":\"n1\",\"toId\":\"n2\",\"kind\":\"NOT_A_REAL_KIND\",\"source\":\"syntactic\",\"engine\":\"tree-sitter\",\"resolved\":false}\n";
        let report = check_bulk_output(ndjson);
        assert!(!report.is_conformant());
        assert_eq!(report.violations[0].context, "NDJSON line 1");
    }

    /// GM-275: a v1-shaped line (`exported`, no `visibility`) no longer
    /// parses at all, so it surfaces as an ordinary NDJSON parse failure
    /// rather than a `placeholder_target_violation` - see this file's git
    /// history for the pre-GM-275 legacy-derivation tests this replaces.
    #[test]
    fn a_v1_shaped_placeholder_line_is_a_violation() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"foo\",\"qualifiedName\":\"target.ts#foo\",\"filePath\":\"a.ts\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"exported\":false,\"language\":\"typescript\",\"nativeKind\":\"pending_symbol\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(!report.is_conformant());
        assert_eq!(report.violations[0].context, "NDJSON line 1");
    }

    /// A placeholder whose `nativeKind` requires a `target` but whose wire
    /// line omits one entirely - a v2 sender's own mistake, not a legacy
    /// shape - must be reported with a message naming both the `nativeKind`
    /// and the `qualifiedName`, not fail silently or as an opaque parse
    /// error.
    #[test]
    fn placeholder_with_no_target_is_a_violation() {
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"foo\",\"qualifiedName\":\"not-the-convention\",\"filePath\":\"a.ts\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":1}},\"visibility\":\"file\",\"language\":\"typescript\",\"nativeKind\":\"pending_symbol\"}\n";
        let report = check_bulk_output(ndjson);
        assert!(!report.is_conformant());
        assert_eq!(report.violations[0].context, "NDJSON line 1");
        assert!(report.violations[0].message.contains("pending_symbol"), "{:?}", report.violations);
        assert!(report.violations[0].message.contains("not-the-convention"), "{:?}", report.violations);
    }

    /// A v2 node whose `target` is present on the wire is conformant.
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
        let ndjson = b"{\"id\":\"n1\",\"kind\":\"Module\",\"name\":\"pkg\",\"qualifiedName\":\"pkg\",\"filePath\":\"\",\"range\":{\"start\":{\"line\":0,\"col\":0},\"end\":{\"line\":0,\"col\":0}},\"visibility\":\"file\",\"language\":\"go\",\"nativeKind\":\"container\"}\n";
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
