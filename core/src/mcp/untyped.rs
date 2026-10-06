//! Method calls made through a receiver whose type the structural tier did
//! not infer, disclosed on the page of a method they may reach. Design:
//! `docs/architecture/gm-486-untyped-receiver-marker.md`.
//!
//! A call `x.f()` whose receiver type the plugin could not name produces no
//! edge at all: there is no target to put on a placeholder, so
//! `super::unlinked` cannot see it either. A plugin that reports such calls
//! sends their bare names on the enclosing node (`untypedCalls`), and they
//! land in the `untyped_calls` table as `(name, nodeId)`. This module counts
//! the rows whose name is the anchor's, so a method's caller/reference page
//! can say it may be missing calls instead of reading as complete.
//!
//! The match is by bare name only, so the field can only say "may", and it
//! stays off every anchor a receiver call cannot reach: only methods count
//! ([`is_method`]). A row is dropped when its caller already has an edge of
//! the page's kinds to the anchor (any `source`: the caller is on the page),
//! and when the semantic tier has answered that call - the caller has a
//! `semantic` edge to some node of that name, wherever it landed. Nothing
//! else gates it: a completed semantic pass hides only the calls it answered.
//!
//! It never counts what `unlinkedUsages` counts: a call whose receiver type
//! was inferred is a typed site and goes on a placeholder, not in this table.

use rusqlite::Connection;

use super::unlinked::{self, CandidateTally};
use crate::storage::write::NodeRecord;

/// Calls named like a method, made through an untyped receiver, that may
/// reach the anchor. `count` is the number of calling functions, not of call
/// sites.
pub(crate) type UntypedReceiverCalls = CandidateTally;

/// The Rust plugin's `nativeKind`s for a member function: inherent method,
/// trait declaration and trait-impl method. Needed besides
/// [`unlinked::is_type_member`] because a trait-impl method's parent path is
/// `<T as Tr>`, which names no node.
const METHOD_NATIVE_KINDS: &[&str] = &["method", "trait_method", "trait_impl_method"];

const UNTYPED_RECEIVER_CALLS_HINT: &str =
    "Method calls named like this symbol, made through a receiver whose type g-mesh did not infer \
     (a closure or loop variable, a generic, `dyn`, a field or a call chain), from functions with \
     no edge to this symbol yet. Some may call this symbol, so this page is not exact: check \
     `files` before treating it as complete.";

/// The untyped receiver calls that may reach `anchor` through `edge_kinds`,
/// or `None` when there is none or the anchor is not a method. Errors are
/// swallowed to `None`, as in [`unlinked::probe`]: a footnote must not fail
/// an answer that already succeeded.
pub(crate) fn probe(
    conn: &Connection,
    anchor: &NodeRecord,
    edge_kinds: &[&str],
    file_paths: &[&str],
) -> Option<UntypedReceiverCalls> {
    if !is_method(conn, anchor) {
        return None;
    }
    let mut sql_params: Vec<&dyn rusqlite::ToSql> = vec![&anchor.name, &anchor.language, &anchor.id];
    sql_params.extend(edge_kinds.iter().map(|kind| kind as &dyn rusqlite::ToSql));
    sql_params.extend(file_paths.iter().map(|path| path as &dyn rusqlite::ToSql));

    let mut stmt = conn.prepare(&candidate_sql(edge_kinds.len(), file_paths.len())).ok()?;
    let rows = stmt
        .query_map(sql_params.as_slice(), |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
        .ok()?;
    let by_file: Vec<(String, i64)> = rows.collect::<rusqlite::Result<_>>().ok()?;
    let count = by_file.iter().map(|(_, refs)| *refs as usize).sum();
    CandidateTally::from_files(count, by_file, UNTYPED_RECEIVER_CALLS_HINT)
}

/// `?1` is the name, `?2` the language, `?3` the anchor id, then `kinds` edge
/// kinds, then `paths` scope paths. Yields `(caller's file, calling
/// functions)`; the table's primary key `(name, nodeId)` makes each row one
/// calling function, and serves the `name = ?1` lookup.
fn candidate_sql(kinds: usize, paths: usize) -> String {
    let kind_params: Vec<String> = (0..kinds).map(|i| format!("?{}", i + 4)).collect();
    let scope_filter = if paths == 0 {
        "1 = 1".to_string()
    } else {
        let params: Vec<String> = (0..paths).map(|i| format!("?{}", i + 4 + kinds)).collect();
        format!("f.filePath IN ({})", params.join(", "))
    };
    format!(
        "SELECT f.filePath, COUNT(*) \
         FROM untyped_calls u \
         JOIN nodes f ON f.id = u.nodeId \
         WHERE u.name = ?1 AND f.language = ?2 \
           AND {scope_filter} \
           AND NOT EXISTS (SELECT 1 FROM edges e \
                           WHERE e.fromId = u.nodeId AND e.toId = ?3 AND e.kind IN ({})) \
           AND NOT EXISTS (SELECT 1 FROM edges e JOIN nodes t ON t.id = e.toId \
                           WHERE e.fromId = u.nodeId AND e.source = 'semantic' AND t.name = u.name) \
         GROUP BY f.filePath",
        kind_params.join(", ")
    )
}

/// Whether a receiver call `x.f()` can reach `anchor`: a `Function` that is
/// one of [`METHOD_NATIVE_KINDS`] or, for a plugin with other native kinds,
/// a member of a `Type` node ([`unlinked::is_type_member`]). A free function
/// never qualifies.
fn is_method(conn: &Connection, anchor: &NodeRecord) -> bool {
    if anchor.kind != "Function" {
        return false;
    }
    if anchor.native_kind.as_deref().is_some_and(|kind| METHOD_NATIVE_KINDS.contains(&kind)) {
        return true;
    }
    let segments = anchor.qualified_path.as_ref().map(|path| path.segments());
    unlinked::is_type_member(conn, anchor, segments)
}

#[cfg(test)]
#[path = "untyped_tests.rs"]
mod tests;
