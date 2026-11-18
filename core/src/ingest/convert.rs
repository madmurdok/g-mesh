//! The wire->storage mapping: a plugin's `FileChangeDiff`/`WireNode`/`WireEdge` to the
//! storage layer's `Diff`/`NodeRecord`/`EdgeRecord`.

use std::collections::HashSet;

use crate::model::{DeclarationRecord, Diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};
use crate::protocol::types::{
    FileChangeDiff, PathError, PlaceholderTarget, QualifiedPath, SourceTier, TargetKey, TargetScope,
    Visibility, WireEdge, WireNode,
};

/// Converts the wire-level diff (nested `range: {start, end}`) into the
/// storage layer's flat `start_line`/`start_col`/`end_line`/`end_col`
/// fields. `delete_node_ids`/`delete_edge_ids` pass through unchanged since
/// both sides already agree on `Vec<String>`.
pub(crate) fn to_storage_diff(wire: FileChangeDiff, warnings: &mut PathWarnings) -> Diff {
    Diff {
        upsert_nodes: wire.upsert_nodes.into_iter().map(|node| to_node_record(node, warnings)).collect(),
        delete_node_ids: wire.delete_node_ids,
        upsert_edges: wire.upsert_edges.into_iter().map(to_edge_record).collect(),
        delete_edge_ids: wire.delete_edge_ids,
    }
}

/// Warns about a plugin's invalid paths at most once per (language, file),
/// so a plugin that gets every path in a file wrong logs one line for it.
#[derive(Debug, Default)]
pub(crate) struct PathWarnings {
    warned: HashSet<(String, String)>,
    /// Every line written, for tests.
    pub(crate) emitted: Vec<String>,
}

impl PathWarnings {
    fn warn(&mut self, node: &WireNode, what: &str, error: &PathError) {
        if !self.warned.insert((node.language.clone(), node.file_path.clone())) {
            return;
        }
        let line = format!(
            "{prefix}the {} plugin sent an invalid {what} for {:?} in {}: {error}; \
             dropped it and kept the node (later invalid paths in this file are dropped without a warning)",
            node.language,
            node.qualified_name,
            node.file_path,
            prefix = crate::log::role_prefix()
        );
        crate::log_line!("{line}");
        self.emitted.push(line);
    }
}

/// The node's paths that pass their rules. A `qualifiedPath` that fails
/// drops with all its aliases; an alias that fails, including one sent
/// without a `qualifiedPath`, drops alone. The node itself is always kept.
pub(crate) fn checked_paths(
    node: &WireNode,
    warnings: &mut PathWarnings,
) -> (Option<QualifiedPath>, Vec<QualifiedPath>) {
    if let Err(error) = node.check_qualified_path() {
        warnings.warn(node, "qualifiedPath", &error);
        return (None, Vec::new());
    }
    let aliases = node
        .alias_paths
        .iter()
        .filter(|alias| match node.check_alias_path(alias) {
            Ok(()) => true,
            Err(error) => {
                warnings.warn(node, "aliasPaths entry", &error);
                false
            }
        })
        .cloned()
        .collect();
    (node.qualified_path.clone(), aliases)
}

/// Wire node -> storage record. Shared with the cold-start bulk index
/// (`daemon::bulk_index`), which ingests the very same `WireNode` shape off
/// an NDJSON stream instead of out of a diff response - the two paths must
/// never disagree about how a wire node becomes a row.
pub(crate) fn to_node_record(node: WireNode, warnings: &mut PathWarnings) -> NodeRecord {
    let (visibility, visibility_container) = to_storage_visibility(&node.visibility);
    let (qualified_path, alias_paths) = checked_paths(&node, warnings);
    let key_path = node.target.as_ref().and_then(|target| match target.check_key_path() {
        Ok(()) => target.key_path.clone(),
        Err(error) => {
            warnings.warn(&node, "keyPath", &error);
            None
        }
    });
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
        // writes it: `link::symbol_links::link_diff` filters this very
        // `Diff` in memory, before anything is read back from storage.
        exported: matches!(node.visibility, Visibility::Public),
        visibility,
        visibility_container,
        container: node.container,
        // Carried through to `apply_diff`, where `storage::containers` stores it
        // on the container's `containers.parentKey` - it describes the
        // container, not this member, so it has no `nodes` column of its own.
        container_parent: node.container_parent,
        target: node.target.as_ref().map(|target| to_placeholder_target_record(target, key_path)),
        doc_comment: node.doc_comment,
        language: node.language,
        native_kind: node.native_kind,
        has_syntax_errors: node.has_syntax_errors,
        generated: node.generated,
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
        qualified_path,
        alias_paths,
        untyped_calls: node.untyped_calls,
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
/// `key_path` is the target's own, already checked by the caller.
pub(crate) fn to_placeholder_target_record(
    target: &PlaceholderTarget,
    key_path: Option<QualifiedPath>,
) -> PlaceholderTargetRecord {
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
        key_path,
        // Extra waiting scopes are file paths: kept only on a file-scoped
        // target (the conformance rule), never read as container keys.
        also_scopes: if matches!(target.scope, TargetScope::File(_)) {
            target.also_scopes.clone()
        } else {
            Vec::new()
        },
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
    // engine to report. A `WireEdge` always has a real one - the protocol's legacy
    // normalization already synthesizes `"tree-sitter"`/`"ts-compiler"` for a
    // v1 sender, so `edge.engine` is populated regardless of which protocol
    // version produced this edge - so it overwrites the guess here, the same
    // way `to_declaration` below is set post-construction rather than
    // threaded through `new`.
    record.engine = edge.engine;
    record.to_declaration = edge.to_declaration.map(|ordinal| ordinal as i64);
    record.specifier = edge.specifier;
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
