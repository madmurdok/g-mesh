//! Import linking: the pass that turns a plugin's *resolved* module
//! placeholder into a real edge onto whatever it names - a `File` node, or,
//! since GM-267, a core-owned logical container node (a Go package, a Rust
//! module, ... - docs/architecture/multi-language-plugins.md, "Data Model >
//! Logical containers").
//!
//! A language plugin emits one placeholder `Module` node per import specifier
//! and hangs the `IMPORTS` edge on it, because a specifier is text, not a
//! node id it can safely point at (see `recordImport` in
//! plugins/typescript/src/extract.ts). When the plugin recognises the specifier as
//! naming something in this project it says so, by setting the placeholder's
//! `nativeKind` to [`RESOLVED_MODULE_NATIVE_KIND`] and its `target` to a
//! structured address (`storage::write::PlaceholderTargetRecord`, the
//! storage mirror of `protocol::types::PlaceholderTarget` -
//! `scopeKind`/`scope`, "Data Model > Structured placeholder targets"):
//! `("file", "<path>")` for a TS-style specifier resolved to a file, or,
//! since GM-267, `("container", "<key>")` for a Go/Rust-style specifier
//! naming a whole package/module. This module is the other half of that
//! handshake: it looks the address up among what the index actually holds -
//! a `File` node at that path, or a `containers` row for that (language,
//! key) - repoints the edge, and drops the placeholder.
//!
//! ## Why here and not in the plugin
//!
//! Node ids are a pure function of the file path or the container key, so
//! the plugin (or, for a container, `graph::containers::container_id`) could
//! compute the target's id itself. What it cannot know is whether that node
//! *exists*:
//!
//!  - Walk order. During a cold-start bulk index the target file may not have
//!    been walked yet, or the target container may not have gained its first
//!    member yet. `daemon::bulk_index` may cut a batch anywhere in the
//!    stream and relies on a file's edges never leaving that file, which an
//!    edge emitted against a not-yet-committed node would break (edges are
//!    foreign keys onto nodes).
//!  - Index membership. A file can exist on disk and still never get a node -
//!    gitignored, under an excluded directory, or simply not this plugin's
//!    language. A container never gets a node until some file gives it a
//!    first member, which may never happen (a whole package excluded by
//!    `exclude_dirs`). Only core knows what the index actually holds.
//!
//! Doing the *path* arithmetic here instead would be worse in the other
//! direction: extension guessing, `index.*` directory imports and
//! TypeScript's `.js` -> `.ts` substitution are language rules, and core is
//! deliberately language-agnostic (see the Data Model section of
//! docs/architecture/g-mesh-v1.md). So the plugin decides *which address* a
//! specifier names and core decides *whether that address is a node* - neither
//! side needs the other's knowledge, and nothing here touches the filesystem,
//! which is what keeps this cheap enough to run inside the same transaction
//! path as every write.
//!
//! ## Reading the address: structured, not parsed
//!
//! Before GM-267 this module read a resolved placeholder's target by parsing
//! its `qualifiedName` (which held nothing but the target file's path) and
//! assumed `scope = file` unconditionally. It now reads
//! `storage::schema::placeholder_targets` instead - the row `apply_diff`
//! already writes for every placeholder carrying a `target`
//! (`storage::write::PlaceholderTargetRecord`) - and branches on
//! `scopeKind`. This is the storage layer GM-264 built and GM-267 is the
//! first reader of for `resolved_module`: `graph::symbol_links` (GM-266)
//! still parses `qualifiedName` for its own placeholder kinds until that task
//! lands, so the two linkers move onto the structured table on their own
//! schedules rather than together.
//!
//! The row's `keyKind`/`key` (which `graph::symbol_links` needs to pick one
//! declaration out of several visible under the same name) are never read
//! here: an import addresses a whole file or a whole container, and there is
//! only ever one node to repoint onto once the address resolves, so nothing
//! about *which* export a specifier names is this module's business. A v1
//! (TS) `resolved_module`'s legacy-derived target
//! (`protocol::types::derive_legacy_target`) always carries `keyKind: name,
//! key: "*"` (`graph::symbol_links::REEXPORT_ALL_NAME`, borrowed there as a
//! stand-in for "the whole module" - see that function's own comment) for
//! exactly this reason: the value is a placeholder the wire shape requires,
//! never one this module - or any module - consults.
//!
//! ## What stays unresolved
//!
//! Everything the plugin did not mark: bare/package specifiers
//! (`"react"`, `"node:crypto"`), and relative specifiers that resolve to
//! nothing on disk. Plus anything marked whose target is not in the index -
//! that placeholder is simply left alone, so a dangling import degrades to
//! exactly the pre-linking behaviour instead of an edge into nothing. A
//! container-scoped placeholder whose package has no members indexed at all
//! (never walked, or excluded) is the same case as a file that does not
//! exist: nothing to repoint onto, so nothing happens.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::storage::write::Diff;

/// The `nativeKind` a plugin marks a resolved import placeholder with.
/// Mirrors `RESOLVED_MODULE_NATIVE_KIND` in plugins/typescript/src/extract.ts -
/// the two are one wire contract and must be changed together.
pub const RESOLVED_MODULE_NATIVE_KIND: &str = "resolved_module";

const MODULE_KIND: &str = "Module";
const FILE_KIND: &str = "File";

/// [`storage::schema::placeholder_targets`]'s `scopeKind` values this module
/// switches on - see that table's own DDL comment.
const FILE_SCOPE: &str = "file";
const CONTAINER_SCOPE: &str = "container";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct LinkSummary {
    /// `IMPORTS` edges repointed from a placeholder onto a real `File` or
    /// container node.
    pub linked_edges: usize,
    /// Placeholders removed because nothing pointed at them any more.
    pub dropped_placeholders: usize,
}

/// One resolved placeholder: the node to eliminate, and the address
/// (`placeholder_targets.scopeKind`/`scope`) it claims to name. `language` is
/// the placeholder's own `nodes.language` - the *requester's* language, which
/// is also the target's for a container scope, since imports never cross a
/// language boundary (design doc: Non-goals - "Cross-language edges"). It is
/// unused for a `file` scope, where a path alone is already unambiguous.
struct Placeholder {
    id: String,
    scope_kind: String,
    scope: String,
    language: String,
}

/// Links every resolved placeholder in the index. The whole-project pass, run
/// once a bulk index has committed its last batch - at which point every
/// `File` node the walk will ever produce, and every container any language's
/// members will ever join, is in, so a placeholder that finds no target here
/// has no target at all.
pub fn link_all(conn: &mut Connection) -> Result<LinkSummary> {
    let placeholders = {
        let mut stmt = conn
            .prepare(
                "SELECT n.id, pt.scopeKind, pt.scope, n.language \
                 FROM nodes n JOIN placeholder_targets pt ON pt.nodeId = n.id \
                 WHERE n.kind = ?1 AND n.nativeKind = ?2",
            )
            .context("failed to prepare the placeholder scan")?;
        let rows = stmt
            .query_map(params![MODULE_KIND, RESOLVED_MODULE_NATIVE_KIND], |row| {
                Ok(Placeholder {
                    id: row.get(0)?,
                    scope_kind: row.get(1)?,
                    scope: row.get(2)?,
                    language: row.get(3)?,
                })
            })
            .context("failed to scan for resolved import placeholders")?;
        rows.collect::<rusqlite::Result<Vec<_>>>().context("failed to read resolved import placeholders")?
    };

    link(conn, placeholders)
}

/// Links what one just-applied diff could have changed, without rescanning
/// the whole index. Three things can newly become linkable:
///
///  - a placeholder the diff itself added (the reindexed file's own imports);
///  - a `File` node the diff added, which may be the target other files'
///    file-scoped placeholders have been waiting for - the cross-file case,
///    and the reason a newly created file does not stay invisible to its
///    importers until they happen to be edited;
///  - since GM-267, a container the diff's own member(s) just joined, which
///    may be the target other files' container-scoped placeholders have been
///    waiting for - the exact counterpart of the file case above, for a
///    package rather than a file. See the module doc's "container first
///    materializes" note below for why this is keyed off *joining a
///    container* rather than off "the container did not exist a moment ago":
///    the two triggers answer the same question either way, since a
///    placeholder that already linked no longer has a row to match again.
///
/// Not covered, deliberately: a file *deleted* from the project, or a
/// container's *last* member leaving (`graph::containers` GCs it). Either
/// node's outgoing... no, *incoming*: the edges pointing at it go when
/// something deletes the node; the importers' edges then come back as fresh
/// placeholders the next time those importers are reindexed, rather than
/// being re-placeholdered eagerly here - `graph::containers`' own module doc
/// documents the container half of this and confirms it is the same
/// deliberate non-eager behaviour this module already had for a deleted file.
pub fn link_diff(conn: &mut Connection, diff: &Diff) -> Result<LinkSummary> {
    let mut placeholders: Vec<Placeholder> = diff
        .upsert_nodes
        .iter()
        .filter(|node| {
            node.kind == MODULE_KIND && node.native_kind.as_deref() == Some(RESOLVED_MODULE_NATIVE_KIND)
        })
        .filter_map(|node| {
            let target = node.target.as_ref()?;
            Some(Placeholder {
                id: node.id.clone(),
                scope_kind: target.scope_kind.clone(),
                scope: target.scope.clone(),
                language: node.language.clone(),
            })
        })
        .collect();

    let added_files: Vec<&str> = diff
        .upsert_nodes
        .iter()
        .filter(|node| node.kind == FILE_KIND)
        .map(|node| node.file_path.as_str())
        .collect();
    if !added_files.is_empty() {
        // Indexed by idx_targets_scope (scopeKind, scope, key), so this is a
        // lookup per added file rather than a scan of the index.
        let mut stmt = conn
            .prepare(
                "SELECT n.id, n.language FROM placeholder_targets pt JOIN nodes n ON n.id = pt.nodeId \
                 WHERE pt.scopeKind = ?1 AND pt.scope = ?2 AND n.kind = ?3 AND n.nativeKind = ?4",
            )
            .context("failed to prepare the file-scoped placeholder lookup")?;
        for file_path in added_files {
            let rows = stmt
                .query_map(params![FILE_SCOPE, file_path, MODULE_KIND, RESOLVED_MODULE_NATIVE_KIND], |row| {
                    Ok(Placeholder {
                        id: row.get(0)?,
                        scope_kind: FILE_SCOPE.to_string(),
                        scope: file_path.to_string(),
                        language: row.get(1)?,
                    })
                })
                .context("failed to look up placeholders waiting on a new file")?;
            for row in rows {
                placeholders.push(row.context("failed to read a waiting placeholder")?);
            }
        }
    }

    // The container-materialization trigger (GM-267): every distinct
    // (language, container key) this diff's own upserted nodes joined -
    // whether that member is the container's first or its five-hundredth.
    // Re-checking an already-materialized container costs one indexed lookup
    // that (almost always) finds nothing new, the same shape the file trigger
    // above already accepts for "does this added file happen to be one
    // someone was waiting on" - and it is what lets this function learn a
    // container appeared without `apply_diff` reporting that fact back
    // explicitly (`graph::containers::attach`'s own `created` set is
    // private, and rightly so - nothing else needs to know it, and the query
    // below gives the same answer without that plumbing). A node whose
    // `container` field is set but which the diff also marks a placeholder
    // kind never happens in practice (no plugin sends `container` on a
    // placeholder), so it is not filtered out separately here; if it ever
    // did, the cost is one harmless extra lookup for a key nothing is really
    // waiting on.
    let mut container_keys: Vec<(String, String)> = diff
        .upsert_nodes
        .iter()
        .filter_map(|node| node.container.as_ref().map(|key| (node.language.clone(), key.clone())))
        .collect();
    container_keys.sort();
    container_keys.dedup();
    if !container_keys.is_empty() {
        let mut stmt = conn
            .prepare(
                "SELECT n.id FROM placeholder_targets pt JOIN nodes n ON n.id = pt.nodeId \
                 WHERE pt.scopeKind = ?1 AND pt.scope = ?2 AND n.language = ?3 AND n.kind = ?4 \
                   AND n.nativeKind = ?5",
            )
            .context("failed to prepare the container-scoped placeholder lookup")?;
        for (language, key) in container_keys {
            let rows = stmt
                .query_map(
                    params![CONTAINER_SCOPE, &key, &language, MODULE_KIND, RESOLVED_MODULE_NATIVE_KIND],
                    |row| {
                        Ok(Placeholder {
                            id: row.get(0)?,
                            scope_kind: CONTAINER_SCOPE.to_string(),
                            scope: key.clone(),
                            language: language.clone(),
                        })
                    },
                )
                .context("failed to look up placeholders waiting on a container")?;
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
        // The `containers` table, not a computed `container_id` existence
        // check: it is the authoritative registry of which containers are
        // actually materialized right now (`graph::containers`'s own doc -
        // a container with zero members is GCed, row and node together), so
        // a lookup against it can never find a dangling id a deleted
        // container might otherwise leave behind.
        let mut find_container = tx
            .prepare("SELECT nodeId FROM containers WHERE language = ?1 AND key = ?2")
            .context("failed to prepare the target-container lookup")?;
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
            let target: Option<String> = if placeholder.scope_kind == CONTAINER_SCOPE {
                find_container
                    .query_row(params![placeholder.language, placeholder.scope], |row| row.get(0))
                    .optional()
                    .context("failed to look up an import target container")?
            } else {
                // `file`, and, defensively, anything else: `scopeKind`'s own
                // CHECK only allows the two values, so this arm is really
                // just `file`, but resolving unrecognized input the same way
                // an unresolved path already does (leave the placeholder
                // alone) is cheaper than a third branch that can never run.
                find_file
                    .query_row(params![FILE_KIND, placeholder.scope], |row| row.get(0))
                    .optional()
                    .context("failed to look up an import target file")?
            };
            // No node for that address: not indexed (gitignored, excluded,
            // another language) or not there at all, or, for a container, no
            // member has ever joined it. Leaving the placeholder exactly as
            // it is *is* the graceful fallback.
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
    use crate::storage::write::{apply_diff, EdgeRecord, NodeRecord, PlaceholderTargetRecord};

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

    /// The wire-derived shape of a `file`-scoped resolved-module target - what
    /// `protocol::types::derive_legacy_target` builds for a v1 (TS) sender,
    /// and what a v2 sender addressing a single file sends directly. The key
    /// is the `REEXPORT_ALL_NAME` ("*") stand-in for "the whole module" -
    /// present because the wire shape requires one, never read by this
    /// module (see the module doc's "Reading the address" section).
    fn file_target(path: &str) -> PlaceholderTargetRecord {
        PlaceholderTargetRecord {
            scope_kind: FILE_SCOPE.to_string(),
            scope: path.to_string(),
            key_kind: "name".to_string(),
            key: crate::graph::symbol_links::REEXPORT_ALL_NAME.to_string(),
            from_container: None,
        }
    }

    /// The GM-267 counterpart of [`file_target`]: a `container`-scoped
    /// resolved-module target, e.g. Go's `import "github.com/x/pkg"`.
    fn container_target(key: &str) -> PlaceholderTargetRecord {
        PlaceholderTargetRecord {
            scope_kind: CONTAINER_SCOPE.to_string(),
            scope: key.to_string(),
            key_kind: "name".to_string(),
            key: crate::graph::symbol_links::REEXPORT_ALL_NAME.to_string(),
            from_container: None,
        }
    }

    /// The plugin's own shape for an import placeholder: `filePath` is the
    /// *importing* file (that is where the statement is written), the
    /// `qualifiedName`/`name` are whatever text the plugin chooses to show a
    /// human (no longer what this module reads - see the module doc), and
    /// `target` is the structured address this module actually links against.
    fn placeholder_node(
        importer: &str,
        display: &str,
        native_kind: &str,
        target: Option<PlaceholderTargetRecord>,
        language: &str,
    ) -> NodeRecord {
        let mut node = NodeRecord::new(
            format!("mod:{importer}:{display}"),
            MODULE_KIND,
            display,
            display,
            importer,
            language,
        );
        node.native_kind = Some(native_kind.to_string());
        node.target = target;
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

    /// `importer` imports `specifier`, resolved by the plugin to `target`
    /// (`file`-scoped, TS-style).
    fn seed_resolved_import(conn: &mut Connection, importer: &str, target: &str) {
        let placeholder = placeholder_node(
            importer,
            target,
            RESOLVED_MODULE_NATIVE_KIND,
            Some(file_target(target)),
            "typescript",
        );
        let edge = import_edge(importer, &placeholder);
        apply_diff(
            conn,
            &Diff { upsert_nodes: vec![placeholder], upsert_edges: vec![edge], ..Default::default() },
        )
        .unwrap();
    }

    /// `importer` (in `language`) imports the container `key` as a whole -
    /// Go's `import "github.com/x/pkg"`. Returns the placeholder's own edge
    /// id, since the container tests need it to check `edge_target`.
    fn seed_container_import(conn: &mut Connection, importer: &str, language: &str, key: &str) -> String {
        let file =
            NodeRecord::new(format!("file:{importer}"), FILE_KIND, importer, importer, importer, language);
        let placeholder = placeholder_node(
            importer,
            key,
            RESOLVED_MODULE_NATIVE_KIND,
            Some(container_target(key)),
            language,
        );
        let edge = import_edge(importer, &placeholder);
        let edge_id = edge.id.clone();
        apply_diff(
            conn,
            &Diff { upsert_nodes: vec![file, placeholder], upsert_edges: vec![edge], ..Default::default() },
        )
        .unwrap();
        edge_id
    }

    /// A member joining container `key` in `language` - the minimal fixture
    /// `graph::containers::attach` needs to materialize (or keep alive) the
    /// container node this module links onto. Mirrors `containers::tests::
    /// member`, kept local rather than shared because that helper is
    /// `#[cfg(test)]`-private to its own module.
    fn container_member(id: &str, language: &str, key: &str) -> NodeRecord {
        let mut node = NodeRecord::new(id, "Function", id, id, format!("{key}/{id}.x"), language);
        node.container = Some(key.to_string());
        node
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

    fn node_exists(conn: &Connection, id: &str) -> bool {
        count_where(conn, "nodes", "id", id) == 1
    }

    fn edge_exists(conn: &Connection, id: &str) -> bool {
        count_where(conn, "edges", "id", id) == 1
    }

    fn count_where(conn: &Connection, table: &str, column: &str, value: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"), params![value], |row| {
            row.get(0)
        })
        .unwrap()
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
        let placeholder =
            placeholder_node("src/index.ts", "node:crypto", "external_module", None, "typescript");
        let edge_id = import_edge("src/index.ts", &placeholder).id;
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![placeholder_node(
                    "src/index.ts",
                    "node:crypto",
                    "external_module",
                    None,
                    "typescript",
                )],
                upsert_edges: vec![import_edge(
                    "src/index.ts",
                    &placeholder_node("src/index.ts", "node:crypto", "external_module", None, "typescript"),
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

        let placeholder = placeholder_node(
            "a.ts",
            "b.ts",
            RESOLVED_MODULE_NATIVE_KIND,
            Some(file_target("b.ts")),
            "typescript",
        );
        let diff = Diff {
            upsert_nodes: vec![
                file_node("a.ts"),
                placeholder_node(
                    "a.ts",
                    "b.ts",
                    RESOLVED_MODULE_NATIVE_KIND,
                    Some(file_target("b.ts")),
                    "typescript",
                ),
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

    // -----------------------------------------------------------------
    // Container-scoped imports (GM-267).
    // -----------------------------------------------------------------

    /// The acceptance criterion, ordinary order: the container already has a
    /// member (so it exists) before the importer is indexed - the same shape
    /// `a_resolved_placeholder_becomes_an_edge_between_the_two_files` proves
    /// for a file target.
    #[test]
    fn a_container_scoped_placeholder_links_onto_the_container_node() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![container_member("a", "go", "github.com/x/pkg")],
                ..Default::default()
            },
        )
        .unwrap();

        let edge_id = seed_container_import(&mut conn, "main.go", "go", "github.com/x/pkg");
        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        let (to_id, resolved) = edge_target(&conn, &edge_id);
        assert_eq!(
            to_id,
            crate::graph::containers::container_id("go", "github.com/x/pkg"),
            "the edge must point at the container's own node"
        );
        assert!(resolved);
    }

    /// The acceptance criterion's harder order, and the whole point of the
    /// container-materialization trigger: the importer is indexed *first*,
    /// while the package has no member at all, and only later does a member
    /// arrive and materialize the container - the direct container
    /// counterpart of `adding_a_file_links_the_placeholders_that_were_
    /// waiting_for_it`.
    #[test]
    fn a_container_import_links_once_a_member_arrives_after_the_importer() {
        let mut conn = setup();
        let edge_id = seed_container_import(&mut conn, "main.go", "go", "github.com/x/pkg");
        assert_eq!(
            link_all(&mut conn).unwrap(),
            LinkSummary::default(),
            "github.com/x/pkg has no member yet, so there is nothing to link onto"
        );

        let diff = Diff {
            upsert_nodes: vec![container_member("a", "go", "github.com/x/pkg")],
            ..Default::default()
        };
        apply_diff(&mut conn, &diff).unwrap();
        let summary = link_diff(&mut conn, &diff).unwrap();

        assert_eq!(
            summary,
            LinkSummary { linked_edges: 1, dropped_placeholders: 1 },
            "the member's own diff must trigger the link, with no reindex of main.go"
        );
        let (to_id, resolved) = edge_target(&conn, &edge_id);
        assert_eq!(to_id, crate::graph::containers::container_id("go", "github.com/x/pkg"));
        assert!(resolved);
    }

    /// GM-265's documented lifecycle, the realistic sequence: a container is
    /// GCed when its last member leaves (dropping the importer's edge with
    /// it, per `graph::containers`' own module doc - "the same non-eager
    /// behaviour `graph::imports` documents for a deleted target file"), and
    /// only comes back once both a member re-appears *and* the importer is
    /// reindexed (re-sending a fresh placeholder - GC does not resurrect the
    /// importer's edge on its own). This is the scenario GM-265 promised and
    /// GM-267 is the first to actually exercise end to end.
    #[test]
    fn a_container_gced_and_later_rematerialized_relinks_once_the_importer_is_reindexed() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff { upsert_nodes: vec![container_member("a", "go", "pkg")], ..Default::default() },
        )
        .unwrap();
        let edge_id = seed_container_import(&mut conn, "main.go", "go", "pkg");
        assert_eq!(link_all(&mut conn).unwrap(), LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        let container_node = crate::graph::containers::container_id("go", "pkg");
        assert_eq!(edge_target(&conn, &edge_id).0, container_node, "linked onto the container first");

        // The last member leaves: `apply_diff` GCs the container and, with
        // it (per `graph::containers`), main.go's now-dangling IMPORTS edge.
        apply_diff(&mut conn, &Diff { delete_node_ids: vec!["a".to_string()], ..Default::default() })
            .unwrap();
        assert!(
            !edge_exists(&conn, &edge_id),
            "the importer's edge must be gone with the container, not dangling"
        );
        assert!(!node_exists(&conn, &container_node));

        // A member re-appears (a fresh `pkg`, possibly with new content) -
        // `container_id` is a pure function of (language, key), so this is
        // the exact same node id coming back, but nothing importer-side
        // knows that yet: main.go's own placeholder is gone (it was linked
        // away, and linking is not "keep a spare in case the target dies").
        let refill = Diff { upsert_nodes: vec![container_member("b", "go", "pkg")], ..Default::default() };
        apply_diff(&mut conn, &refill).unwrap();
        assert_eq!(
            link_diff(&mut conn, &refill).unwrap(),
            LinkSummary::default(),
            "the container is back, but nothing is waiting on it until main.go is reindexed"
        );

        // main.go is reindexed: its plugin re-sends a fresh placeholder
        // (deterministically the same ids, since nothing about the file's
        // own content changed) plus a fresh IMPORTS edge - exactly
        // `graph::containers`' documented "re-placeholdered on reindex".
        let reindex_edge_id = seed_container_import(&mut conn, "main.go", "go", "pkg");
        assert_eq!(reindex_edge_id, edge_id, "a deterministic re-extraction reuses the same edge id");
        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 1, dropped_placeholders: 1 });
        assert_eq!(edge_target(&conn, &edge_id), (container_node, true));
    }

    /// Decision 6: two containers reached from the same file must not
    /// collapse into one row anywhere in this module's own bookkeeping -
    /// both `filePath = ''` (`graph::containers::ensure_container`), so any
    /// dedup keyed by that column instead of node id would merge them. This
    /// module never groups by `filePath` at all, but the fixture is shared
    /// with `mcp::get_dependencies`'s own version of this test, which is
    /// where the risk (`ReachedNode`/`DependencyNode`) actually lives.
    #[test]
    fn two_containers_imported_by_the_same_file_both_link_independently() {
        let mut conn = setup();
        apply_diff(
            &mut conn,
            &Diff {
                upsert_nodes: vec![
                    container_member("a", "go", "pkg/a"),
                    container_member("b", "go", "pkg/b"),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let edge_a = seed_container_import(&mut conn, "main.go", "go", "pkg/a");
        let edge_b = seed_container_import(&mut conn, "main.go", "go", "pkg/b");

        let summary = link_all(&mut conn).unwrap();

        assert_eq!(summary, LinkSummary { linked_edges: 2, dropped_placeholders: 2 });
        assert_eq!(edge_target(&conn, &edge_a).0, crate::graph::containers::container_id("go", "pkg/a"));
        assert_eq!(edge_target(&conn, &edge_b).0, crate::graph::containers::container_id("go", "pkg/b"));
        assert_ne!(edge_target(&conn, &edge_a).0, edge_target(&conn, &edge_b).0);
    }
}
