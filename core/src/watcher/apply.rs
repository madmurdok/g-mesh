//! Bridges a (debounced) file-change event to a committed diff: sends a
//! `FileChanged` control-plane request to a language plugin, reads back its
//! diff, and applies it through `storage::write::apply_diff`.
//!
//! The semantic pass ([`apply_semantic_pass`]) is the same motion with a
//! different question: instead of "what does this file look like now", it
//! asks "what can your type checker now resolve that tree-sitter could
//! only guess at". Its answer comes back in the same diff shape and goes
//! through the same commit-and-link pipeline, because an upgraded edge is
//! just that edge re-sent under its own id with a better `source`.
//!
//! This module is transport-agnostic on purpose - it only knows about
//! `Read`/`Write` streams (the same abstraction `jsonrpc.rs` and
//! `handshake.rs` already use), not about how the peer on the other end of
//! those streams came to exist. A real spawned-plugin-process transport can
//! be plugged in later without touching this function; for now, tests fake
//! the peer with `std::io::pipe()` plus a thread, exactly like
//! `jsonrpc.rs`'s own pipe-based tests do.

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::Connection;

use crate::embedding::EmbeddingPipeline;
use crate::graph::{imports, symbol_links};
use crate::protocol::jsonrpc::{read_message_with_timeout, write_message};
use crate::protocol::types::{
    ControlEnvelope, ControlMessage, FileChangeDiff, FileChangeResponse, PlaceholderTarget, RequestId,
    SourceTier, TargetKey, TargetScope, Visibility, WireEdge, WireNode, JSONRPC_VERSION,
};
use crate::storage::write::{
    apply_diff, DeclarationRecord, Diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord,
};

/// Sends a `FileChanged` request (tagged with `request_id`) for `file_path`
/// over `writer`, reads the plugin's `FileChangeResponse` off `reader`,
/// validates the response id matches the request id, converts the wire
/// diff into a `storage::write::Diff`, and commits it via `apply_diff` -
/// then asks the same plugin for a semantic pass over that same file.
///
/// The semantic pass rides here, rather than at each of this function's
/// call sites, because "the reparse has settled" is precisely the condition
/// it needs and this is the only place that knows it. Every caller that
/// reparses a file gets it for free: the watcher (via
/// `daemon::lifecycle::PluginSupervisor::file_changed`), the wake-up replay
/// of everything queued while the plugin slept, and the query-time
/// staleness catch-up in `watcher::staleness::ensure_fresh` - which is a
/// reparse settling as much as any other, and whose edges deserve the
/// upgrade just as much.
///
/// A failing semantic pass is reported and dropped, not propagated. It is
/// an *upgrade* over a graph that is already committed, correct and
/// serviceable: failing the whole reparse because the semantic layer was
/// unavailable would turn a better answer that did not arrive into a worse
/// one that did. (A plugin that died is still noticed - the next request's
/// write fails, which is what `daemon::plugin::PluginProcess` relaunches
/// and replays on.)
///
/// `request_id` is supplied by the caller rather than generated internally
/// so this function stays pure and easy to test; the id for the second
/// round trip is derived from it by [`semantic_pass_id`].
///
/// `file_changed_timeout`/`semantic_pass_timeout` bound each of this
/// function's two round trips independently - see
/// `daemon::plugin::RoundTripTimeouts`'s doc comment for why they differ and
/// how their values are chosen. `on_timeout` is called at most once, from
/// whichever round trip actually times out (the two never overlap: a timed-
/// out `FileChanged` returns early via `?` before the semantic pass is ever
/// sent) - see [`round_trip`] for what it is for and why this function does
/// not know how to implement it itself.
///
/// `semantic_pass_capable` gates the second round trip entirely: a plugin
/// whose manifest declares `capabilities.semantic_pass = false` (the
/// conservative default - see `daemon::manifest::Capabilities::default`)
/// never receives a `semanticPass` request at all, per the architecture
/// doc's `plugin.toml additions` ("`false`: core never sends it, and no
/// empty-diff answer is required"). A plain `bool` rather than the
/// `Capabilities` type itself: this module is transport-agnostic and knows
/// nothing about manifests on purpose (see its own doc comment), so the
/// caller that *does* own the manifest - `daemon::plugin::PluginProcess`,
/// via its own `manifest.capabilities.semantic_pass` - resolves the
/// capability and hands down only the one bit this function needs, rather
/// than this module reaching into `daemon::manifest` for itself.
#[allow(clippy::too_many_arguments)]
pub fn apply_file_change<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    file_path: impl Into<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    file_changed_timeout: Duration,
    semantic_pass_timeout: Duration,
    semantic_pass_capable: bool,
    on_timeout: &mut dyn FnMut(),
) -> Result<()> {
    let file_path = file_path.into();
    // A structural reparse has nothing to be incomplete about - the plugin
    // either extracted the file or it did not - so this round trip's report is
    // deliberately dropped here and read only for a `semanticPass`.
    let _structural = round_trip(
        reader,
        writer,
        conn,
        ControlMessage::FileChanged { file_path: file_path.clone() },
        request_id.clone(),
        embedding,
        file_changed_timeout,
        on_timeout,
    )?;

    if !semantic_pass_capable {
        return Ok(());
    }

    if let Err(err) = apply_semantic_pass(
        reader,
        writer,
        conn,
        vec![file_path.clone()],
        semantic_pass_id(&request_id),
        embedding,
        semantic_pass_timeout,
        on_timeout,
    ) {
        eprintln!(
            "g-mesh: the semantic pass over {file_path} failed after its reparse ({err:#}) - \
             its edges keep whatever the structural pass resolved"
        );
    }
    Ok(())
}

/// Asks the plugin's semantic layer what it can now resolve, and commits
/// the answer exactly like a file-change diff.
///
/// `file_paths` scopes the pass: one entry after an incremental reparse,
/// **empty** for the whole project once the cold-start bulk walk is done -
/// see `ControlMessage::SemanticPass` on why "empty" means everything
/// rather than nothing.
///
/// There is deliberately no new storage path here. The pass answers with a
/// `FileChangeDiff` whose edges carry the ids the structural pass already
/// gave them, so `apply_diff`'s `ON CONFLICT(id) DO UPDATE` upgrades each
/// one in place - `source` `tree-sitter` -> `ts-compiler`, `resolved`
/// `false` -> `true` - and touches nothing it was not sent.
#[allow(clippy::too_many_arguments)]
pub fn apply_semantic_pass<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    file_paths: Vec<String>,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<()> {
    let whole_project = file_paths.is_empty();
    let outcome = round_trip(
        reader,
        writer,
        conn,
        ControlMessage::SemanticPass { file_paths },
        request_id,
        embedding,
        timeout,
        on_timeout,
    )?;

    // The diff is committed by now, deliberately: an incomplete pass is a
    // *partial* answer, not a failed one, and everything it did resolve is as
    // real as any other semantic edge (`protocol::types::
    // FileChangeResponse::incomplete` says why the plugin does not report this
    // as a JSON-RPC error instead). What is left to do is refuse to call the
    // pass finished, which for a whole-project pass is exactly what an `Err`
    // here means to `daemon::semantic`: `language_state.semanticPassAt` stays
    // unset and the next daemon start asks this language again.
    //
    // A per-file pass has no completion flag to protect, so an incomplete one
    // is worth a line and nothing more - failing it would only make
    // `apply_file_change` log the same thing twice.
    if outcome.incomplete {
        if whole_project {
            bail!(
                "the plugin reported an incomplete whole-project semantic pass - its diff is committed, \
                 but the pass is not recorded as done"
            );
        }
        eprintln!(
            "g-mesh: the plugin reported an incomplete per-file semantic pass - its edges keep whatever \
             this pass did resolve"
        );
    }
    Ok(())
}

/// What one round trip reported about itself, beyond the diff it already
/// committed - today only [`FileChangeResponse::incomplete`], which is
/// meaningless for a `fileChanged` and load-bearing for a `semanticPass`.
///
/// A struct rather than a bare `bool` so that the one thing [`round_trip`]
/// returns keeps a name at both call sites: `let _ = round_trip(...)` reads
/// as "nothing to say", where a discarded bare `bool` reads as a bug.
struct RoundTrip {
    incomplete: bool,
}

/// The id for the semantic pass that follows a file change, derived from
/// that file change's own id.
///
/// Both round trips happen back to back on one stream that the caller
/// (`daemon::plugin::PluginProcess`) holds locked for their whole duration,
/// so the second needs an id the plugin cannot confuse with the first, not
/// a globally unique one. Deriving beats threading a second id through
/// every caller, and cannot collide with the counter-issued ids either:
/// those are always `Number`, these are always `String`, and `RequestId`
/// compares across variants.
///
/// Which variant it was derived from is part of the derived id, so the two
/// id spaces cannot alias: without it a `Number(3)` and a `String("3")`
/// base - both legal per JSON-RPC 2.0, and both reachable, since
/// `PluginProcess` issues numbers while callers may pass strings - would
/// derive the very same id.
fn semantic_pass_id(base: &RequestId) -> RequestId {
    RequestId::String(match base {
        RequestId::Number(n) => format!("semanticPass:num:{n}"),
        RequestId::String(s) => format!("semanticPass:str:{s}"),
    })
}

/// One control-plane round trip that answers with a diff: write the
/// request, read the response, check that it answers *this* request, then
/// commit and link the diff it carried.
///
/// Shared by [`apply_file_change`] and [`apply_semantic_pass`], which
/// differ only in the message they send. Everything after the response is
/// identical, which is the whole reason a semantic upgrade needed no
/// storage or schema work of its own.
///
/// The write above is not timed: this module's whole concern is the read
/// having no timeout (see this module's own doc comment and
/// `docs/architecture/multi-language-plugins.md`'s "Semantic engine hangs or
/// is slow" failure mode), and a write blocking would need the plugin's own
/// stdin pipe buffer to be full *and* the plugin to have stopped reading it -
/// a materially different, and much rarer, failure than a plugin that reads a
/// request and simply never answers it.
///
/// `timeout` bounds only the read; `on_timeout` is this function's entire
/// interface to whatever makes that read actually give up - see
/// `protocol::jsonrpc::read_message_with_timeout`'s doc comment for the
/// mechanism and why it lives there instead of here. This module stays
/// transport-agnostic on purpose (see its own doc comment): it has no idea
/// whether `reader`/`writer` are a spawned plugin's pipes or a test's bare
/// `std::io::pipe()`, so it cannot itself know what "make the peer stop being
/// silent" means - only the caller that owns the transport does (for a real
/// plugin, `daemon::plugin::PluginProcess` kills the child).
#[allow(clippy::too_many_arguments)]
fn round_trip<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &mut Connection,
    message: ControlMessage,
    request_id: RequestId,
    embedding: &EmbeddingPipeline,
    timeout: Duration,
    on_timeout: &mut dyn FnMut(),
) -> Result<RoundTrip> {
    let method = method_name(&message);
    let request =
        ControlEnvelope { jsonrpc: JSONRPC_VERSION.to_string(), id: Some(request_id.clone()), message };
    write_message(writer, &request).with_context(|| format!("failed to write {method} request to plugin"))?;

    let response: FileChangeResponse = read_message_with_timeout(reader, timeout, on_timeout)
        .with_context(|| format!("failed to read plugin's {method} response"))?
        .with_context(|| format!("plugin closed its output before responding to {method}"))?;

    if response.id != request_id {
        bail!(
            "{method} response id {:?} does not match request id {:?} - refusing to apply a diff that answers a different request",
            response.id,
            request_id,
        );
    }

    let diff = to_storage_diff(response.result);
    apply_diff(conn, &diff).with_context(|| format!("failed to apply the {method} diff"))?;
    // After the commit, never before: linking points edges at `File` nodes,
    // and the ones this diff brought with it have to be in the index first.
    imports::link_diff(conn, &diff).context("failed to link the file's resolved imports")?;
    // Symbols second, and for the same reason: a usage edge can only be
    // repointed at an export that is already committed - including the ones
    // this very diff added, which other files may have been waiting for.
    symbol_links::link_diff(conn, &diff).context("failed to link the file's cross-file symbol usages")?;
    // Embedding is best-effort and reported rather than propagated
    // (`EmbeddingPipeline::apply`'s own doc comment), for the same reason a
    // failed semantic pass does not fail this round trip: a diff that is
    // already committed and linked must not be undone by an optional layer
    // on top of it.
    if let Err(err) = embedding.apply(conn, &diff) {
        eprintln!("g-mesh daemon: failed to embed the {method} diff: {err:#}");
    }
    Ok(RoundTrip { incomplete: response.incomplete })
}

/// The wire `method` string for a control message, for error messages that
/// name the request that failed. Matched rather than round-tripped through
/// serde so that adding a variant to `ControlMessage` fails to compile here
/// instead of silently reporting the wrong method name.
fn method_name(message: &ControlMessage) -> &'static str {
    match message {
        ControlMessage::Reindex { .. } => "reindex",
        ControlMessage::FileChanged { .. } => "fileChanged",
        ControlMessage::Status => "status",
        ControlMessage::SemanticPass { .. } => "semanticPass",
        ControlMessage::WorkspaceChanged { .. } => "workspaceChanged",
    }
}

/// Converts the wire-level diff (nested `range: {start, end}`) into the
/// storage layer's flat `start_line`/`start_col`/`end_line`/`end_col`
/// fields. `delete_node_ids`/`delete_edge_ids` pass through unchanged since
/// both sides already agree on `Vec<String>`.
fn to_storage_diff(wire: FileChangeDiff) -> Diff {
    Diff {
        upsert_nodes: wire.upsert_nodes.into_iter().map(to_node_record).collect(),
        delete_node_ids: wire.delete_node_ids,
        upsert_edges: wire.upsert_edges.into_iter().map(to_edge_record).collect(),
        delete_edge_ids: wire.delete_edge_ids,
    }
}

/// Wire node -> storage record. Shared with the cold-start bulk index
/// (`daemon::bulk_index`), which ingests the very same `WireNode` shape off
/// an NDJSON stream instead of out of a diff response - the two paths must
/// never disagree about how a wire node becomes a row.
pub(crate) fn to_node_record(node: WireNode) -> NodeRecord {
    let (visibility, visibility_container) = to_storage_visibility(&node.visibility);
    NodeRecord {
        id: node.id,
        kind: format!("{:?}", node.kind),
        name: node.name,
        qualified_name: node.qualified_name,
        file_path: node.file_path.clone(),
        start_line: node.range.start.line as i64,
        start_col: node.range.start.col as i64,
        end_line: node.range.end.line as i64,
        end_col: node.range.end.col as i64,
        signature: node.signature,
        // The read-side mirror of the database's `GENERATED ALWAYS exported`
        // column - see `NodeRecord.exported`'s own doc comment for why this
        // still has to be set correctly here even though `apply_diff` never
        // writes it: `graph::symbol_links::link_diff` filters this very
        // `Diff` in memory, before anything is read back from storage.
        exported: matches!(node.visibility, Visibility::Public),
        visibility,
        visibility_container,
        container: node.container,
        // Carried through to `apply_diff`, where `graph::containers` stores it
        // on the container's `containers.parentKey` - it describes the
        // container, not this member, so it has no `nodes` column of its own.
        container_parent: node.container_parent,
        target: node.target.as_ref().map(to_placeholder_target_record),
        doc_comment: node.doc_comment,
        language: node.language,
        native_kind: node.native_kind,
        has_syntax_errors: node.has_syntax_errors,
        // Absent means "one declaration", which is no rows in the child table
        // - the same thing an empty list means to `apply_diff`, which replaces
        // whatever was stored either way.
        declarations: node
            .declarations
            .unwrap_or_default()
            .into_iter()
            .map(|declaration| DeclarationRecord {
                ordinal: declaration.ordinal as i64,
                start_line: declaration.start_line as i64,
                start_col: declaration.start_col as i64,
                end_line: declaration.end_line as i64,
                end_col: declaration.end_col as i64,
                signature: declaration.signature,
                has_body: declaration.has_body,
            })
            .collect(),
    }
}

/// `protocol::types::Visibility` -> `(nodes.visibility, nodes.
/// visibilityContainer)`. See `storage::schema`'s DDL comment on those two
/// columns for why the container's key gets its own nullable column rather
/// than being packed into `visibility` itself.
fn to_storage_visibility(visibility: &Visibility) -> (String, Option<String>) {
    match visibility {
        Visibility::Public => ("public".to_string(), None),
        Visibility::File => ("file".to_string(), None),
        Visibility::Container(key) => ("container".to_string(), Some(key.clone())),
    }
}

/// `protocol::types::PlaceholderTarget` -> `storage::write::
/// PlaceholderTargetRecord`. Notably does *not* set a `from_file` - the wire
/// type has no such field, because it is already available for free as the
/// placeholder node's own `filePath` (the existing "a placeholder's filePath
/// is the importing file" convention - `graph::symbol_links`'s module doc),
/// so `storage::write::apply_diff` fills `placeholder_targets.fromFile` from
/// `NodeRecord.file_path` directly rather than threading it through this
/// record - see that table's own DDL comment.
fn to_placeholder_target_record(target: &PlaceholderTarget) -> PlaceholderTargetRecord {
    let (scope_kind, scope) = match &target.scope {
        TargetScope::File(path) => ("file".to_string(), path.clone()),
        TargetScope::Container(key) => ("container".to_string(), key.clone()),
    };
    let (key_kind, key) = match &target.key {
        TargetKey::Name(name) => ("name".to_string(), name.clone()),
        TargetKey::QualifiedName(qualified_name) => ("qualifiedName".to_string(), qualified_name.clone()),
    };
    PlaceholderTargetRecord {
        scope_kind,
        scope,
        key_kind,
        key,
        from_container: target.from_container.clone(),
    }
}

/// Wire edge -> storage record; see [`to_node_record`] on why this is shared.
pub(crate) fn to_edge_record(edge: WireEdge) -> EdgeRecord {
    let mut record = EdgeRecord::new(
        edge.id,
        edge.from_id,
        edge.to_id,
        edge_kind_wire_value(&edge.kind),
        edge_source_tier_wire_value(&edge.source),
        edge.resolved,
    );
    // `EdgeRecord::new`'s legacy-string inference (`storage::write::
    // normalize_legacy_source`) sets `.engine` to a copy of the tier string
    // passed above, which is only ever right for a v1 caller with no real
    // engine to report. A `WireEdge` always has a real one - GM-263's legacy
    // normalization already synthesizes `"tree-sitter"`/`"ts-compiler"` for a
    // v1 sender, so `edge.engine` is populated regardless of which protocol
    // version produced this edge - so it overwrites the guess here, the same
    // way `to_declaration` below is set post-construction rather than
    // threaded through `new`.
    record.engine = edge.engine;
    record.to_declaration = edge.to_declaration.map(|ordinal| ordinal as i64);
    record
}

/// `nodes.kind` in the schema is a plain string matching `NodeKind`'s Rust
/// variant name (see `WireNode`'s doc comment in `protocol::types`); no
/// custom serde attributes are attached to `NodeKind`, so `{:?}` already
/// gives the exact variant name (e.g. "Function").
///
/// `edges.kind`, by contrast, has a custom serde rename
/// (`SCREAMING_SNAKE_CASE`) - reuse that exact wire string by round-tripping
/// through serde_json rather than re-deriving the mapping by hand, so the
/// storage string always matches what the wire format (and therefore what
/// the plugin actually sent) says, and any future rename attribute change on
/// this enum doesn't silently desync this file.
fn edge_kind_wire_value(kind: &crate::protocol::types::EdgeKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{kind:?}"))
}

/// `SourceTier` -> `edges.source`'s own two values (`storage::schema`'s DDL:
/// `CHECK (source IN ('syntactic', 'semantic'))`). A direct 1:1 mapping, not
/// the legacy engine-conflating one `storage::write::normalize_legacy_source`
/// still carries for callers that only have a v1 string - a `WireEdge`
/// always has a real, separate `engine` (see [`to_edge_record`]'s own
/// comment), so the tier alone is all this needs to produce.
fn edge_source_tier_wire_value(source: &SourceTier) -> String {
    match source {
        SourceTier::Syntactic => "syntactic".to_string(),
        SourceTier::Semantic => "semantic".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::jsonrpc::read_message;
    use crate::protocol::types::{
        EdgeKind, NodeKind, Position, Range, SourceTier, Visibility, WireEdge, WireNode,
    };
    use crate::storage::schema;
    use std::io::BufReader;

    /// A timeout no test below is meant to hit - every stub plugin in this
    /// module answers immediately, so this only has to be longer than a
    /// slow CI box's scheduling noise. The dedicated timeout tests near the
    /// bottom of this module use their own short, explicit durations.
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    /// `on_timeout` for every call below that is not itself testing the
    /// timeout mechanism - asserting it is never invoked would be redundant
    /// with `TEST_TIMEOUT` never elapsing, but a stub plugin that hung would
    /// otherwise turn into a 5-second wait per test instead of a fast panic.
    fn on_timeout_must_not_fire() {
        panic!(
            "on_timeout fired in a test whose stub plugin always answers - the stub or the timeout is broken"
        );
    }

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
    }

    fn canned_node(id: &str) -> WireNode {
        WireNode {
            id: id.to_string(),
            kind: NodeKind::Function,
            name: "foo".to_string(),
            qualified_name: "mod::foo".to_string(),
            file_path: "src/lib.rs".to_string(),
            range: Range { start: Position { line: 1, col: 0 }, end: Position { line: 3, col: 1 } },
            signature: None,
            visibility: Visibility::Public,
            doc_comment: None,
            language: "rust".to_string(),
            native_kind: None,
            has_syntax_errors: false,
            declarations: None,
            container: None,
            container_parent: None,
            target: None,
        }
    }

    /// Spawns a thread acting as a stub plugin: reads one `ControlEnvelope`
    /// request off `reader`, asserts it's the expected `FileChanged` with
    /// the expected id, then writes `response` back over `writer`. Mirrors
    /// how `jsonrpc.rs`/`handshake.rs` fake a peer over a pipe in their own
    /// tests.
    ///
    /// `semantic_diff` says whether this stub should also expect the
    /// semantic pass that `apply_file_change` sends once the reparse has
    /// settled, and what to answer it with. It has to be explicit rather
    /// than "answer one if it comes": a stub that speculatively read a
    /// second request would block forever against the tests where no second
    /// request is sent (a rejected response never gets that far), and the
    /// test would deadlock on `join` instead of failing.
    fn spawn_stub_plugin(
        mut reader: std::io::PipeReader,
        mut writer: std::io::PipeWriter,
        expected_file_path: &'static str,
        expected_id: RequestId,
        response: FileChangeResponse,
        semantic_diff: Option<FileChangeDiff>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(&mut reader);
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            assert_eq!(request.id, Some(expected_id));
            match request.message {
                ControlMessage::FileChanged { file_path } => assert_eq!(file_path, expected_file_path),
                other => panic!("expected FileChanged, got {other:?}"),
            }
            write_message(&mut writer, &response).unwrap();

            let Some(result) = semantic_diff else { return };
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            let id = request.id.clone().expect("the semantic pass must be a request, not a notification");
            match request.message {
                ControlMessage::SemanticPass { file_paths } => {
                    assert_eq!(
                        file_paths,
                        vec![expected_file_path.to_string()],
                        "a reparse-triggered pass names exactly the file that settled"
                    );
                }
                other => panic!("expected SemanticPass, got {other:?}"),
            }
            write_message(
                &mut writer,
                &FileChangeResponse { jsonrpc: JSONRPC_VERSION.to_string(), id, result, incomplete: false },
            )
            .unwrap();
        })
    }

    /// An edge as the structural pass leaves it: a guess, unconfirmed.
    fn unresolved_edge(id: &str, from: &str, to: &str) -> WireEdge {
        WireEdge {
            id: id.to_string(),
            from_id: from.to_string(),
            to_id: to.to_string(),
            kind: EdgeKind::Calls,
            source: SourceTier::Syntactic,
            engine: "tree-sitter".to_string(),
            resolved: false,
            to_declaration: None,
        }
    }

    /// `(source, engine, resolved)` - `source` is the tier
    /// (`"syntactic"`/`"semantic"`) `edges.source`'s CHECK now enforces,
    /// `engine` its own new column (`storage::schema`'s DDL comment on
    /// `edges`).
    fn edge_source_and_resolved(conn: &Connection, id: &str) -> (String, String, bool) {
        conn.query_row("SELECT source, engine, resolved FROM edges WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
    }

    #[test]
    fn a_wire_nodes_declaration_list_becomes_storage_records() {
        use crate::protocol::types::WireDeclaration;

        let mut node = canned_node("n1");
        node.declarations = Some(vec![
            WireDeclaration {
                ordinal: 0,
                start_line: 1,
                start_col: 7,
                end_line: 1,
                end_col: 47,
                signature: Some("parse(input: string): string[]".to_string()),
                has_body: false,
            },
            WireDeclaration {
                ordinal: 1,
                start_line: 3,
                start_col: 7,
                end_line: 5,
                end_col: 1,
                signature: None,
                has_body: true,
            },
        ]);

        let record = to_node_record(node);

        assert_eq!(
            record.declarations,
            vec![
                DeclarationRecord {
                    ordinal: 0,
                    start_line: 1,
                    start_col: 7,
                    end_line: 1,
                    end_col: 47,
                    signature: Some("parse(input: string): string[]".to_string()),
                    has_body: false,
                },
                DeclarationRecord {
                    ordinal: 1,
                    start_line: 3,
                    start_col: 7,
                    end_line: 5,
                    end_col: 1,
                    signature: None,
                    has_body: true,
                },
            ]
        );
        // And an ordinary node still says "no declarations", which `apply_diff`
        // reads as "one" - the same thing an absent field on the wire means.
        assert!(to_node_record(canned_node("n2")).declarations.is_empty());
    }

    #[test]
    fn a_wire_edges_declaration_binding_becomes_a_storage_record() {
        let mut edge = unresolved_edge("e1", "n1", "n2");
        assert_eq!(to_edge_record(edge.clone()).to_declaration, None);

        edge.to_declaration = Some(0);
        assert_eq!(
            to_edge_record(edge).to_declaration,
            Some(0),
            "ordinal 0 is a binding, not the absence of one"
        );
    }

    /// The GM-264 wire boundary: a container-scoped, qualifiedName-keyed
    /// placeholder (the shape only a semantic tier over a containered
    /// language sends - see `protocol::types`'s own
    /// `wire_node_v2_shape_round_trips_container_and_target` test) becomes
    /// the storage-layer `visibility`/`visibility_container`/`container`/
    /// `target` fields `apply_diff` writes.
    #[test]
    fn to_node_record_derives_visibility_container_and_target_from_the_wire_v2_shape() {
        let mut node = canned_node("n1");
        node.visibility = Visibility::Container("github.com/x/app/server".to_string());
        node.container = Some("github.com/x/app/server".to_string());
        node.native_kind = Some("pending_symbol".to_string());
        node.target = Some(PlaceholderTarget {
            scope: TargetScope::Container("github.com/x/app/server".to_string()),
            key: TargetKey::QualifiedName("Server.Close".to_string()),
            from_container: Some("github.com/x/app/client".to_string()),
        });

        let record = to_node_record(node);

        assert!(!record.exported, "container visibility is never Public");
        assert_eq!(record.visibility, "container");
        assert_eq!(record.visibility_container.as_deref(), Some("github.com/x/app/server"));
        assert_eq!(record.container.as_deref(), Some("github.com/x/app/server"));
        let target = record.target.expect("a pending_symbol with a wire target must keep it");
        assert_eq!(target.scope_kind, "container");
        assert_eq!(target.scope, "github.com/x/app/server");
        assert_eq!(target.key_kind, "qualifiedName");
        assert_eq!(target.key, "Server.Close");
        assert_eq!(target.from_container.as_deref(), Some("github.com/x/app/client"));
    }

    /// The ordinary case: `Visibility::Public` maps to `"public"`/`exported`,
    /// and an ordinary (non-placeholder) node carries no target at all.
    #[test]
    fn to_node_record_maps_public_visibility_and_leaves_target_absent_for_an_ordinary_node() {
        let record = to_node_record(canned_node("n1"));
        assert!(record.exported);
        assert_eq!(record.visibility, "public");
        assert_eq!(record.visibility_container, None);
        assert_eq!(record.target, None);
    }

    /// `to_edge_record`'s own boundary: `engine` comes from the wire's real
    /// `WireEdge.engine`, not from `EdgeRecord::new`'s legacy-string
    /// inference off the tier - see `to_edge_record`'s own comment on why the
    /// guess `::new` makes has to be overwritten.
    #[test]
    fn to_edge_record_carries_the_wires_own_engine_rather_than_guessing_one_from_the_tier() {
        let mut edge = unresolved_edge("e1", "n1", "n2");
        edge.source = SourceTier::Semantic;
        edge.engine = "go-types".to_string();

        let record = to_edge_record(edge);

        assert_eq!(record.source, "semantic");
        assert_eq!(record.engine, "go-types", "a real wire engine must never be collapsed to the tier name");
    }

    #[test]
    fn file_change_diff_is_committed_to_sqlite() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(1);
        let canned_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: request_id.clone(),
            result: FileChangeDiff {
                upsert_nodes: vec![canned_node("n1"), canned_node("n2")],
                delete_node_ids: vec![],
                upsert_edges: vec![WireEdge {
                    id: "e1".to_string(),
                    from_id: "n1".to_string(),
                    to_id: "n2".to_string(),
                    kind: EdgeKind::Calls,
                    source: SourceTier::Syntactic,
                    engine: "tree-sitter".to_string(),
                    resolved: false,
                    to_declaration: None,
                }],
                delete_edge_ids: vec![],
            },
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            canned_response,
            Some(FileChangeDiff::default()),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "nodes"), 2);
        assert_eq!(count(&conn, "edges"), 1);

        let name: String =
            conn.query_row("SELECT name FROM nodes WHERE id = 'n1'", [], |row| row.get(0)).unwrap();
        assert_eq!(name, "foo");
        let start_line: i64 =
            conn.query_row("SELECT startLine FROM nodes WHERE id = 'n1'", [], |row| row.get(0)).unwrap();
        assert_eq!(start_line, 1);
        let edge_kind: String =
            conn.query_row("SELECT kind FROM edges WHERE id = 'e1'", [], |row| row.get(0)).unwrap();
        assert_eq!(edge_kind, "CALLS");
    }

    #[test]
    fn diff_with_deletes_removes_rows() {
        let mut conn = setup_conn();
        // Seed rows the stub plugin's diff will delete.
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "rust")],
                ..Default::default()
            },
        )
        .unwrap();

        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();

        let request_id = RequestId::String("req-2".to_string());
        let canned_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: request_id.clone(),
            result: FileChangeDiff { delete_node_ids: vec!["n1".to_string()], ..Default::default() },
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            canned_response,
            Some(FileChangeDiff::default()),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "nodes"), 0);
    }

    #[test]
    fn mismatched_response_id_is_rejected() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(10);
        let wrong_id_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: RequestId::Number(999), // deliberately does not match the request
            result: FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            wrong_id_response,
            // A rejected file-change response never gets as far as the
            // semantic pass, so no second request is coming.
            None,
        );

        let mut buf_reader = BufReader::new(core_reader);
        let result = apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        );
        plugin.join().unwrap();

        assert!(result.is_err(), "a response for a different request id must not be applied");
        assert_eq!(count(&conn, "nodes"), 0, "diff from a mismatched-id response must not be committed");
    }

    #[test]
    fn empty_diff_response_is_a_safe_no_op() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(3);
        let empty_response = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: request_id.clone(),
            result: FileChangeDiff::default(),
        };

        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/unchanged.rs",
            request_id.clone(),
            empty_response,
            Some(FileChangeDiff::default()),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/unchanged.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "nodes"), 0);
        assert_eq!(count(&conn, "edges"), 0);
    }

    /// A stub that expects a `SemanticPass` as its *first* request - for
    /// exercising [`apply_semantic_pass`] on its own, the way the
    /// post-bulk-index caller reaches it.
    fn spawn_semantic_stub(
        mut reader: std::io::PipeReader,
        mut writer: std::io::PipeWriter,
        expected_file_paths: Vec<String>,
        result: FileChangeDiff,
        incomplete: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut buf_reader = BufReader::new(&mut reader);
            let request: ControlEnvelope = read_message(&mut buf_reader).unwrap().unwrap();
            let id = request.id.clone().expect("a semantic pass expects an answer, so it carries an id");
            match request.message {
                ControlMessage::SemanticPass { file_paths } => {
                    assert_eq!(file_paths, expected_file_paths)
                }
                other => panic!("expected SemanticPass, got {other:?}"),
            }
            write_message(
                &mut writer,
                &FileChangeResponse { jsonrpc: JSONRPC_VERSION.to_string(), id, result, incomplete },
            )
            .unwrap();
        })
    }

    /// The acceptance criterion, at the unit level: an edge the structural
    /// pass left as a `tree-sitter` guess comes back confirmed, and nothing
    /// else in the index moves.
    #[test]
    fn a_semantic_pass_upgrades_an_edge_in_place_and_leaves_the_others_alone() {
        let mut conn = setup_conn();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    NodeRecord::new("n1", "Function", "foo", "m::foo", "src/lib.rs", "typescript"),
                    NodeRecord::new("n2", "Function", "bar", "m::bar", "src/lib.rs", "typescript"),
                ],
                upsert_edges: vec![
                    EdgeRecord::new("e1", "n1", "n2", "CALLS", "tree-sitter", false),
                    EdgeRecord::new("e2", "n2", "n1", "CALLS", "tree-sitter", false),
                ],
                ..Default::default()
            },
        )
        .unwrap();

        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();

        // Only e1 is answered for - e2 is not in the diff at all.
        let mut upgraded = unresolved_edge("e1", "n1", "n2");
        upgraded.source = SourceTier::Semantic;
        upgraded.engine = "ts-compiler".to_string();
        upgraded.resolved = true;
        let plugin = spawn_semantic_stub(
            plugin_reader,
            plugin_writer,
            vec!["src/lib.rs".to_string()],
            FileChangeDiff { upsert_edges: vec![upgraded], ..Default::default() },
            false,
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_semantic_pass(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            vec!["src/lib.rs".to_string()],
            RequestId::Number(9),
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(
            edge_source_and_resolved(&conn, "e1"),
            ("semantic".to_string(), "ts-compiler".to_string(), true),
            "the answered edge must be upgraded in place, not duplicated"
        );
        assert_eq!(
            edge_source_and_resolved(&conn, "e2"),
            ("syntactic".to_string(), "tree-sitter".to_string(), false),
            "an edge the pass said nothing about must not change"
        );
        assert_eq!(count(&conn, "edges"), 2, "an upgrade is an update, never an insert");
        assert_eq!(count(&conn, "nodes"), 2);
    }

    /// The trigger half of the same criterion: a reparse that settles is
    /// followed, on the same stream, by a pass over exactly that file -
    /// asserted inside `spawn_stub_plugin` - whose diff lands.
    #[test]
    fn a_settled_reparse_is_followed_by_a_semantic_pass_over_that_file() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(4);
        let structural = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: request_id.clone(),
            result: FileChangeDiff {
                upsert_nodes: vec![canned_node("n1"), canned_node("n2")],
                upsert_edges: vec![unresolved_edge("e1", "n1", "n2")],
                ..Default::default()
            },
        };

        let mut upgraded = unresolved_edge("e1", "n1", "n2");
        upgraded.source = SourceTier::Semantic;
        upgraded.engine = "ts-compiler".to_string();
        upgraded.resolved = true;
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            structural,
            Some(FileChangeDiff { upsert_edges: vec![upgraded], ..Default::default() }),
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(
            edge_source_and_resolved(&conn, "e1"),
            ("semantic".to_string(), "ts-compiler".to_string(), true),
            "the edge the reparse left unresolved must come back upgraded"
        );
        assert_eq!(count(&conn, "edges"), 1);
    }

    /// The semantic pass is an upgrade over a graph that is already
    /// committed and serviceable, so losing it must not lose the reparse
    /// that earned it.
    #[test]
    fn a_failing_semantic_pass_does_not_fail_the_reparse() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(5);
        let structural = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: request_id.clone(),
            result: FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
        };

        // `None`: the stub answers the reparse and then goes away, which is
        // what a plugin whose semantic layer died looks like from here.
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            structural,
            None,
        );

        let mut buf_reader = BufReader::new(core_reader);
        let outcome = apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            true,
            &mut on_timeout_must_not_fire,
        );
        plugin.join().unwrap();

        assert!(outcome.is_ok(), "a lost upgrade must not undo a committed reparse: {outcome:?}");
        assert_eq!(count(&conn, "nodes"), 1, "the structural diff still committed");
    }

    /// GM-270's acceptance criterion at the unit level: `semantic_pass_capable
    /// = false` must skip the second round trip entirely, not just ignore its
    /// answer - a plugin whose manifest never declared
    /// `capabilities.semantic_pass = true` gets no `semanticPass` request at
    /// all, per the architecture doc's `plugin.toml additions` ("core never
    /// sends it, and no empty-diff answer is required").
    ///
    /// Discriminates: the stub here is built with `spawn_stub_plugin`'s
    /// `semantic_diff: None`, which answers only the first (`FileChanged`)
    /// request and then returns - it never reads a second frame. If this
    /// function sent `semanticPass` anyway (the pre-GM-270, unconditional
    /// behaviour), the stub thread would still be blocked reading a request
    /// nobody is answering when the main thread reaches `plugin.join()`
    /// below, and the test would hang instead of completing - flip
    /// `semantic_pass_capable` to `true` here to see it hang.
    #[test]
    fn a_semantic_pass_incapable_plugin_is_never_sent_a_semantic_pass_request() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let request_id = RequestId::Number(6);
        let structural = FileChangeResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            incomplete: false,
            id: request_id.clone(),
            result: FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
        };

        // `None`: this stub answers exactly one request and never looks for a
        // second - the same shape `a_failing_semantic_pass_does_not_fail_the_reparse`
        // uses for "the semantic layer died", reused here for "was never
        // asked in the first place".
        let plugin = spawn_stub_plugin(
            plugin_reader,
            plugin_writer,
            "src/lib.rs",
            request_id.clone(),
            structural,
            None,
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_file_change(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            "src/lib.rs",
            request_id,
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            TEST_TIMEOUT,
            false,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(
            count(&conn, "nodes"),
            1,
            "the structural diff must still commit with no semantic pass sent"
        );
    }

    /// The post-bulk-index shape: nothing to name, so the list is empty and
    /// the plugin reads that as "the whole project".
    #[test]
    fn a_whole_project_semantic_pass_sends_an_empty_file_list() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let plugin =
            spawn_semantic_stub(plugin_reader, plugin_writer, Vec::new(), FileChangeDiff::default(), false);

        let mut buf_reader = BufReader::new(core_reader);
        apply_semantic_pass(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            Vec::new(),
            RequestId::Number(1),
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            &mut on_timeout_must_not_fire,
        )
        .unwrap();
        plugin.join().unwrap();

        assert_eq!(count(&conn, "edges"), 0);
    }

    /// A plugin that answers a whole-project pass with `incomplete` (GM-289's
    /// `FileChangeResponse::incomplete`) gets both halves of what it asked
    /// for: the diff it did manage is committed, and the pass is reported as
    /// *not* finished, which is what stops `daemon::semantic` recording
    /// `language_state.semanticPassAt` for that language.
    #[test]
    fn an_incomplete_whole_project_pass_commits_its_diff_and_is_still_an_error() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let plugin = spawn_semantic_stub(
            plugin_reader,
            plugin_writer,
            Vec::new(),
            FileChangeDiff { upsert_nodes: vec![canned_node("n1")], ..Default::default() },
            true,
        );

        let mut buf_reader = BufReader::new(core_reader);
        let outcome = apply_semantic_pass(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            Vec::new(),
            RequestId::Number(1),
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            &mut on_timeout_must_not_fire,
        );
        plugin.join().unwrap();

        let err = outcome.expect_err("an incomplete pass must not be reported as a completed one");
        assert!(format!("{err:#}").contains("incomplete"), "{err:#}");
        assert_eq!(
            count(&conn, "nodes"),
            1,
            "what the pass did resolve is committed - an incomplete pass is partial, not failed"
        );
    }

    /// The same flag on a *per-file* pass is a log line, not a failure: there
    /// is no completion record for one to protect, and failing it would only
    /// make `apply_file_change` print the same thing twice.
    #[test]
    fn an_incomplete_per_file_pass_is_not_an_error() {
        let (plugin_reader, mut core_writer) = std::io::pipe().unwrap();
        let (core_reader, plugin_writer) = std::io::pipe().unwrap();
        let mut conn = setup_conn();

        let plugin = spawn_semantic_stub(
            plugin_reader,
            plugin_writer,
            vec!["src/lib.rs".to_string()],
            FileChangeDiff::default(),
            true,
        );

        let mut buf_reader = BufReader::new(core_reader);
        apply_semantic_pass(
            &mut buf_reader,
            &mut core_writer,
            &mut conn,
            vec!["src/lib.rs".to_string()],
            RequestId::Number(1),
            &EmbeddingPipeline::disabled(),
            TEST_TIMEOUT,
            &mut on_timeout_must_not_fire,
        )
        .expect("a per-file pass reports incompleteness without failing");
        plugin.join().unwrap();
    }

    /// The derived id has to be distinguishable from the file change it
    /// follows *and* from every counter-issued id that comes after it.
    #[test]
    fn the_semantic_pass_id_cannot_collide_with_the_request_it_follows() {
        let base = RequestId::Number(3);
        let derived = semantic_pass_id(&base);
        assert_ne!(derived, base);
        assert_ne!(derived, RequestId::Number(4));
        assert_ne!(semantic_pass_id(&RequestId::String("3".to_string())), derived);
    }
}
