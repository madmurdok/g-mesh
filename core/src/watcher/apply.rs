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
use std::sync::Mutex;
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
    conn: &Mutex<Connection>,
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
    conn: &Mutex<Connection>,
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
///
/// # GM-396: embedding inference runs with the connection lock released
///
/// `conn` is a `Mutex`, not an already-locked `&mut Connection`, so this
/// function can choose exactly when to hold it - the same seam
/// `daemon::bulk_index::commit` already uses for a bulk batch (see that
/// function's own "GM-394" doc section). It is locked once for `apply_diff`
/// and the two linking passes - ordinary SQLite writes, bounded by the size
/// of one file's diff - then released before
/// [`EmbeddingPipeline::compute`](crate::embedding::EmbeddingPipeline::compute)
/// runs the model over whatever that diff upserted, and locked again only for
/// [`EmbeddingPipeline::store`](crate::embedding::EmbeddingPipeline::store).
///
/// Before this, every caller of this function held `conn`'s lock (taken by
/// `daemon::plugin::PluginProcess::send_one`/`ensure_fresh`/`semantic_pass`,
/// once, around the whole round trip) for as long as inference took -
/// GM-393 bounds one node's embedding input to
/// [`DEFAULT_MAX_SEQUENCE_LENGTH`](crate::embedding::model::DEFAULT_MAX_SEQUENCE_LENGTH)
/// tokens, but a diff from one large file can still upsert many embeddable
/// nodes, and every one of them ran with every other connection - a tool
/// call, the MCP handshake - locked out for however long that took. Nothing
/// about the *plugin* round trip changes here: `daemon::plugin::PluginProcess`
/// still serializes every call through this file's own `self.state` lock
/// exactly as before (see `daemon::lifecycle`'s "Lock order" section), so two
/// incremental reparses for the same language still never interleave on the
/// wire - only `conn`'s lock, the one other connections actually wait on, is
/// narrowed.
///
/// The lock-free gap this opens is a window in which some other writer (a
/// second reparse of the same file replayed after a relaunch, a bulk walk, a
/// workspace-triggered per-language re-walk) can commit its own diff for a
/// node this round trip is about to write a vector for.
/// [`EmbeddingPipeline::store`]'s own doc comment ("GM-396") is what actually
/// closes that: it re-checks each node's current content before writing, so
/// only a node still present with the exact text this embedding was computed
/// from gets its vector stored - a node that changed or disappeared in the
/// meantime is left alone, safely, for whatever wrote its newer content to
/// re-embed.
#[allow(clippy::too_many_arguments)]
fn round_trip<R: BufRead + Send, W: Write>(
    reader: &mut R,
    writer: &mut W,
    conn: &Mutex<Connection>,
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
    {
        let mut guard = conn.lock().unwrap();
        apply_diff(&mut guard, &diff).with_context(|| format!("failed to apply the {method} diff"))?;
        // After the commit, never before: linking points edges at `File`
        // nodes, and the ones this diff brought with it have to be in the
        // index first.
        imports::link_diff(&mut guard, &diff).context("failed to link the file's resolved imports")?;
        // Symbols second, and for the same reason: a usage edge can only be
        // repointed at an export that is already committed - including the
        // ones this very diff added, which other files may have been waiting
        // for.
        symbol_links::link_diff(&mut guard, &diff)
            .context("failed to link the file's cross-file symbol usages")?;
    } // GM-396: released here - see this function's own doc section.

    // Test-only hook, honored between the lock release above and the
    // re-lock below - see `HOLD_COMPUTE_FILE_ENV`'s own doc comment.
    hold_compute_open_for_tests();

    // Runs with no lock held at all - see this function's own "GM-396" doc
    // section.
    let computed = embedding.compute(&diff);

    // Embedding is best-effort and reported rather than propagated
    // (`EmbeddingPipeline::store`'s own doc comment), for the same reason a
    // failed semantic pass does not fail this round trip: a diff that is
    // already committed and linked must not be undone by an optional layer
    // on top of it.
    {
        let guard = conn.lock().unwrap();
        embedding.store(&guard, &computed);
    }
    Ok(RoundTrip { incomplete: response.incomplete })
}

/// Test-only: holds this round trip open, with `conn`'s lock already
/// released, for as long as the file named by
/// [`HOLD_COMPUTE_FILE_ENV`] exists - GM-396's counterpart to
/// `daemon::bulk_index::HOLD_LOCK_FILE_ENV`/`hold_the_lock_open_for_tests`,
/// which proves the opposite property (a lock genuinely *held* across a
/// batch's embedding step). This one sits exactly where a real
/// `EmbeddingPipeline::compute`'s inference would run - after `apply_diff`/
/// the two linking passes have released `conn` and before `store` reacquires
/// it - so a test can prove a concurrent connection user is never blocked on
/// an incremental reparse's embedding step, without needing real model
/// weights on the machine running it (this hook fires regardless of whether
/// `compute` goes on to find a model loaded at all).
///
/// A no-op unless [`HOLD_COMPUTE_FILE_ENV`] is set, which is every real run.
fn hold_compute_open_for_tests() {
    let Some(path) = std::env::var_os(HOLD_COMPUTE_FILE_ENV).filter(|p| !p.is_empty()) else { return };
    let path = std::path::PathBuf::from(path);
    eprintln!(
        "g-mesh: holding a reparse's lock-free embedding window open until {} is removed \
         ({HOLD_COMPUTE_FILE_ENV})",
        path.display()
    );
    // Bounded the same way `daemon::bulk_index`'s own test-only holds are:
    // this is scaffolding for a test that deletes the file itself, and a test
    // that forgets to must fail as a timeout rather than wedge the daemon
    // forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Path whose *deletion* releases a round trip that is holding its lock-free
/// embedding window open for a test - see [`hold_compute_open_for_tests`].
pub const HOLD_COMPUTE_FILE_ENV: &str = "G_MESH_ROUND_TRIP_HOLD_COMPUTE_FILE";

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
mod tests;
