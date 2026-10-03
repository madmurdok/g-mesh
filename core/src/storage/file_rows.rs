//! The rows one file owns, and how a `fileChanged` diff is widened to retire
//! the ones the plugin no longer stands behind but could not name.
//!
//! A plugin names a deleted id only if it remembers emitting it, and its
//! memory is per process: the control process that answers a `fileChanged`
//! is not the one that ran the bulk walk, and a restart empties it. So core
//! completes the diff from its own rows, in two cases the plugin can state
//! without that memory:
//!
//! - [`FileScope::Gone`]: the file is not on disk any more. Every node of the
//!   file goes, with its outgoing edges and its `indexed_files` row.
//! - [`FileScope::Complete`]: the plugin says its upserts are the whole file
//!   (`FileChangeDiff::complete`). Every node of the file the diff does not
//!   upsert goes. So does every edge out of the file's nodes that the diff
//!   does not upsert, except a `semantic` edge from a node that stays: the
//!   structural diff never carries those, and the semantic pass that follows
//!   the reparse re-sends what it still resolves.
//!
//! A file owns the nodes whose `filePath` is the file (placeholders included:
//! they carry the importer's path) and the edges out of them. Container nodes
//! have an empty `filePath` and belong to no file. Edges *into* a removed node
//! from another file are left alone, as `storage::write::apply_diff`
//! documents for every node delete.

use std::collections::HashSet;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::storage::write::Diff;

/// What a `fileChanged` diff says about the file as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileScope {
    /// The diff is relative to what the plugin last told core: it names its
    /// own deletes, and core adds none.
    Partial,
    /// The diff's upserts are the whole file.
    Complete,
    /// The file is gone from disk.
    Gone,
}

/// Adds to `diff` the deletes `scope` implies for `file_path`'s stored rows.
/// Ids the diff already deletes are not repeated.
pub(crate) fn widen(conn: &Connection, file_path: &str, scope: FileScope, diff: &mut Diff) -> Result<()> {
    if scope == FileScope::Partial {
        return Ok(());
    }
    let upserted_nodes: HashSet<&str> = diff.upsert_nodes.iter().map(|node| node.id.as_str()).collect();
    let upserted_edges: HashSet<&str> = diff.upsert_edges.iter().map(|edge| edge.id.as_str()).collect();

    let mut stmt = conn
        .prepare_cached("SELECT id FROM nodes WHERE filePath = ?1 ORDER BY id")
        .context("failed to prepare the file's node query")?;
    let stored_nodes = stmt
        .query_map(params![file_path], |row| row.get::<_, String>(0))
        .context("failed to read the file's nodes")?
        .collect::<rusqlite::Result<Vec<String>>>()
        .context("failed to read the file's nodes")?;
    let removed: HashSet<String> =
        stored_nodes.iter().filter(|id| !upserted_nodes.contains(id.as_str())).cloned().collect();

    let mut stmt = conn
        .prepare_cached(
            "SELECT e.id, e.fromId, e.source FROM edges e JOIN nodes n ON n.id = e.fromId
             WHERE n.filePath = ?1 ORDER BY e.id",
        )
        .context("failed to prepare the file's edge query")?;
    let stored_edges = stmt
        .query_map(params![file_path], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        })
        .context("failed to read the file's edges")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to read the file's edges")?;

    let already_nodes: HashSet<String> = diff.delete_node_ids.iter().cloned().collect();
    let already_edges: HashSet<String> = diff.delete_edge_ids.iter().cloned().collect();

    for (id, from_id, source) in stored_edges {
        if upserted_edges.contains(id.as_str()) || already_edges.contains(&id) {
            continue;
        }
        let keep_semantic =
            scope == FileScope::Complete && source == "semantic" && !removed.contains(&from_id);
        if !keep_semantic {
            diff.delete_edge_ids.push(id);
        }
    }
    for id in stored_nodes {
        if removed.contains(&id) && !already_nodes.contains(&id) {
            diff.delete_node_ids.push(id);
        }
    }
    Ok(())
}

/// Deletes `file_path`'s `indexed_files` baseline, for a file that is gone.
pub(crate) fn delete_indexed_file(conn: &Connection, file_path: &str) -> Result<()> {
    conn.execute("DELETE FROM indexed_files WHERE filePath = ?1", params![file_path])
        .context("failed to delete the file's indexed_files row")?;
    Ok(())
}
