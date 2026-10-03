//! Usages the linker could not attach to any declaration, disclosed on the
//! page of a declaration they may belong to.
//!
//! A usage whose target the linker cannot settle stays on its own
//! `pending_symbol` placeholder: the edge's `toId` is the placeholder, never
//! a declaration. A `find_callers`/`find_references` page anchored on the
//! declaration walks only edges whose `toId` is that declaration, so it cannot
//! list such a usage, and without this field the page would read as complete
//! (`hasMore: false`, nothing else) while a caller is missing. Shapes that end
//! up here include a call through `use super::*` in a `mod tests`, a type
//! re-exported under two `#[cfg]` arms, a crate alias, and an index written by
//! an older plugin.
//!
//! The match is by name, so it can only say "may": a placeholder qualifies
//! when its bare `name` equals the anchor's and, for a key of two or more
//! segments (`T::f`), the key's second-to-last segment equals the anchor's
//! second-to-last. A one-segment key (a bare `f(..)` or an imported name)
//! qualifies only when the anchor is not a member of a type, since a bare
//! name never reaches a member. A key with no segments recorded (`keyPath`
//! NULL) is treated as a one-segment key. Segments come from the plugin's
//! paths (`storage::qualified_path`); no display string is split here.

use rusqlite::Connection;
use serde::Serialize;

use crate::graph::pagination::{self, FileTally};
use crate::graph::symbol_links::{PENDING_SYMBOL_NATIVE_KIND, REEXPORT_NATIVE_KIND};
use crate::protocol::types::PathSegment;
use crate::storage::qualified_path;
use crate::storage::write::NodeRecord;

/// Ceiling on how many files the tally names, at ~40 bytes an entry.
const MAX_UNLINKED_FILE_TALLY: usize = 20;

const UNLINKED_USAGES_HINT: &str =
    "Unresolved usages whose target has this symbol's name (and type, for a `T::f` call) that \
     g-mesh could not link to any declaration - e.g. a call through `use super::*` or through a \
     type re-exported under two `#[cfg]` arms. They may be usages of this symbol missing from \
     `results`, so this page is not exact: check `files` before treating it as complete.";

/// Usages that may belong to the anchor but sit on unlinked placeholders.
/// Absent from a response, not zero, when there is no candidate: most anchors
/// have none, and they must not grow a byte to say so.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnlinkedUsages {
    /// Every candidate usage edge, uncapped.
    count: usize,
    /// Files holding the candidates, highest count first, capped at
    /// [`MAX_UNLINKED_FILE_TALLY`].
    files: Vec<FileTally>,
    /// Present only when the cap cut `files`.
    #[serde(skip_serializing_if = "is_false")]
    files_truncated: bool,
    hint: &'static str,
}

fn is_false(flag: &bool) -> bool {
    !*flag
}

impl UnlinkedUsages {
    /// The files this disclosure names.
    pub(crate) fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(|tally| tally.path.as_str())
    }

    /// Bytes this field adds to a response, so the page bound can hold them
    /// back. Zero when there is nothing to disclose.
    pub(crate) fn wire_len(disclosure: &Option<Self>) -> usize {
        disclosure.as_ref().map_or(0, |found| {
            // `,"unlinkedUsages":` plus the value.
            serde_json::to_vec(found).map_or(0, |bytes| bytes.len()) + 20
        })
    }
}

/// The unresolved `edge_kinds` usages that may belong to `anchor`, or `None`
/// when there is none. Errors are swallowed to `None`: this is a footnote on
/// an answer that already succeeded, and failing the call over it would trade
/// a good answer for no answer.
pub(crate) fn probe(
    conn: &Connection,
    anchor: &NodeRecord,
    edge_kinds: &[&str],
    file_paths: &[&str],
) -> Option<UnlinkedUsages> {
    if anchor.kind == pagination::FILE_KIND
        || matches!(
            anchor.native_kind.as_deref(),
            Some(PENDING_SYMBOL_NATIVE_KIND) | Some(REEXPORT_NATIVE_KIND)
        )
    {
        return None;
    }
    let candidates = candidate_rows(conn, anchor, edge_kinds, file_paths).ok()?;
    if candidates.is_empty() {
        return None;
    }

    let anchor_segments = anchor.qualified_path.as_ref().map(|path| path.segments());
    let parent = anchor_segments.and_then(|segments| segments.len().checked_sub(2).map(|i| &segments[i]));
    let mut is_member: Option<bool> = None;
    let mut by_file: Vec<(String, i64)> = Vec::new();
    let mut count = 0;
    for (file_path, key_path) in candidates {
        let key = key_path.as_deref().and_then(qualified_path::decode);
        let key_segments = key.as_ref().map(|path| path.segments()).unwrap_or_default();
        let matches = if key_segments.len() >= 2 {
            parent.is_some_and(|parent| key_segments[key_segments.len() - 2].name == parent.name)
        } else {
            !*is_member.get_or_insert_with(|| is_type_member(conn, anchor, anchor_segments))
        };
        if !matches {
            continue;
        }
        count += 1;
        match by_file.iter_mut().find(|(path, _)| *path == file_path) {
            Some((_, refs)) => *refs += 1,
            None => by_file.push((file_path, 1)),
        }
    }
    if count == 0 {
        return None;
    }

    by_file.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let files_truncated = by_file.len() > MAX_UNLINKED_FILE_TALLY;
    by_file.truncate(MAX_UNLINKED_FILE_TALLY);
    let files = by_file.into_iter().map(|(path, refs)| FileTally { path, refs }).collect();
    Some(UnlinkedUsages { count, files, files_truncated, hint: UNLINKED_USAGES_HINT })
}

/// `(usage's file, placeholder keyPath)` for every `edge_kinds` edge into a
/// `pending_symbol` placeholder of the anchor's language whose bare name is
/// the anchor's, under the `file_paths` scope.
///
/// An edge still on a placeholder is unlinked whatever its `resolved` bit
/// says: the linker sets `resolved = 1` only in the same update that repoints
/// the edge onto a declaration, and ingest stores a plugin's own bit as sent,
/// so a plugin that marks a placeholder edge resolved leaves `resolved = 1`
/// on an edge nothing linked. The query therefore does not filter on it.
fn candidate_rows(
    conn: &Connection,
    anchor: &NodeRecord,
    edge_kinds: &[&str],
    file_paths: &[&str],
) -> rusqlite::Result<Vec<(String, Option<String>)>> {
    let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&anchor.name, &anchor.language];
    sql_params.extend(edge_kinds.iter().map(|kind| kind as &dyn rusqlite::ToSql));
    sql_params.extend(file_paths.iter().map(|path| path as &dyn rusqlite::ToSql));

    let mut stmt = conn.prepare(&candidate_sql(edge_kinds.len(), file_paths.len()))?;
    let rows = stmt.query_map(sql_params.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// `?1` is the name and `?2` the language, then `kinds` edge kinds, then
/// `paths` scope paths. The placeholder kind is a literal so the planner can
/// use the partial `idx_nodes_pending_name`; without it this is a scan of
/// every node of the language on every call.
fn candidate_sql(kinds: usize, paths: usize) -> String {
    let kind_params: Vec<String> = (0..kinds).map(|i| format!("?{}", i + 3)).collect();
    let scope_filter = if paths == 0 {
        "1 = 1".to_string()
    } else {
        let params: Vec<String> = (0..paths).map(|i| format!("?{}", i + 3 + kinds)).collect();
        format!("f.filePath IN ({})", params.join(", "))
    };
    format!(
        "SELECT f.filePath, p.keyPath \
         FROM nodes t \
         JOIN placeholder_targets p ON p.nodeId = t.id \
         JOIN edges e ON e.toId = t.id \
         JOIN nodes f ON f.id = e.fromId \
         WHERE t.name = ?1 AND t.language = ?2 AND t.nativeKind = '{PENDING_SYMBOL_NATIVE_KIND}' \
           AND e.kind IN ({}) \
           AND {scope_filter}",
        kind_params.join(", ")
    )
}

/// Whether the anchor's parent path names a `Type` node of its language. An
/// anchor with no recorded path, or a one-segment one, is not a member.
fn is_type_member(conn: &Connection, anchor: &NodeRecord, segments: Option<&[PathSegment]>) -> bool {
    let Some(segments) = segments.filter(|segments| segments.len() >= 2) else {
        return false;
    };
    let mut parent = String::new();
    for segment in &segments[..segments.len() - 1] {
        parent.push_str(segment.sep.as_deref().unwrap_or(""));
        parent.push_str(&segment.name);
    }
    conn.query_row(
        "SELECT 1 FROM nodes WHERE qualifiedName = ?1 AND language = ?2 AND kind = 'Type' LIMIT 1",
        rusqlite::params![parent, anchor.language],
        |_| Ok(()),
    )
    .is_ok()
}

#[cfg(test)]
#[path = "unlinked_tests.rs"]
mod tests;
