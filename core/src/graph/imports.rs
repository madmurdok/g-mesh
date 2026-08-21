//! Import linking: the pass that turns a plugin's *resolved* module
//! placeholder into a real edge between two `File` nodes.
//!
//! A language plugin emits one placeholder `Module` node per import specifier
//! and hangs the `IMPORTS` edge on it, because a specifier is text, not a
//! node id it can safely point at (see `recordImport` in
//! plugins/typescript/src/extract.ts). When the plugin recognises the specifier as
//! naming a file in this project it says so, by setting the placeholder's
//! `nativeKind` to [`RESOLVED_MODULE_NATIVE_KIND`] and its `qualifiedName` to
//! that file's project-relative path. This module is the other half of that
//! handshake: it looks the path up among the `File` nodes actually in the
//! index, repoints the edge, and drops the placeholder.
//!
//! ## Why here and not in the plugin
//!
//! Node ids are a pure function of the file path, so the plugin could compute
//! the target's id itself. What it cannot know is whether that node *exists*:
//!
//!  - Walk order. During a cold-start bulk index the target file may not have
//!    been walked yet. `daemon::bulk_index` may cut a batch anywhere in the
//!    stream and relies on a file's edges never leaving that file, which an
//!    edge emitted against a not-yet-committed node would break (edges are
//!    foreign keys onto nodes).
//!  - Index membership. A file can exist on disk and still never get a node -
//!    gitignored, under an excluded directory, or simply not this plugin's
//!    language. Only core knows what the index actually holds.
//!
//! Doing the *path* arithmetic here instead would be worse in the other
//! direction: extension guessing, `index.*` directory imports and
//! TypeScript's `.js` -> `.ts` substitution are language rules, and core is
//! deliberately language-agnostic (see the Data Model section of
//! docs/architecture/g-mesh-v1.md). So the plugin decides *which path* a
//! specifier names and core decides *whether that path is a node* - neither
//! side needs the other's knowledge, and nothing here touches the filesystem,
//! which is what keeps this cheap enough to run inside the same transaction
//! path as every write.
//!
//! ## What stays unresolved
//!
//! Everything the plugin did not mark: bare/package specifiers
//! (`"react"`, `"node:crypto"`), and relative specifiers that resolve to
//! nothing on disk. Plus anything marked whose target is not in the index -
//! that placeholder is simply left alone, so a dangling import degrades to
//! exactly the pre-linking behaviour instead of an edge into nothing.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::storage::write::Diff;

/// The `nativeKind` a plugin marks a resolved import placeholder with.
/// Mirrors `RESOLVED_MODULE_NATIVE_KIND` in plugins/typescript/src/extract.ts -
/// the two are one wire contract and must be changed together.
pub const RESOLVED_MODULE_NATIVE_KIND: &str = "resolved_module";

const MODULE_KIND: &str = "Module";
const FILE_KIND: &str = "File";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkSummary {
    /// `IMPORTS` edges repointed from a placeholder onto a real `File` node.
    pub linked_edges: usize,
    /// Placeholders removed because nothing pointed at them any more.
    pub dropped_placeholders: usize,
}

/// One resolved placeholder: the node to eliminate, and the project-relative
/// path it claims to name.
struct Placeholder {
    id: String,
    target_path: String,
}

/// Links every resolved placeholder in the index. The whole-project pass, run
/// once a bulk index has committed its last batch - at which point every
/// `File` node the walk will ever produce is in, so a placeholder that finds
/// no target here has no target at all.
pub fn link_all(conn: &mut Connection) -> Result<LinkSummary> {
    let placeholders = {
        let mut stmt = conn
            .prepare("SELECT id, qualifiedName FROM nodes WHERE kind = ?1 AND nativeKind = ?2")
            .context("failed to prepare the placeholder scan")?;
        let rows = stmt
            .query_map(params![MODULE_KIND, RESOLVED_MODULE_NATIVE_KIND], |row| {
                Ok(Placeholder { id: row.get(0)?, target_path: row.get(1)? })
            })
            .context("failed to scan for resolved import placeholders")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("failed to read resolved import placeholders")?
    };

    link(conn, placeholders)
}

/// Links what one just-applied diff could have changed, without rescanning
/// the whole index. Two things can newly become linkable:
///
///  - a placeholder the diff itself added (the reindexed file's own imports);
///  - a `File` node the diff added, which may be the target other files'
///    placeholders have been waiting for - the cross-file case, and the
///    reason a newly created file does not stay invisible to its importers
///    until they happen to be edited.
///
/// Not covered, deliberately: a file *deleted* from the project. Its `File`
/// node (and the edges into it) go when something deletes the node; the
/// importers' edges then come back as fresh placeholders the next time those
/// importers are reindexed, rather than being re-placeholdered eagerly here.
pub fn link_diff(conn: &mut Connection, diff: &Diff) -> Result<LinkSummary> {
    let mut placeholders: Vec<Placeholder> = diff
        .upsert_nodes
        .iter()
        .filter(|node| {
            node.kind == MODULE_KIND && node.native_kind.as_deref() == Some(RESOLVED_MODULE_NATIVE_KIND)
        })
        .map(|node| Placeholder { id: node.id.clone(), target_path: node.qualified_name.clone() })
        .collect();

    let added_files: Vec<&str> = diff
        .upsert_nodes
        .iter()
        .filter(|node| node.kind == FILE_KIND)
        .map(|node| node.file_path.as_str())
        .collect();
    if !added_files.is_empty() {
        // Indexed by idx_nodes_qualifiedName, so this is a lookup per added
        // file rather than a scan of the index.
        let mut stmt = conn
            .prepare("SELECT id, qualifiedName FROM nodes WHERE kind = ?1 AND nativeKind = ?2 AND qualifiedName = ?3")
            .context("failed to prepare the placeholder lookup")?;
        for file_path in added_files {
            let rows = stmt
                .query_map(params![MODULE_KIND, RESOLVED_MODULE_NATIVE_KIND, file_path], |row| {
                    Ok(Placeholder { id: row.get(0)?, target_path: row.get(1)? })
                })
                .context("failed to look up placeholders waiting on a new file")?;
            for row in rows {
                placeholders.push(row.context("failed to read a waiting placeholder")?);
            }
        }
    }

    placeholders.sort_by(|a, b| a.id.cmp(&b.id));
    placeholders.dedup_by(|a, b| a.id == b.id);
    link(conn, placeholders)
}

/// The linking itself, in one transaction: repoint, then drop what that
/// orphaned. Order matters - a placeholder still referenced by an edge is a
/// foreign-key parent, so it can only be deleted after the edges have moved.
///
/// Idempotent, which is what makes it safe to run after every write: a
/// placeholder that was already linked has no edges left to repoint and no
/// row left to delete, and a reindex that re-adds one (a restarted plugin
/// re-sends its full extraction, resetting `resolved` to false on the way
/// through) is simply linked again.
fn link(conn: &mut Connection, placeholders: Vec<Placeholder>) -> Result<LinkSummary> {
    let mut summary = LinkSummary::default();
    if placeholders.is_empty() {
        return Ok(summary);
    }

    let tx = conn.transaction().context("failed to start the import-linking transaction")?;
    {
        let mut find_file = tx
            .prepare("SELECT id FROM nodes WHERE kind = ?1 AND filePath = ?2")
            .context("failed to prepare the target-file lookup")?;
        let mut repoint = tx
            .prepare("UPDATE edges SET toId = ?1, resolved = 1 WHERE toId = ?2 AND kind = 'IMPORTS'")
            .context("failed to prepare the edge repoint")?;
        let mut incident = tx
            .prepare("SELECT COUNT(*) FROM edges WHERE fromId = ?1 OR toId = ?1")
            .context("failed to prepare the incident-edge count")?;
        let mut drop_placeholder = tx
            .prepare("DELETE FROM nodes WHERE id = ?1")
            .context("failed to prepare the placeholder delete")?;

        for placeholder in placeholders {
            let target: Option<String> = find_file
                .query_row(params![FILE_KIND, placeholder.target_path], |row| row.get(0))
                .optional()
                .context("failed to look up an import target")?;
            // No `File` node for that path: not indexed (gitignored,
            // excluded, another language) or not there at all. Leaving the
            // placeholder exactly as it is *is* the graceful fallback.
            let Some(target_id) = target else { continue };

            summary.linked_edges += repoint
                .execute(params![target_id, placeholder.id])
                .context("failed to repoint an import edge")?;

            let remaining: i64 = incident
                .query_row(params![placeholder.id], |row| row.get(0))
                .context("failed to count a placeholder's remaining edges")?;
            if remaining == 0 {
                summary.dropped_placeholders += drop_placeholder
                    .execute(params![placeholder.id])
                    .context("failed to drop a linked-away placeholder")?;
            }
        }
    }
    tx.commit().context("failed to commit the import-linking transaction")?;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema;
    use crate::storage::write::{apply_diff, EdgeRecord, NodeRecord};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // On, so that any attempt to point an edge at a node that is not
        // there - the exact failure this module exists to prevent - is a hard
        // error here rather than a silently dangling row.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::apply(&conn).unwrap();
        conn
    }

    fn file_node(path: &str) -> NodeRecord {
        NodeRecord::new(format!("file:{path}"), FILE_KIND, path, path, path, "typescript")
    }

    /// The plugin's own shape for an import placeholder: `filePath` is the
    /// *importing* file (that is where the statement is written), while
    /// `qualifiedName` is either the raw specifier or, when resolved, the
    /// target's project-relative path.
    fn placeholder_node(importer: &str, qualified_name: &str, native_kind: &str) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("mod:{importer}:{qualified_name}"),
            MODULE_KIND,
            qualified_name,
            qualified_name,
            importer,
            "typescript",
        );
        node.native_kind = Some(native_kind.to_string());
        node
    }

    fn import_edge(importer: &str, placeholder: &NodeRecord) -> EdgeRecord {
        EdgeRecord::new(
            format!("edge:{importer}:{}", placeholder.id),
            format!("file:{importer}"),
            placeholder.id.clone(),
            "IMPORTS",
            "tree-sitter",
            false,
        )
    }

    /// `importer` imports `specifier`, resolved by the plugin to `target`.
    fn seed_resolved_import(conn: &mut Connection, importer: &str, target: &str) {
        let placeholder = placeholder_node(importer, target, RESOLVED_MODULE_NATIVE_KIND);
        let edge = import_edge(importer, &placeholder);
        apply_diff(
            conn,
            &Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() },
        )
        .unwrap();
    }

    fn edge_target(conn: &Connection, edge_id: &str) -> (String, bool) {
        conn.query_row("SELECT toId, resolved FROM edges WHERE id = ?1", params![edge_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_resolved_placeholder_becomes_an_edge_between_the_two_files() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![file_node("src/index.ts"), file_node("src/db/connection.ts")],
                ..Default::default()
            },
        )
        .unwrap();
        seed_resolved_import(&mut conn, "src/index.ts", "src/db/connection.ts");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        let (to_id, resolved) = edge_target(&conn, "edge:src/index.ts:mod:src/index.ts:src/db/connection.ts");
        assert_eq!(to_id, "file:src/db/connection.ts", "the edge must point at the target's File node");
        assert!(resolved, "a path-resolved import is resolved, even though it is still tree-sitter-sourced");
        assert_eq!(count(&conn, "nodes"), 2, "the placeholder must be gone once nothing points at it");
    }

    /// The multi-hop case the whole ticket is about: a placeholder has no
    /// outgoing edges, so before linking a walk could never get past one hop.
    #[test]
    fn linked_imports_chain_so_a_transitive_walk_can_continue() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![file_node("a.ts"), file_node("b.ts"), file_node("c.ts")],
                ..Default::default()
            },
        )
        .unwrap();
        seed_resolved_import(&mut conn, "a.ts", "b.ts");
        seed_resolved_import(&mut conn, "b.ts", "c.ts");

        link_all(&mut conn).unwrap();

        let hops: Vec<String> = conn
            .prepare(
                "WITH RECURSIVE walk(id) AS (
                     SELECT 'file:a.ts'
                     UNION
                     SELECT e.toId FROM edges e JOIN walk ON e.fromId = walk.id WHERE e.kind = 'IMPORTS'
                 ) SELECT id FROM walk ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(hops, vec!["file:a.ts", "file:b.ts", "file:c.ts"]);
    }

    #[test]
    fn a_bare_specifier_placeholder_is_left_exactly_as_it_was() {
        let mut conn = setup();
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![file_node("src/index.ts")], ..Default::default() })
            .unwrap();
        let placeholder = placeholder_node("src/index.ts", "node:crypto", "external_module");
        let edge_id = import_edge("src/index.ts", &placeholder).id;
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![placeholder_node("src/index.ts", "node:crypto", "external_module")],
                upsert_edges: vec![import_edge(
                    "src/index.ts",
                    &placeholder_node("src/index.ts", "node:crypto", "external_module"),
                )],
                ..Default::default()
            },
        )
        .unwrap();

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary::default(), "an unmarked placeholder is not even a candidate");
        assert_eq!(edge_target(&conn, &edge_id), (placeholder.id.clone(), false));
        assert!(count(&conn, "nodes") == 2, "the placeholder node must survive");
    }

    /// A relative import whose target is not in the index - deleted,
    /// gitignored, or another language. It must degrade to the placeholder
    /// behaviour, not to a dangling edge.
    #[test]
    fn a_resolved_path_with_no_file_node_stays_a_placeholder() {
        let mut conn = setup();
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![file_node("src/index.ts")], ..Default::default() })
            .unwrap();
        seed_resolved_import(&mut conn, "src/index.ts", "src/generated/schema.ts");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary::default());
        let (to_id, resolved) =
            edge_target(&conn, "edge:src/index.ts:mod:src/index.ts:src/generated/schema.ts");
        assert_eq!(to_id, "mod:src/index.ts:src/generated/schema.ts");
        assert!(!resolved, "nothing was confirmed, so nothing may claim to be resolved");
    }

    #[test]
    fn linking_twice_changes_nothing_the_second_time() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff { upsert_nodes: vec![file_node("a.ts"), file_node("b.ts")], ..Default::default() },
        )
        .unwrap();
        seed_resolved_import(&mut conn, "a.ts", "b.ts");

        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
        assert_eq!(count(&conn, "edges"), 1);
    }

    /// The scoped pass must see the placeholders in the diff it is handed,
    /// without a whole-index scan.
    #[test]
    fn a_diff_links_the_imports_it_brought_with_it() {
        let mut conn = setup();
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![file_node("b.ts")], ..Default::default() }).unwrap();

        let placeholder = placeholder_node("a.ts", "b.ts", RESOLVED_MODULE_NATIVE_KIND);
        let diff = Diff {
            upsert_nodes: vec![
                file_node("a.ts"),
                placeholder_node("a.ts", "b.ts", RESOLVED_MODULE_NATIVE_KIND),
            ],
            upsert_edges: vec![import_edge("a.ts", &placeholder)],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();

        let summary = link_diff(&mut conn, &diff).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        assert_eq!(edge_target(&conn, &import_edge("a.ts", &placeholder).id).0, "file:b.ts");
    }

    /// The cross-file half: an importer indexed while its target did not
    /// exist yet gets linked when the target finally shows up, rather than
    /// waiting for the importer to be edited again.
    #[test]
    fn adding_a_file_links_the_placeholders_that_were_waiting_for_it() {
        let mut conn = setup();
        apply_diff(&mut conn, &Diff { upsert_nodes: vec![file_node("a.ts")], ..Default::default() }).unwrap();
        seed_resolved_import(&mut conn, "a.ts", "b.ts");
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default(), "b.ts does not exist yet");

        let diff = Diff { upsert_nodes: vec![file_node("b.ts")], ..Default::default() };
        apply_diff(&mut conn, &diff).unwrap();
        let summary = link_diff(&mut conn, &diff).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        assert_eq!(edge_target(&conn, "edge:a.ts:mod:a.ts:b.ts"), ("file:b.ts".to_string(), true));
    }

    /// Two files importing the same module have their own placeholder each
    /// (ids are per importing file), so linking one must not disturb the
    /// other's edge.
    #[test]
    fn each_importer_is_linked_independently() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![file_node("a.ts"), file_node("b.ts"), file_node("shared.ts")],
                ..Default::default()
            },
        )
        .unwrap();
        seed_resolved_import(&mut conn, "a.ts", "shared.ts");
        seed_resolved_import(&mut conn, "b.ts", "shared.ts");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 2, dropped_placeholders: 2 });
        let importers: Vec<String> = conn
            .prepare("SELECT fromId FROM edges WHERE toId = 'file:shared.ts' ORDER BY fromId")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(importers, vec!["file:a.ts", "file:b.ts"]);
    }

    /// A placeholder that is still the target of something else (nothing does
    /// this today, but the delete must not assume so) keeps its row.
    #[test]
    fn a_placeholder_with_other_edges_is_repointed_but_not_deleted() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff { upsert_nodes: vec![file_node("a.ts"), file_node("b.ts")], ..Default::default() },
        )
        .unwrap();
        seed_resolved_import(&mut conn, "a.ts", "b.ts");
        let placeholder_id = "mod:a.ts:b.ts";
        apply_diff(
            &mut conn,
            &Diff {
                upsert_edges: vec![EdgeRecord::new(
                    "e_ref",
                    "file:a.ts",
                    placeholder_id,
                    "REFERENCES",
                    "tree-sitter",
                    false,
                )],
                ..Default::default()
            },
        )
        .unwrap();

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1, dropped_placeholders: 0 });
        assert!(
            conn.query_row("SELECT COUNT(*) FROM nodes WHERE id = ?1", params![placeholder_id], |row| row
                .get::<_, i64>(0))
                .unwrap()
                == 1,
            "a placeholder something still points at must not be deleted out from under it"
        );
    }

    #[test]
    fn an_empty_diff_is_a_no_op() {
        let mut conn = setup();
        assert_eq!(link_diff(&mut conn, &Diff::default()).unwrap(), LinkSummary::default());
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary::default());
    }
}
